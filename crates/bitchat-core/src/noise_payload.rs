//! Typed payloads inside `noiseEncrypted` packets. Port of the
//! `NoisePayloadType` table in `BitchatProtocol.swift`.
//!
//! The first decrypted byte of a Noise payload indicates the type, so
//! observers cannot distinguish message types.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NoisePayloadType {
    PrivateMessage = 0x01,
    ReadReceipt = 0x02,
    Delivered = 0x03,
    GroupInvite = 0x06,
    GroupKeyUpdate = 0x07,
    VoiceFrame = 0x08,
    /// Value already deployed by the Android client.
    PrivateFile = 0x20,
    AuthenticatedPeerState = 0x21,
    VerifyChallenge = 0x10,
    VerifyResponse = 0x11,
    /// Transitive verification (web of trust).
    Vouch = 0x12,
}

/// Upstream #1434 briefly used 0x09 before release. Accept it while
/// prerelease builds age out, but never emit it: decoders canonicalize both
/// values to `PrivateFile`.
pub const PRERELEASE_PRIVATE_FILE_RAW: u8 = 0x09;

impl NoisePayloadType {
    pub fn from_u8(v: u8) -> Option<Self> {
        if v == PRERELEASE_PRIVATE_FILE_RAW {
            return Some(Self::PrivateFile);
        }
        Some(match v {
            0x01 => Self::PrivateMessage,
            0x02 => Self::ReadReceipt,
            0x03 => Self::Delivered,
            0x06 => Self::GroupInvite,
            0x07 => Self::GroupKeyUpdate,
            0x08 => Self::VoiceFrame,
            0x20 => Self::PrivateFile,
            0x21 => Self::AuthenticatedPeerState,
            0x10 => Self::VerifyChallenge,
            0x11 => Self::VerifyResponse,
            0x12 => Self::Vouch,
            _ => return None,
        })
    }

    pub fn is_private_file(v: Option<u8>) -> bool {
        matches!(v, Some(0x20) | Some(PRERELEASE_PRIVATE_FILE_RAW))
    }
}
