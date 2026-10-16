//! Minimal Nostr event model: NIP-01 id computation, BIP-340 Schnorr
//! signing/verification. Deliberately not the `nostr` crate: BitChat's
//! envelope reuses NIP kind numbers for a proprietary scheme, and keeping
//! the event model tiny keeps the wire behavior auditable against upstream
//! (`NostrProtocol.swift`).

use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::str::FromStr;

pub mod kinds {
    /// Unsigned inner message (inside ciphertext).
    pub const DM: u32 = 14;
    /// Sender-signed seal (inside ciphertext).
    pub const SEAL: u32 = 13;
    /// Public outer envelope (one-time key).
    pub const GIFT_WRAP: u32 = 1059;
    /// Geohash-scoped ephemeral public message.
    pub const EPHEMERAL_EVENT: u32 = 20000;
    /// Geohash presence heartbeat.
    pub const GEOHASH_PRESENCE: u32 = 20001;
    /// Sealed courier envelope parked under its rotating recipient tag.
    pub const COURIER_DROP: u32 = 1401;
}

/// Inbound tag limits (bounds untrusted relay input; mirrors upstream's
/// TransportConfig gates).
pub const MAX_EVENT_TAGS: usize = 256;
pub const MAX_EVENT_TAG_VALUES: usize = 16;
pub const MAX_EVENT_TAG_VALUE_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: i64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl NostrEvent {
    /// An unsigned event (id empty, no sig): a "rumor" in BitChat terms.
    pub fn unsigned(pubkey: String, created_at: i64, kind: u32, tags: Vec<Vec<String>>, content: String) -> Self {
        Self { id: String::new(), pubkey, created_at, kind, tags, content, sig: None }
    }

    /// Canonical NIP-01 serialization: compact JSON of
    /// `[0, pubkey, created_at, kind, tags, content]`.
    pub fn serialization(&self) -> Vec<u8> {
        serde_json::json!([
            0,
            self.pubkey,
            self.created_at,
            self.kind,
            self.tags,
            self.content,
        ])
        .to_string()
        .into_bytes()
    }

    /// The 32-byte sha256 of the canonical serialization (the message that
    /// Schnorr signs): its hex is the event id.
    pub fn event_hash(&self) -> [u8; 32] {
        sha2::Sha256::digest(self.serialization()).into()
    }

    /// Sign with BIP-340 Schnorr, filling id and sig.
    pub fn sign(mut self, key: &secp256k1::Keypair, secp: &secp256k1::Secp256k1<secp256k1::SignOnly>) -> Self {
        let hash = self.event_hash();
        self.id = hex::encode(hash);
        let sig = secp.sign_schnorr(&hash, key);
        self.sig = Some(sig.to_string());
        self
    }

    /// Structural checks for inbound events (bounds attacker-controlled
    /// input before parsing/verification work).
    pub fn within_inbound_limits(&self) -> bool {
        self.tags.len() <= MAX_EVENT_TAGS
            && self.tags.iter().all(|t| {
                t.len() <= MAX_EVENT_TAG_VALUES
                    && t.iter().all(|v| v.len() <= MAX_EVENT_TAG_VALUE_BYTES)
            })
    }

    /// Verify id and Schnorr signature against pubkey.
    pub fn is_valid_signature(&self) -> bool {
        let Some(sig_hex) = self.sig.as_deref() else {
            return false;
        };
        if self.pubkey.len() != 64 || sig_hex.len() != 128 {
            return false;
        }
        let Ok(sig) = secp256k1::schnorr::Signature::from_str(sig_hex) else {
            return false;
        };
        let Ok(pubkey) = secp256k1::XOnlyPublicKey::from_str(&self.pubkey) else {
            return false;
        };
        let hash = self.event_hash();
        if hex::encode(hash) != self.id {
            return false;
        }
        let message = hash.as_slice();
        let secp = secp256k1::Secp256k1::verification_only();
        secp.verify_schnorr(&sig, message, &pubkey).is_ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("malformed event: {0}")]
    Malformed(&'static str),
}
