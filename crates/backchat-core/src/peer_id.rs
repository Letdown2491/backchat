//! Peer IDs. Port of `BitFoundation/PeerID.swift`.
//!
//! A peer ID is a prefixed hex string. Mesh IDs are 8 bytes: the first 8
//! bytes of the SHA-256 fingerprint of the peer's Noise static public key.
//! They are stable for the life of the identity (panic-wipe rotation only).

pub const MESH_ID_LEN: usize = 8;
pub const NOISE_PUBKEY_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerIdPrefix {
    /// No prefix.
    Empty,
    /// `mesh:`: 16 hex chars.
    Mesh,
    /// `name:`: display name fallback.
    Name,
    /// `noise:` + 64 hex chars (full Noise static public key).
    Noise,
    /// `nostr_` + 16 hex chars (first 16 of a Nostr pubkey; geohash DMs).
    GeoDm,
    /// `nostr:` + 8 hex chars (geohash chat).
    GeoChat,
    /// `group_` + 32 hex chars (16-byte group ID; virtual conversation).
    Group,
    /// `bridge:` + 16 hex chars (sender reached across a mesh bridge).
    Bridge,
}

impl PeerIdPrefix {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Empty => "",
            Self::Mesh => "mesh:",
            Self::Name => "name:",
            Self::Noise => "noise:",
            Self::GeoDm => "nostr_",
            Self::GeoChat => "nostr:",
            Self::Group => "group_",
            Self::Bridge => "bridge:",
        }
    }
}

/// A parsed peer ID. `bare` is always lowercased, mirroring upstream.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerId {
    pub prefix: PeerIdPrefix,
    pub bare: String,
}

impl PeerId {
    pub fn parse(s: &str) -> Self {
        const PREFIXES: [(PeerIdPrefix, &str); 7] = [
            (PeerIdPrefix::Mesh, "mesh:"),
            (PeerIdPrefix::Name, "name:"),
            (PeerIdPrefix::Noise, "noise:"),
            (PeerIdPrefix::GeoDm, "nostr_"),
            (PeerIdPrefix::GeoChat, "nostr:"),
            (PeerIdPrefix::Group, "group_"),
            (PeerIdPrefix::Bridge, "bridge:"),
        ];
        for (prefix, raw) in PREFIXES {
            if let Some(bare) = s.strip_prefix(raw) {
                return Self { prefix, bare: bare.to_lowercase() };
            }
        }
        Self { prefix: PeerIdPrefix::Empty, bare: s.to_lowercase() }
    }

    pub fn from_mesh_bytes(bytes: &[u8]) -> Self {
        let mut id = [0u8; MESH_ID_LEN];
        id.copy_from_slice(&bytes[..MESH_ID_LEN]);
        Self::parse(&format!("mesh:{}", hex::encode(id)))
    }

    /// The stable 8-byte mesh ID: first 8 bytes of SHA-256 of the Noise
    /// static public key.
    pub fn from_noise_static_public_key(pk: &[u8; NOISE_PUBKEY_LEN]) -> Self {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(pk);
        Self::from_mesh_bytes(&digest)
    }

    pub fn full(&self) -> String {
        format!("{}{}", self.prefix.as_str(), self.bare)
    }

    /// The 8 raw mesh bytes, if this is a bare hex mesh ID.
    pub fn mesh_bytes(&self) -> Option<[u8; MESH_ID_LEN]> {
        if self.prefix != PeerIdPrefix::Empty && self.prefix != PeerIdPrefix::Mesh {
            return None;
        }
        let bytes = hex::decode(&self.bare).ok()?;
        if bytes.len() != MESH_ID_LEN {
            return None;
        }
        let mut id = [0u8; MESH_ID_LEN];
        id.copy_from_slice(&bytes);
        Some(id)
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.full())
    }
}
