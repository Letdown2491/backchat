//! `bitchat1:` embedding: whole BitchatPackets inside Nostr content.
//! Port of `NostrEmbeddedBitChat.swift`.

use bitchat_core::message_type::MessageType;
use bitchat_core::noise_payload::NoisePayloadType;
use bitchat_core::packet::Packet;
use bitchat_core::peer_id::PeerId;
use bitchat_core::tlv::PrivateMessagePacket;
use base64::Engine;

#[derive(Debug, thiserror::Error)]
pub enum EmbeddedError {
    #[error("not a bitchat1: payload")]
    NotBitchat,
    #[error("invalid base64url body")]
    BadBody,
    #[error("inner packet failed to decode")]
    BadPacket,
    #[error("inner TLV failed to encode/decode")]
    BadTlv,
}

/// Encode a private message for Nostr DMs: TLV-encoded private message,
/// prefixed with the noise payload type, inside a whole packet.
pub fn encode_pm_for_nostr(
    content: &str,
    message_id: &str,
    recipient_peer_id: &PeerId,
    sender_peer_id: &PeerId,
) -> Result<String, EmbeddedError> {
    let tlv = PrivateMessagePacket {
        message_id: message_id.to_string(),
        content: content.to_string(),
    }
    .encode()
    .ok_or(EmbeddedError::BadTlv)?;

    let mut payload = vec![NoisePayloadType::PrivateMessage as u8];
    payload.extend_from_slice(&tlv);

    let packet = Packet::new(
        MessageType::NoiseEncrypted,
        sender_peer_id.mesh_bytes().ok_or(EmbeddedError::BadTlv)?,
        now_ms(),
        payload,
        7,
    )
    .with_recipient(recipient_peer_id.mesh_bytes().ok_or(EmbeddedError::BadTlv)?);
    encode_packet_as_bitchat1(&packet)
}

/// Encode a delivered/read ack for Nostr DMs.
pub fn encode_ack_for_nostr(
    ack_type: NoisePayloadType,
    message_id: &str,
    recipient_peer_id: &PeerId,
    sender_peer_id: &PeerId,
) -> Result<String, EmbeddedError> {
    if !matches!(ack_type, NoisePayloadType::Delivered | NoisePayloadType::ReadReceipt) {
        return Err(EmbeddedError::BadTlv);
    }
    let mut payload = vec![ack_type as u8];
    payload.extend_from_slice(message_id.as_bytes());

    let packet = Packet::new(
        MessageType::NoiseEncrypted,
        sender_peer_id.mesh_bytes().ok_or(EmbeddedError::BadTlv)?,
        now_ms(),
        payload,
        7,
    )
    .with_recipient(recipient_peer_id.mesh_bytes().ok_or(EmbeddedError::BadTlv)?);
    encode_packet_as_bitchat1(&packet)
}

/// Decode a `bitchat1:` body back into a packet.
pub fn decode_bitchat1(content: &str) -> Result<Packet, EmbeddedError> {
    let body = content.strip_prefix("bitchat1:").ok_or(EmbeddedError::NotBitchat)?;
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| EmbeddedError::BadBody)?;
    Packet::decode(&data).map_err(|_| EmbeddedError::BadPacket)
}

pub fn encode_packet_as_bitchat1(packet: &Packet) -> Result<String, EmbeddedError> {
    let data = packet.encode(true).map_err(|_| EmbeddedError::BadPacket)?;
    Ok(format!(
        "bitchat1:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    ))
}

trait WithRecipient {
    fn with_recipient(self, recipient: [u8; 8]) -> Self;
}

impl WithRecipient for Packet {
    fn with_recipient(mut self, recipient: [u8; 8]) -> Self {
        self.recipient_id = Some(recipient);
        self
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pm_roundtrip() {
        let encoded = encode_pm_for_nostr(
            "hello from linux",
            "msg-1",
            &PeerId::parse("aabbccddeeff0011"),
            &PeerId::parse("1122334455667788"),
        )
        .unwrap();
        assert!(encoded.starts_with("bitchat1:"));
        let packet = decode_bitchat1(&encoded).unwrap();
        assert_eq!(packet.kind, MessageType::NoiseEncrypted as u8);
        let mut payload = packet.payload;
        let payload_type = payload.remove(0);
        assert_eq!(payload_type, NoisePayloadType::PrivateMessage as u8);
        let pm = PrivateMessagePacket::decode(&payload).unwrap();
        assert_eq!(pm.content, "hello from linux");
        assert_eq!(pm.message_id, "msg-1");
    }

    #[test]
    fn ack_rejects_non_ack_types() {
        assert!(encode_ack_for_nostr(
            NoisePayloadType::PrivateMessage,
            "m",
            &PeerId::parse("aabbccddeeff0011"),
            &PeerId::parse("1122334455667788"),
        )
        .is_err());
    }
}
