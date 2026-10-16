//! backchat-nostr: the internet transport.
//!
//! Upstream sources of truth (see PORTING.md):
//! - `NostrEmbeddedBitChat.swift` — `bitchat1:` base64url embedding of whole
//!   packets (noiseEncrypted, ttl 7; random unlinkable sender IDs for
//!   geohash DMs) → [`embedded`]
//! - `NostrProtocol.swift` — the private-envelope scheme (kind 13/14/1059,
//!   `v2:` + XChaCha20-Poly1305 over secp256k1 ECDH + HKDF-SHA256 with the
//!   historical "nip44-v2" info label; NOT NIP-17/44/59) → [`envelope`]
//! - `NostrIdentityBridge.swift` — HMAC-derived per-geohash personas →
//!   [`geohash::derive_geohash_identity`]
//! - `NostrPoW.swift` — NIP-13 nonce mining for kind-20000 → [`pow`]
//! - `NostrRelayManager.swift` / `GeoRelayDirectory.swift` → [`relay`]

pub mod crypto;
pub mod embedded;
pub mod envelope;
pub mod event;
pub mod geo_ip;
pub mod geohash;
pub mod identity;
pub mod pow;
#[cfg(feature = "relay")]
pub mod relay;
pub mod relay_geo;

pub use event::NostrEvent;
pub use identity::Identity;

/// Default relay set (used when the geo relay directory is unavailable).
/// Upstream ships a GPS-tagged CSV directory (`relays/online_relays_gps.csv`)
/// and picks relays nearest the geohash; the bundled CSV fallback lives with
/// the relay client.
pub const FALLBACK_RELAYS: &[&str] = &[
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://nostr.mom",
];
