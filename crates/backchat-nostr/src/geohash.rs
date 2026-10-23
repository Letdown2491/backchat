//! Geohash channels: ephemeral identity derivation and event builders.
//! Ports of `NostrIdentityBridge.swift` (HMAC-derived per-cell personas)
//! and the geohash builders in `NostrProtocol.swift`.

use crate::event::{kinds, NostrEvent};
use crate::identity::Identity;
use hmac::Mac;
use secp256k1::Secp256k1;
use sha2::Digest;

/// Derive a deterministic, unlinkable Nostr identity for a geohash cell:
/// HMAC-SHA256(deviceSeed, geohash ‖ u32be(iteration)), retrying up to 10
/// iterations for a valid secp256k1 key, with a sha256(seed‖msg) fallback;
/// byte-for-byte the upstream schedule. The same cell string always yields
/// the same identity on one device, while different devices never collide.
pub fn derive_geohash_identity(device_seed: &[u8; 32], geohash: &str) -> Identity {
    for i in 0u32..10 {
        let mut input = geohash.as_bytes().to_vec();
        input.extend_from_slice(&i.to_be_bytes());
        let code = hmac_code(device_seed, &input);
        if let Some(id) = Identity::from_secret_hex(&hex::encode(code)) {
            return id;
        }
    }
    // Fallback: hash seed+msg and use it directly.
    let mut msg = device_seed.to_vec();
    msg.extend_from_slice(geohash.as_bytes());
    let digest: [u8; 32] = sha2::Sha256::digest(&msg).into();
    Identity::from_secret_hex(&hex::encode(digest)).expect("sha256 output is a valid key")
}

/// Bridge rendezvous identities use a distinct label so they stay unlinkable
/// from the geohash-chat identity for the same cell.
pub fn derive_bridge_identity(device_seed: &[u8; 32], cell: &str) -> Identity {
    derive_geohash_identity(device_seed, &format!("bridge|{cell}"))
}

