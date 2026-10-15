//! bitchat-nostr: the internet transport.
//!
//! Upstream sources of truth (see PORTING.md):
//! - `NostrEmbeddedBitChat.swift` — `bitchat1:` base64url embedding of whole
//!   packets (noiseEncrypted, ttl 7; random unlinkable sender IDs for
//!   geohash DMs)
//! - whitepaper §5.3 — private envelope: unsigned inner message (kind 14)
//!   inside a sender-signed seal (kind 13), inside a public envelope
//!   (kind 1059) signed by a one-time key. Content fields are `v2:` +
//!   base64url(24 B nonce ‖ XChaCha20-Poly1305 ct ‖ 16 B tag); keys from
//!   secp256k1 ECDH + HKDF-SHA256 reusing the "nip44-v2" info label.
//!   NOT NIP-17/NIP-44/NIP-59 compatible.
//! - `NostrIdentity.swift` / `NostrIdentityBridge.swift` — secp256k1
//!   identities, per-geohash ephemeral personas
//! - `NostrRelayManager.swift` / `GeoRelayDirectory.swift` — relay set,
//!   subscriptions, 24 h lookback

// TODO(M1): envelope::seal / envelope::open, embedded::encode_bitchat1 /
// parse_bitchat1, identity::GeohashIdentity, relay subscriptions.
// Conformance: XChaCha20Poly1305CompatTests + bitchatTests/Nostr/Fixtures
// must be ported before any of this is trusted.
