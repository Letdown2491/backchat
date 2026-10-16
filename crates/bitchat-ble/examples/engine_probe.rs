//! Engine-level probe: runs the real engine over the real radio and logs
//! exactly WHY announces are accepted or rejected (peer-id derivation,
//! signature verification), plus every event the UI would see.

use bitchat_ble::engine::{Action, Engine, MeshEvent};
use bitchat_ble::link;
use bitchat_core::identity::MeshIdentity;
use bitchat_core::packet::Packet;
use bitchat_core::peer_id::PeerId;
use bitchat_core::tlv::AnnouncementPacket;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let identity = load_or_create();
    let mut engine = Engine::new(identity.clone(), "engine-probe".into());
    eprintln!(
        "probe: my peer id = {} (announce nickname: engine-probe)",
        identity.peer_id()
    );

    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(256);
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::channel(256);
    let config = bitchat_ble::MeshConfig {
        nickname: "engine-probe".into(),
        ..Default::default()
    };
    let _link = link::spawn(&config, inbound_tx, outbound_rx).await?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Kick off with an announce.
    if let Action::Broadcast(bytes) = engine.announce_action(now) {
        let _ = outbound_tx.send(bytes).await;
    }

    loop {
        let Some(raw) = inbound_rx.recv().await else { return Ok(()) };
        let packet = match Packet::decode(&raw) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "probe: undecodable frame ({} bytes): {e} — hex: {}",
                    raw.len(),
                    hex::encode(&raw)
                );
                continue;
            }
        };
        eprintln!(
            "probe: kind=0x{:02x} len={} sender={} recipient={} hex={}",
            packet.kind,
            raw.len(),
            PeerId::from_mesh_bytes(&packet.sender_id).full(),
            packet
                .recipient_id
                .as_ref()
                .map(|r| PeerId::from_mesh_bytes(r).full())
                .unwrap_or_else(|| "-".into()),
            hex::encode(&raw)
        );
        if packet.kind == 0x01 {
            let sender = PeerId::from_mesh_bytes(&packet.sender_id);
            match AnnouncementPacket::decode(&packet.payload) {
                Some(announce) => {
                    let derived = PeerId::from_noise_static_public_key(&announce.noise_public_key);
                    eprintln!(
                        "probe: ANNOUNCE from claimed={} nickname={:?} derived_from_noise_key={} match={} sig_present={}",
                        sender.full(),
                        announce.nickname,
                        derived.full(),
                        derived.mesh_bytes() == Some(packet.sender_id),
                        packet.signature.is_some()
                    );
                    if let Some(sig) = packet.signature {
                        let signable = packet.to_signing_bytes()?;
                        let ok = MeshIdentity::verify(&announce.signing_public_key, &sig, &signable);
                        eprintln!("probe:   signature over signing-bytes ({} B): {}", signable.len(), if ok { "VALID" } else { "INVALID" });
                        if !ok {
                            eprintln!("probe:   signing-bytes hex: {}", hex::encode(&signable));
                            eprintln!("probe:   sig: {}", hex::encode(sig));
                            eprintln!("probe:   announced signing key: {}", hex::encode(announce.signing_public_key));
                        }
                    }
                }
                None => eprintln!("probe: ANNOUNCE from {} — TLV DECODE FAILED", sender.full()),
            }
        }
        for action in engine.on_bytes(&raw, now) {
            match action {
                Action::Broadcast(_) | Action::SendTo(_, _) => {}
                Action::Event(event) => match event {
                    MeshEvent::PeerSeen { nickname, is_new, .. } => {
                        eprintln!("probe: EVENT PeerSeen {nickname} (new: {is_new})");
                    }
                    other => eprintln!("probe: EVENT {other:?}"),
                },
            }
        }
    }
}

fn load_or_create() -> MeshIdentity {
    use bitchat_core::identity::{MeshIdentity, IDENTITY_BLOB_LEN};
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".local/share")
    });
    let path = base.join("bitchat-rs/mesh_identity.bin");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == IDENTITY_BLOB_LEN {
            let mut blob = [0u8; IDENTITY_BLOB_LEN];
            blob.copy_from_slice(&bytes);
            if let Some(id) = MeshIdentity::from_bytes(&blob) {
                return id;
            }
        }
    }
    let id = MeshIdentity::generate();
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    std::fs::write(&path, id.to_bytes()).expect("write");
    id
}
use std::path::PathBuf;
