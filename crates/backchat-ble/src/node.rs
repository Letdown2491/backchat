//! Tokio glue: owns the persisted [`MeshIdentity`], the [`Engine`], and the
//! link task; exposes a channel-based handle for the UI (works from any
//! executor — including GPUI's — because it's just mpsc).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use backchat_core::identity::{MeshIdentity, IDENTITY_BLOB_LEN};
use tokio::sync::mpsc;

use crate::engine::{Action, Engine, MeshEvent};
use crate::{link, MeshConfig};

/// Commands the UI can send.
#[derive(Debug, Clone)]
pub enum Command {
    SetNickname(String),
    SendPublic(String),
    SendPrivate { peer: [u8; 8], content: String },
    /// Send an announce immediately (used on manual refresh).
    AnnounceNow,
    /// Flood a leave packet and stop the node.
    Shutdown,
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("link: {0}")]
    Link(#[from] link::LinkError),
    #[error("identity storage: {0}")]
    Storage(String),
}

/// The UI-facing end of a running mesh node.
pub struct NodeHandle {
    /// Mesh events for the UI (peer updates, messages).
    pub events: mpsc::Receiver<MeshEvent>,
    /// UI → node commands.
    pub cmd_tx: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl NodeHandle {
    pub async fn send(&self, command: Command) {
        let _ = self.cmd_tx.send(command).await;
    }

    pub fn abort(&self) {
        self.task.abort();
    }
}

fn identity_path() -> Result<PathBuf, NodeError> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| NodeError::Storage("HOME not set".into()))?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("backchat/mesh_identity.bin"))
}

use std::path::PathBuf;

fn load_or_create_identity() -> Result<MeshIdentity, NodeError> {
    let path = identity_path()?;
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == IDENTITY_BLOB_LEN {
            let mut blob = [0u8; IDENTITY_BLOB_LEN];
            blob.copy_from_slice(&bytes);
            if let Some(identity) = MeshIdentity::from_bytes(&blob) {
                return Ok(identity);
            }
        }
    }
    let identity = MeshIdentity::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| NodeError::Storage(format!("{}: {e}", parent.display())))?;
    }
    std::fs::write(&path, identity.to_bytes())
        .map_err(|e| NodeError::Storage(format!("{}: {e}", path.display())))?;
    Ok(identity)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Execute one engine action; returns true when the node should stop.
async fn dispatch(
    action: Action,
    outbound_tx: &mpsc::Sender<Vec<u8>>,
    events_tx: &mpsc::Sender<MeshEvent>,
) -> bool {
    match action {
        // v1 link layer has no per-peer link table; unicast frames are
        // flooded like broadcasts and routed by the recipients themselves.
        Action::Broadcast(bytes) | Action::SendTo(_, bytes) => {
            outbound_tx.send(bytes).await.is_err()
        }
        Action::Event(event) => events_tx.send(event).await.is_err(),
    }
}

/// Spawn the mesh node. It runs until [`NodeHandle::abort`] or the handle is
/// dropped (the spawned task is tied to the channels).
pub async fn spawn(config: MeshConfig) -> Result<NodeHandle, NodeError> {
    let identity = load_or_create_identity()?;
    // The engine task moves `config`; the link task needs its own copy.
    let link_config = config.clone();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(64);
    let (events_tx, events_rx) = mpsc::channel::<MeshEvent>(256);

    let task = tokio::spawn(async move {
        let mut engine = Engine::new(identity, config.nickname.clone());
        let mut announce =
            tokio::time::interval(Duration::from_millis(config.announce_interval_ms));
        announce.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut tick = tokio::time::interval(Duration::from_secs(30));

        let mut last_inbound_ms = now_ms();
        let mut link = match link::spawn(&link_config, false).await {
            Ok(link) => link,
            Err(e) => {
                eprintln!("node: link failed to start: {e}");
                return;
            }
        };

        if dispatch(engine.announce_action(now_ms()), &link.outbound_tx, &events_tx).await {
            return;
        }

        loop {
            // Heal check: the link has been reporting hard connect failures
            // and nothing has arrived inbound for a long while — the adapter
            // is wedged. Rebuild the radio around a power cycle; the engine
            // (identity, sessions, registry) survives untouched.
            {
                let now = now_ms();
                let wedged = link.wedge_failures.load(std::sync::atomic::Ordering::Relaxed);
                let silent_for = now.saturating_sub(last_inbound_ms);
                if wedged >= 2 && silent_for > 120_000 {
                    eprintln!(
                        "node: adapter wedged ({wedged} hard failures, no inbound for {:.0}s) — restarting the link",
                        silent_for / 1000
                    );
                    link.task.abort();
                    link.discovery.abort();
                    link = match link::spawn(&link_config, true).await {
                        Ok(link) => {
                            last_inbound_ms = now_ms();
                            let _ = events_tx
                                .send(MeshEvent::RadioRestarted { heal: true })
                                .await;
                            link
                        }
                        Err(e) => {
                            eprintln!("node: link restart failed: {e}");
                            return;
                        }
                    };
                }
            }

            tokio::select! {
                raw = link.inbound_rx.recv() => {
                    let Some(raw) = raw else {
                        // Link task ended (adapter reset, fatal bluer error):
                        // rebuild the radio instead of taking the node down.
                        eprintln!("node: link ended — restarting");
                        link = match link::spawn(&link_config, false).await {
                            Ok(link) => {
                                last_inbound_ms = now_ms();
                                let _ = events_tx
                                    .send(MeshEvent::RadioRestarted { heal: false })
                                    .await;
                                link
                            }
                            Err(e) => {
                                eprintln!("node: link restart failed: {e}");
                                return;
                            }
                        };
                        continue;
                    };
                    last_inbound_ms = now_ms();
                    let mut actions = engine.on_bytes(&raw, now_ms());
                    actions.extend(engine.tick(now_ms()));
                    for action in actions {
                        if dispatch(action, &link.outbound_tx, &events_tx).await {
                            return;
                        }
                    }
                }
                _ = announce.tick() => {
                    if dispatch(engine.announce_action(now_ms()), &link.outbound_tx, &events_tx).await {
                        return;
                    }
                }
                _ = tick.tick() => {
                    for action in engine.tick(now_ms()) {
                        if dispatch(action, &link.outbound_tx, &events_tx).await {
                            return;
                        }
                    }
                }
                command = cmd_rx.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        Command::SetNickname(nickname) => {
                            engine.set_nickname(nickname.clone());
                            // Re-announce under the new name.
                            if dispatch(engine.announce_action(now_ms()), &link.outbound_tx, &events_tx).await {
                                return;
                            }
                        }
                        Command::SendPublic(content) => {
                            for action in engine.send_public(&content, now_ms()) {
                                if dispatch(action, &link.outbound_tx, &events_tx).await {
                                    return;
                                }
                            }
                        }
                        Command::SendPrivate { peer, content } => {
                            for action in engine.send_private(peer, &content, now_ms()) {
                                if dispatch(action, &link.outbound_tx, &events_tx).await {
                                    return;
                                }
                            }
                        }
                        Command::AnnounceNow => {
                            if dispatch(engine.announce_action(now_ms()), &link.outbound_tx, &events_tx).await {
                                return;
                            }
                        }
                        Command::Shutdown => {
                            let leave = engine.leave_action(now_ms());
                            if let Action::Broadcast(bytes) = leave {
                                let _ = link.outbound_tx.send(bytes).await;
                            }
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            break;
                        }
                    }
                }
            }
        }
        link.task.abort();
        link.discovery.abort();
    });

    Ok(NodeHandle { events: events_rx, cmd_tx, task })
}
