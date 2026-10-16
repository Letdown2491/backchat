//! Minimal relay client: websocket sessions to a set of relays, NIP-01
//! REQ/EVENT/CLOSE handling, reconnect with backoff. The seam the GPUI app
//! talks to. Port of the transport-relevant half of
//! `NostrRelayManager.swift` (subscription shape and lookback windows).

use crate::event::NostrEvent;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// Subscription id (per-relay; we generate globally unique ones).
    pub id: String,
    /// NIP-01 filter as JSON (already shaped by the caller).
    pub filter: serde_json::Value,
}

impl Subscription {
    pub fn new(filter: serde_json::Value) -> Self {
        Self { id: unique_sub_id(), filter }
    }
}

#[derive(Debug, Clone)]
pub enum RelayCommand {
    /// Replace the subscription set on all relays.
    Subscribe(Vec<Subscription>),
    /// Publish a signed event to every connected relay.
    Publish(NostrEvent),
    /// Tear down current sessions and connect to a new relay set (used
    /// when the joined channel's nearest relays change).
    SetRelays(Vec<String>),
    /// Internal: close this session (the manager aborts on SetRelays too;
    /// this is the cooperative path).
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum RelayEvent {
    /// An EVENT received (relay url, event).
    Received { relay: String, event: NostrEvent },
    /// Connectivity changes (a session connected or dropped); delta-summed
    /// for the status banner.
    Status { delta: i32 },
}

static SUB_COUNTER: AtomicU64 = AtomicU64::new(1);

fn unique_sub_id() -> String {
    format!("backchat-{}", SUB_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// NIP-01 filter builder for geohash channels: ephemeral messages + presence
/// tagged `#g`, with a lookback window.
pub fn geohash_filter(geohash: &str, since_secs_ago: i64) -> serde_json::Value {
    let since = now_secs() - since_secs_ago;
    serde_json::json!({
        "kinds": [20000, 20001],
        "#g": [geohash],
        "since": since,
    })
}

pub fn bridge_filter(cell: &str, since_secs_ago: i64) -> serde_json::Value {
    let since = now_secs() - since_secs_ago;
    serde_json::json!({
        "kinds": [20000, 20001],
        "#r": [cell],
        "since": since,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Spawn the relay session task. Returns the command channel (what the app
/// sends) and the event channel (what the app receives). The task runs for
/// the lifetime of the process; drop the handles to shut it down.
pub fn spawn_relay_manager(
    relays: Vec<String>,
) -> (mpsc::UnboundedSender<RelayCommand>, mpsc::UnboundedReceiver<RelayEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<RelayCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<RelayEvent>();
    tokio::spawn(async move {
        run_manager(relays, cmd_rx, evt_tx).await;
    });
    (cmd_tx, evt_rx)
}

async fn run_manager(
    relays: Vec<String>,
    mut cmd_rx: mpsc::UnboundedReceiver<RelayCommand>,
    evt_tx: mpsc::UnboundedSender<RelayEvent>,
) {
    // One command channel per relay session; the loop below fans commands
    // out to all of them.
    let mut sessions: Vec<(mpsc::UnboundedSender<RelayCommand>, tokio::task::JoinHandle<()>)> =
        Vec::new();
    for relay in relays {
        spawn_session(&mut sessions, relay, evt_tx.clone());
    }

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            RelayCommand::SetRelays(urls) => {
                for (tx, handle) in sessions.drain(..) {
                    let _ = tx.send(RelayCommand::Shutdown);
                    handle.abort();
                }
                for url in urls {
                    spawn_session(&mut sessions, url, evt_tx.clone());
                }
            }
            other => {
                for (tx, _) in &sessions {
                    let _ = tx.send(other.clone());
                }
            }
        }
    }
}

fn spawn_session(
    sessions: &mut Vec<(mpsc::UnboundedSender<RelayCommand>, tokio::task::JoinHandle<()>)>,
    relay: String,
    evt_tx: mpsc::UnboundedSender<RelayEvent>,
) {
    let (relay_cmd_tx, relay_cmd_rx) = mpsc::unbounded_channel::<RelayCommand>();
    let handle = tokio::spawn(async move {
        relay_session(relay, relay_cmd_rx, evt_tx).await;
    });
    sessions.push((relay_cmd_tx, handle));
}

async fn relay_session(
    url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<RelayCommand>,
    evt_tx: mpsc::UnboundedSender<RelayEvent>,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    // Snapshot of the current subscription set, re-sent on every
    // (re)connect so a dropped session resumes its REQs.
    let mut current_subs: Vec<Subscription> = Vec::new();
    'session: loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                eprintln!("[relay] connected: {url}");
                let _ = evt_tx.send(RelayEvent::Status { delta: 1 });
                backoff = std::time::Duration::from_secs(1);
                let (mut write, mut read) = ws.split();

                // Adopt the current subscription set.
                for sub in &current_subs {
                    let req = serde_json::json!(["REQ", sub.id, sub.filter]);
                    if write.send(WsMessage::Text(req.to_string().into())).await.is_err() {
                        break;
                    }
                }

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(WsMessage::Text(text))) => {
                                    if let Some(event) = parse_event_message(&text) {
                                        let _ = evt_tx.send(RelayEvent::Received {
                                            relay: url.clone(),
                                            event,
                                        });
                                    }
                                }
                                _ => break, // closed or errored
                            }
                        }
                        cmd = cmd_rx.recv() => {
                            match cmd {
                                Some(RelayCommand::Shutdown) => return,
                                Some(RelayCommand::Subscribe(subs)) => {
                                    current_subs = subs;
                                    for sub in &current_subs {
                                        let req = serde_json::json!(["REQ", sub.id, sub.filter]);
                                        if write.send(WsMessage::Text(req.to_string().into())).await.is_err() {
                                            break 'session;
                                        }
                                    }
                                }
                                Some(RelayCommand::SetRelays(_)) => {
                                    // Managed by the manager, not sessions.
                                }
                                Some(RelayCommand::Publish(event)) => {
                                    let msg = serde_json::json!(["EVENT", event]);
                                    if write.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                                        break 'session;
                                    }
                                }
                                None => return, // manager gone: shut down
                            }
                        }
                    }
                }
            }
            Err(e) => eprintln!("[relay] connect failed: {url}: {e}"),
        }
        let _ = evt_tx.send(RelayEvent::Status { delta: -1 });
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
    }
}

