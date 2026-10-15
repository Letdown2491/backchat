//! Message type table. Port of `BitFoundation/MessageType.swift`.
//!
//! 24 original wire types were consolidated into the 18 below. Type gaps are
//! load-bearing: see the 0x05 note (recycled twice before #446) in upstream
//! comments. Never reassign a value, ever.

/// Wire message types (first byte after the version byte of a packet header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageType {
    // Public messages (unencrypted)
    Announce = 0x01,
    Message = 0x02,
    Leave = 0x03,
    CourierEnvelope = 0x04,
    RequestSync = 0x21, // GCS-filter sync request (local-only)

    // Noise encryption
    NoiseHandshake = 0x10,
    NoiseEncrypted = 0x11,

    // Fragmentation
    Fragment = 0x20,
    FileTransfer = 0x22,
    BoardPost = 0x23,
    PrekeyBundle = 0x24,
    GroupMessage = 0x25,

    // Mesh diagnostics
    Ping = 0x26,
    Pong = 0x27,

    // Gateway mode: signed Nostr events ferried between a mesh-only peer
    // and an internet gateway peer.
    NostrCarrier = 0x28,

    // Live voice: one signed push-to-talk burst packet (ephemeral broadcast).
    VoiceFrame = 0x29,

    // Identity-free presence for rotating peer IDs (epoch + recognition
    // tags). Deliberately 0x2C, NOT the seemingly-free 0x05.
    AnnounceV2 = 0x2C,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Announce,
            0x02 => Self::Message,
            0x03 => Self::Leave,
            0x04 => Self::CourierEnvelope,
            0x21 => Self::RequestSync,
            0x10 => Self::NoiseHandshake,
            0x11 => Self::NoiseEncrypted,
            0x20 => Self::Fragment,
            0x22 => Self::FileTransfer,
            0x23 => Self::BoardPost,
            0x24 => Self::PrekeyBundle,
            0x25 => Self::GroupMessage,
            0x26 => Self::Ping,
            0x27 => Self::Pong,
            0x28 => Self::NostrCarrier,
            0x29 => Self::VoiceFrame,
            0x2C => Self::AnnounceV2,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Announce => "announce",
            Self::AnnounceV2 => "announceV2",
            Self::Message => "message",
            Self::Leave => "leave",
            Self::CourierEnvelope => "courierEnvelope",
            Self::RequestSync => "requestSync",
            Self::NoiseHandshake => "noiseHandshake",
            Self::NoiseEncrypted => "noiseEncrypted",
            Self::Fragment => "fragment",
            Self::FileTransfer => "fileTransfer",
            Self::BoardPost => "boardPost",
            Self::PrekeyBundle => "prekeyBundle",
            Self::GroupMessage => "groupMessage",
            Self::Ping => "ping",
            Self::Pong => "pong",
            Self::NostrCarrier => "nostrCarrier",
            Self::VoiceFrame => "voiceFrame",
        }
    }
}
