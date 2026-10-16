//! PeerCapabilities bit flags. Port of `PeerCapabilities.swift`.
//!
//! Encoded as canonical minimal little-endian bytes (no trailing zeros).
//! Decoders of `authenticatedPeerState` reject non-canonical encodings.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PeerCapabilities(pub u64);

impl PeerCapabilities {
    pub const PREKEYS: Self = Self(1 << 0);
    pub const WIFI_BULK: Self = Self(1 << 1);
    pub const GATEWAY: Self = Self(1 << 2);
    pub const GROUPS: Self = Self(1 << 3);
    pub const BOARD: Self = Self(1 << 4);
    pub const VOUCH: Self = Self(1 << 5);
    pub const MESH_DIAGNOSTICS: Self = Self(1 << 6);
    pub const BRIDGE: Self = Self(1 << 7);
    pub const PRIVATE_MEDIA: Self = Self(1 << 8);
    pub const PRIVATE_MEDIA_RECEIPTS: Self = Self(1 << 9);
    pub const NON_DESTRUCTIVE_NOISE_REPLACEMENT: Self = Self(1 << 10);

    /// Canonical minimal little-endian encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut value = self.0;
        let mut bytes = Vec::new();
        while value != 0 {
            bytes.push((value & 0xFF) as u8);
            value >>= 8;
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Self {
        let mut value: u64 = 0;
        for (i, &b) in bytes.iter().enumerate().take(8) {
            value |= (b as u64) << (8 * i);
        }
        Self(value)
    }

    /// The canonical bytes of `decode(bytes)`, i.e. whether `bytes` is the
    /// canonical encoding of the capabilities it decodes to.
    pub fn is_canonical(bytes: &[u8]) -> bool {
        !bytes.is_empty() && bytes.len() <= 8 && Self::decode(bytes).encode() == bytes
    }

    pub fn has(&self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for PeerCapabilities {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for PeerCapabilities {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_minimal_le() {
        assert_eq!(PeerCapabilities::PREKEYS.encode(), vec![0x01]);
        assert_eq!(PeerCapabilities::BRIDGE.encode(), vec![0x80]);
        assert_eq!(PeerCapabilities::PRIVATE_MEDIA.encode(), vec![0x00, 0x01]);
        assert_eq!(PeerCapabilities::default().encode(), Vec::<u8>::new());
    }

    #[test]
    fn non_canonical_rejected() {
        assert!(!PeerCapabilities::is_canonical(&[0x01, 0x00])); // trailing zero
        assert!(!PeerCapabilities::is_canonical(&[])); // empty
        assert!(!PeerCapabilities::is_canonical(&[0x01; 9])); // too long
        assert!(PeerCapabilities::is_canonical(&[0x00, 0x01]));
    }
}