/// Parse a NIP-01 relay message; returns the event from ["EVENT", sub, ev].
fn parse_event_message(text: &str) -> Option<NostrEvent> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = parsed.as_array()?;
    if arr.first()?.as_str()? != "EVENT" {
        return None;
    }
    serde_json::from_value(arr.get(2)?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_messages() {
        let msg = serde_json::json!([
            "EVENT",
            "sub-1",
            {
                "id": "a".repeat(64),
                "pubkey": "b".repeat(64),
                "created_at": 1700000000i64,
                "kind": 20000,
                "tags": [["g", "dr5rs"]],
                "content": "hi",
                "sig": "c".repeat(128),
            }
        ])
        .to_string();
        let event = parse_event_message(&msg).expect("parses");
        assert_eq!(event.kind, 20000);
        assert_eq!(event.tags, [["g", "dr5rs"]]);

        // Non-EVENT messages (NOTICE, OK, EOSE) are ignored.
        assert!(parse_event_message(r#"["NOTICE","hi"]"#).is_none());
        assert!(parse_event_message(r#"["OK","id",true,""]"#).is_none());
    }

    #[test]
    fn geohash_filter_shape() {
        let filter = geohash_filter("dr5rs", 3600);
        assert_eq!(filter["kinds"], serde_json::json!([20000, 20001]));
        assert_eq!(filter["#g"], serde_json::json!(["dr5rs"]));
        assert!(filter["since"].is_i64());
    }

    #[test]
    fn subscription_ids_are_unique() {
        let a = Subscription::new(serde_json::json!({}));
        let b = Subscription::new(serde_json::json!({}));
        assert_ne!(a.id, b.id);
    }
}

/// Publish helper usable from tests and the app: opens a short-lived
/// connection, sends the EVENT, waits for OK, closes.
pub async fn publish_once(relay_url: &str, event: &NostrEvent) -> Result<(), String> {
    let (ws, _) = tokio_tungstenite::connect_async(relay_url)
        .await
        .map_err(|e| format!("connect {relay_url}: {e}"))?;
    let (mut write, mut read) = ws.split();
    let msg = serde_json::json!(["EVENT", event]);
    write
        .send(WsMessage::Text(msg.to_string().into()))
        .await
        .map_err(|e| format!("send: {e}"))?;
    // Wait for the OK frame (bounded).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let next = tokio::time::timeout_at(deadline, read.next()).await;
        match next {
            Err(_) => return Err(format!("{relay_url}: OK timeout")),
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                if let Some(arr) = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                {
                    if arr.first().and_then(|v| v.as_str()) == Some("OK") {
                        return match arr.get(2).and_then(|v| v.as_bool()) {
                            Some(true) => Ok(()),
                            _ => Err(format!("{relay_url}: rejected: {text}")),
                        };
                    }
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => return Err(format!("{relay_url}: {e}")),
            Ok(None) => return Err(format!("{relay_url}: closed before OK")),
        }
    }
}

/// Distinct relays helper (input hygiene).
pub fn dedupe(relays: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    relays
        .iter()
        .filter(|r| seen.insert(r.as_str()))
        .cloned()
        .collect()
}