fn hmac_code(seed: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(seed)
        .expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// A kind-20000 ephemeral geohash message.
pub fn create_ephemeral_geohash_event(
    content: &str,
    geohash: &str,
    sender: &Identity,
    nickname: Option<&str>,
    teleported: bool,
) -> NostrEvent {
    create_ephemeral_geohash_event_at(content, geohash, sender, nickname, teleported, now_secs())
}

/// Same, with an explicit timestamp (tests backdate traffic this way).
pub fn create_ephemeral_geohash_event_at(
    content: &str,
    geohash: &str,
    sender: &Identity,
    nickname: Option<&str>,
    teleported: bool,
    created_at: i64,
) -> NostrEvent {
    let tags = ephemeral_geohash_tags(geohash, nickname, teleported);
    let event = NostrEvent::unsigned(
        sender.public_key_hex(),
        created_at,
        kinds::EPHEMERAL_EVENT,
        tags,
        content.to_string(),
    );
    event.sign(&sender.keypair, &Secp256k1::signing_only())
}

/// A kind-20000 geohash message carrying a NIP-13 PoW nonce tag.
/// Mining is bounded: the event ships at the highest committed difficulty
/// still met, and unmined if the cap hits before any nonce qualifies.
pub fn create_mined_ephemeral_geohash_event(
    content: &str,
    geohash: &str,
    sender: &Identity,
    nickname: Option<&str>,
    teleported: bool,
    pow_target_bits: u32,
    max_mine_ms: u64,
) -> NostrEvent {
    let mut tags = ephemeral_geohash_tags(geohash, nickname, teleported);
    let created_at = now_secs();
    if let Some(nonce) = super::pow::mine_nonce_tag(
        &sender.public_key_hex(),
        created_at,
        kinds::EPHEMERAL_EVENT,
        &tags,
        content,
        pow_target_bits,
        max_mine_ms,
    ) {
        tags.push(nonce);
    }
    let event = NostrEvent::unsigned(
        sender.public_key_hex(),
        created_at,
        kinds::EPHEMERAL_EVENT,
        tags,
        content.to_string(),
    );
    event.sign(&sender.keypair, &Secp256k1::signing_only())
}

/// Kind-20001 presence heartbeat: empty content, `g` tag only, no nickname.
pub fn create_geohash_presence_event(geohash: &str, sender: &Identity) -> NostrEvent {
    let event = NostrEvent::unsigned(
        sender.public_key_hex(),
        now_secs(),
        kinds::GEOHASH_PRESENCE,
        vec![vec!["g".into(), geohash.to_string()]],
        String::new(),
    );
    event.sign(&sender.keypair, &Secp256k1::signing_only())
}

fn ephemeral_geohash_tags(geohash: &str, nickname: Option<&str>, teleported: bool) -> Vec<Vec<String>> {
    let mut tags = vec![vec!["g".into(), geohash.to_string()]];
    if let Some(n) = nickname.filter(|n| !n.trim().is_empty()) {
        tags.push(vec!["n".into(), n.trim().to_string()]);
    }
    if teleported {
        tags.push(vec!["t".into(), "teleport".into()]);
    }
    tags
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_identity_is_deterministic_and_linkage_free() {
        let seed = [7u8; 32];
        let a = derive_geohash_identity(&seed, "dr5rs");
        let b = derive_geohash_identity(&seed, "dr5rs");
        assert_eq!(a.public_key_hex(), b.public_key_hex());

        // Distinct cells -> unlinkable identities.
        let other = derive_geohash_identity(&seed, "dr5rt");
        assert_ne!(a.public_key_hex(), other.public_key_hex());

        // Different device seeds never collide either.
        let other_device = derive_geohash_identity(&[8u8; 32], "dr5rs");
        assert_ne!(a.public_key_hex(), other_device.public_key_hex());

        // Bridge label is distinct from the chat identity for the same cell.
        let bridge = derive_bridge_identity(&seed, "dr5rs");
        assert_ne!(a.public_key_hex(), bridge.public_key_hex());
    }

    #[test]
    fn geohash_event_shape() {
        let seed = [1u8; 32];
        let sender = derive_geohash_identity(&seed, "dr5rs");
        let event = create_ephemeral_geohash_event("hi", "dr5rs", &sender, Some("martin"), false);
        assert_eq!(event.kind, kinds::EPHEMERAL_EVENT);
        assert_eq!(event.tags[0], ["g", "dr5rs"]);
        assert_eq!(event.tags[1], ["n", "martin"]);
        assert!(event.is_valid_signature());
        assert_eq!(event.id, hex::encode(event.event_hash()));
    }
}

/// Encode a lat/lon point as a geohash cell of `precision` characters:
/// the standard interleave algorithm (lon bit first, base32 MSB-first).
/// Used for the location ladder in the channel browser; joins still accept
/// any cell string the user types.
pub fn encode(lat: f64, lon: f64, precision: usize) -> String {
    const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let (mut lat_lo, mut lat_hi) = (-90.0f64, 90.0f64);
    let (mut lon_lo, mut lon_hi) = (-180.0f64, 180.0f64);
    let bits = [16u8, 8, 4, 2, 1];
    let mut out = String::with_capacity(precision);
    let mut bit = 0usize;
    let mut ch = 0u8;
    let mut even = true; // lon first, like every geohash implementation
    while out.len() < precision {
        let mid;
        if even {
            mid = (lon_lo + lon_hi) / 2.0;
            if lon >= mid {
                ch |= bits[bit];
                lon_lo = mid;
            } else {
                lon_hi = mid;
            }
        } else {
            mid = (lat_lo + lat_hi) / 2.0;
            if lat >= mid {
                ch |= bits[bit];
                lat_lo = mid;
            } else {
                lat_hi = mid;
            }
        }
        even = !even;
        if bit < 4 {
            bit += 1;
        } else {
            out.push(BASE32[ch as usize] as char);
            bit = 0;
            ch = 0;
        }
    }
    out
}

/// Human label for a cell's zoom level: upstream's
/// `GeohashChannelLevel` mapping (`levelForLength` in
/// LocationChannelsSheet.kt).
pub fn level_name(len: usize) -> &'static str {
    match len {
        0..=2 => "region",
        3..=4 => "province",
        5 => "city",
        6 => "neighborhood",
        7 => "block",
        _ => "building",
    }
}

#[cfg(test)]
mod encode_tests {
    use super::*;

    #[test]
    fn matches_the_reference_vector() {
        // The worked example on Wikipedia's Geohash page.
        assert_eq!(encode(42.605, -5.603, 5), "ezs42");
    }

    #[test]
    fn prefixes_nest_by_precision() {
        let full = encode(37.7749, -122.4194, 6);
        assert!(full.starts_with(&encode(37.7749, -122.4194, 3)));
        // San Francisco Bay area cells sit under 9q.
        assert!(full.starts_with("9q"));
    }

    #[test]
    fn levels_match_upstreams_mapping() {
        assert_eq!(level_name(1), "region");
        assert_eq!(level_name(4), "province");
        assert_eq!(level_name(5), "city");
        assert_eq!(level_name(6), "neighborhood");
        assert_eq!(level_name(7), "block");
    }
}
