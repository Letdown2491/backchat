//! Long-lived mesh identity: an X25519 Noise static keypair (peer ID +
//! sessions) and an independent Ed25519 signing keypair (packet/announce
//! signatures). Upstream stores both in the keychain
//! (`NoiseEncryptionService.swift` — the signing key is a separate Ed25519
//! key, *not* derived from the Noise key).

use ed25519_dalek::Signer;
use x25519_dalek::PublicKey as XPublicKey;

use crate::peer_id::PeerId;

/// Wire sizes: both keys are 32 raw bytes; persisted as one 64-byte blob.
pub const IDENTITY_BLOB_LEN: usize = 64;

#[derive(Clone)]
pub struct MeshIdentity {
    /// X25519 Noise static keypair — determines the peer ID.
    pub noise: crate::noise::StaticIdentity,
    /// Ed25519 signing keypair — announces and signed packets.
    pub signing: ed25519_dalek::SigningKey,
}

impl MeshIdentity {
    pub fn generate() -> Self {
        Self {
            noise: crate::noise::StaticIdentity::generate(),
            signing: ed25519_dalek::SigningKey::generate(&mut chacha20poly1305::aead::OsRng),
        }
    }

    pub fn peer_id(&self) -> PeerId {
        PeerId::from_noise_static_public_key(&self.noise.public_key)
    }

    pub fn sign(&self, data: &[u8]) -> ed25519_dalek::Signature {
        self.signing.sign(data)
    }

    pub fn verify(
        signing_public_key: &[u8; 32],
        signature: &[u8; 64],
        data: &[u8],
    ) -> bool {
        let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(signing_public_key) else {
            return false;
        };
        let Ok(sig) = ed25519_dalek::Signature::from_slice(signature) else {
            return false;
        };
        vk.verify_strict(data, &sig).is_ok()
    }

    /// 64-byte persistence blob: noise private ‖ signing private.
    pub fn to_bytes(&self) -> [u8; IDENTITY_BLOB_LEN] {
        let mut out = [0u8; IDENTITY_BLOB_LEN];
        out[..32].copy_from_slice(&self.noise.private_key);
        out[32..].copy_from_slice(self.signing.as_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8; IDENTITY_BLOB_LEN]) -> Option<Self> {
        let mut noise_priv = [0u8; 32];
        noise_priv.copy_from_slice(&bytes[..32]);
        let mut sign_priv = [0u8; 32];
        sign_priv.copy_from_slice(&bytes[32..]);
        Some(Self {
            noise: crate::noise::StaticIdentity {
                private_key: noise_priv,
                public_key: *XPublicKey::from(&x25519_dalek::StaticSecret::from(noise_priv))
                    .as_bytes(),
            },
            signing: ed25519_dalek::SigningKey::from_bytes(&sign_priv),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_and_peer_id() {
        let id = MeshIdentity::generate();
        let blob = id.to_bytes();
        let restored = MeshIdentity::from_bytes(&blob).unwrap();
        assert_eq!(restored.noise.private_key, id.noise.private_key);
        assert_eq!(restored.signing.to_bytes(), id.signing.to_bytes());
        assert_eq!(restored.peer_id(), id.peer_id());
    }

    #[test]
    fn signature_verifies_with_announced_key() {
        let id = MeshIdentity::generate();
        let sig = id.sign(b"payload");
        assert!(MeshIdentity::verify(
            &id.signing.verifying_key().to_bytes(),
            &sig.to_bytes(),
            b"payload"
        ));
        assert!(!MeshIdentity::verify(
            &id.signing.verifying_key().to_bytes(),
            &sig.to_bytes(),
            b"other"
        ));
    }
}
