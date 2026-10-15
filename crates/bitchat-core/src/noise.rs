//! Noise_XX_25519_ChaChaPoly_SHA256 sessions. Wraps `snow`.
//!
//! Upstream source of truth: `bitchat/Noise/NoiseProtocol.swift` and
//! `NoiseSessionManager.swift`; conformance vectors:
//! `fixtures/noise_test_vectors.json` (snow/cacophony format, consumed
//! directly by `tests/noise_vectors.rs`).

pub const PROTOCOL_NAME: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
pub const PEER_ID_LEN: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    #[error("snow error: {0}")]
    Snow(#[from] snow::Error),
    #[error("session not in the required state for this operation")]
    WrongState,
}

/// Key pair for the long-lived Noise static identity.
#[derive(Debug, Clone)]
pub struct StaticIdentity {
    pub private_key: [u8; 32],
    pub public_key: [u8; 32],
}

impl StaticIdentity {
    pub fn generate() -> Self {
        let kp = snow::Builder::new(PROTOCOL_NAME.parse().expect("valid pattern"))
            .generate_keypair()
            .expect("keypair");
        let mut private_key = [0u8; 32];
        private_key.copy_from_slice(&kp.private);
        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(&kp.public);
        Self { private_key, public_key }
    }

    /// The stable peer ID these keys map to (first 8 bytes of the SHA-256
    /// fingerprint of the static public key).
    pub fn peer_id(&self) -> crate::peer_id::PeerId {
        crate::peer_id::PeerId::from_noise_static_public_key(&self.public_key)
    }
}

// TODO(M1): NoiseSession (XX handshake driving + transport-mode encrypt/
// decrypt with rekey and replay windows) and NoiseSessionManager (session
// lifecycle, initiation policy). Port semantics from
// NoiseSession.swift / NoiseSessionManager.swift; the byte level is already
// covered by the fixture test below.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_id_derivation_is_stable() {
        let id = StaticIdentity::generate();
        let peer_id = id.peer_id();
        assert_eq!(peer_id.mesh_bytes().map(|b| b.len()), Some(PEER_ID_LEN));
        // deterministic in the key
        let again = crate::peer_id::PeerId::from_noise_static_public_key(&id.public_key);
        assert_eq!(peer_id, again);
    }
}
