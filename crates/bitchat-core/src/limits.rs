//! Receive-side per-type payload ceilings. Port of `PacketPayloadLimits.swift`.
//!
//! Enforced on every decoded payload, compressed or not. A future wire type
//! needing more than a v1 frame must be added here before it ships.

use crate::message_type::MessageType;

/// Raw DEFLATE's hard expansion ceiling (258-byte match costs >= 2 bits).
pub const MAX_DEFLATE_RATIO: usize = 1032;

const FRAGMENT_BYTES: usize = 1_280;
const CONTROL_BYTES: usize = 4 * 1024;
const REQUEST_SYNC_BYTES: usize = 8 * 1024;
const ENVELOPE_BYTES: usize = 64 * 1024;
pub const MAX_FRAMED_FILE_BYTES: usize = 1024 * 1024;
const TWICE_V1_FRAME_BYTES: usize = 128 * 1024;

/// Largest decoded payload accepted for a packet of `type`.
pub fn max_payload_bytes(message_type: u8) -> usize {
    let Some(mt) = MessageType::from_u8(message_type) else {
        return TWICE_V1_FRAME_BYTES;
    };
    match mt {
        MessageType::FileTransfer | MessageType::NoiseEncrypted => MAX_FRAMED_FILE_BYTES,
        MessageType::Message | MessageType::GroupMessage => TWICE_V1_FRAME_BYTES,
        MessageType::CourierEnvelope | MessageType::NostrCarrier => ENVELOPE_BYTES,
        MessageType::RequestSync => REQUEST_SYNC_BYTES,
        MessageType::Fragment => FRAGMENT_BYTES,
        _ => CONTROL_BYTES,
    }
}
