//! Runs the link layer alone (no app, no engine) and prints raw inbound
//! traffic + link logs for manual radio debugging. Exit with Ctrl-C.

use bitchat_ble::MeshConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = MeshConfig {
        nickname: std::env::args()
            .nth(1)
            .unwrap_or_else(|| "probe".into()),
        ..Default::default()
    };
    let mut link = bitchat_ble::link::spawn(&config, false).await?;
    let inbound_rx = &mut link.inbound_rx;
    let outbound_tx = &link.outbound_tx;
    eprintln!("probe: link layer running — waiting for peers (Ctrl-C to quit)");

    // One announce so a phone that discovers us sees a packet too.
    outbound_tx
        .send(make_announce(&config.nickname))
        .await
        .ok();

    loop {
        match inbound_rx.recv().await {
            Some(bytes) => eprintln!(
                "probe: INBOUND {} bytes: {:02x?}",
                bytes.len(),
                &bytes[..bytes.len().min(32)]
            ),
            None => return Ok(()),
        }
    }
}

/// Minimal unsigned announce (the phone will log it but not add us —
/// enough to see traffic in both directions).
fn make_announce(nickname: &str) -> Vec<u8> {
    use bitchat_core::identity::MeshIdentity;
    use bitchat_core::message_type::MessageType;
    use bitchat_core::packet::Packet;
    use bitchat_core::tlv::AnnouncementPacket;
    use ed25519_dalek::Signer;

    let identity = MeshIdentity::generate();
    let announce = AnnouncementPacket {
        nickname: nickname.to_string(),
        noise_public_key: identity.noise.public_key,
        signing_public_key: identity.signing.verifying_key().to_bytes(),
        direct_neighbors: None,
        capabilities: None,
        bridge_geohash: None,
    };
    let mut packet = Packet::new(
        MessageType::Announce,
        identity.peer_id().mesh_bytes().expect("id"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        announce.encode().expect("tlv"),
        7,
    );
    packet.signature = Some(
        identity
            .sign(&packet.to_signing_bytes().expect("signable"))
            .to_bytes(),
    );
    packet.encode(false).expect("encodable")
}
