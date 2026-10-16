//! Nostr identity: a secp256k1 keypair behind BIP-340 Schnorr.
//! Port of `NostrIdentity.swift` (the geohash ephemeral personas use this
//! too: a fresh identity per geohash cell).

use secp256k1::Secp256k1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub secret: secp256k1::SecretKey,
    pub keypair: secp256k1::Keypair,
}

impl Identity {
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let secp = Secp256k1::signing_only();
        Self::from_keypair(secp256k1::Keypair::new(&secp, &mut rng))
    }

    pub fn from_keypair(keypair: secp256k1::Keypair) -> Self {
        Self { secret: keypair.secret_key().clone(), keypair }
    }

    pub fn from_secret_hex(hex_str: &str) -> Option<Self> {
        let bytes = hex::decode(hex_str).ok()?;
        let secret = secp256k1::SecretKey::from_byte_array(bytes.try_into().ok()?).ok()?;
        Some(Self::from_keypair(secp256k1::Keypair::from_secret_key(
            &Secp256k1::signing_only(),
            &secret,
        )))
    }

    pub fn secret_hex(&self) -> String {
        hex::encode(self.secret.secret_bytes())
    }

    /// The x-only public key hex (64 chars): the Nostr pubkey.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.keypair.x_only_public_key().0.serialize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_roundtrip_via_hex() {
        let id = Identity::generate();
        let restored = Identity::from_secret_hex(&id.secret_hex()).unwrap();
        assert_eq!(id.public_key_hex(), restored.public_key_hex());
    }
}
