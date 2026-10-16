//! NIP-13 proof-of-work nonce tags for kind-20000 geohash messages.
//! Port of `NostrPoW.swift` semantics: mine a `nonce` tag so the event id
//! has the target number of leading zero bits; bounded by a time cap —
//! sending is never blocked, so on timeout the event ships at whatever
//! difficulty was reached (none, if no nonce qualified).

use sha2::Digest;

/// Default difficulty target (bits of leading zeros in the event id).
pub const TARGET_BITS: u32 = 20;

/// Hex value with at least `bits` leading zero bits (NIP-13: leading zero
/// nibbles first, then the first significant nibble halved for odd counts).
fn meets_target(id_hex: &str, bits: u32) -> bool {
    if bits == 0 {
        return true;
    }
    let mut counted = 0u32;
    for ch in id_hex.bytes() {
        let v: u32 = match ch {
            b'0' => 0,
            b'1' => 1,
            b'2' => 2,
            b'3' => 3,
            b'4' => 4,
            b'5' => 5,
            b'6' => 6,
            b'7' => 7,
            b'8' => 8,
            b'9' => 9,
            b'a' | b'A' => 10,
            b'b' | b'B' => 11,
            b'c' | b'C' => 12,
            b'd' | b'D' => 13,
            b'e' | b'E' => 14,
            b'f' | b'F' => 15,
            _ => return false,
        };
        if v == 0 {
            counted += 4;
            if counted >= bits {
                return true;
            }
        } else {
            // Count leading zero bits within this nibble (a nibble value v
            // has 4 - floor(log2(v)) - 1... equivalently for 1..15: lz(v)).
            let lz = match v {
                1 => 3,
                2 | 3 => 2,
                4..=7 => 1,
                _ => 0,
            };
            return counted + lz >= bits;
        }
    }
    counted >= bits
}

fn event_id_hex(
    pubkey: &str,
    created_at: i64,
    kind: u32,
    tags: &[Vec<String>],
    content: &str,
    nonce_hex: &str,
    target_bits: u32,
) -> String {
    let mut tags_with_nonce = tags.to_vec();
    tags_with_nonce.push(vec![
        "nonce".to_string(),
        nonce_hex.to_string(),
        format!("{target_bits}"),
    ]);
    let serialized = serde_json::json!([
        0,
        pubkey,
        created_at,
        kind,
        tags_with_nonce,
        content,
    ])
    .to_string()
    .into_bytes();
    hex::encode(sha2::Sha256::digest(serialized))
}

/// Mine a `nonce` tag, bounded by `max_mine_ms`. Returns `["nonce", hex,
/// target]` or None if nothing qualified in time.
pub fn mine_nonce_tag(
    pubkey: &str,
    created_at: i64,
    kind: u32,
    tags: &[Vec<String>],
    content: &str,
    target_bits: u32,
    max_mine_ms: u64,
) -> Option<Vec<String>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(max_mine_ms);
    let mut nonce: u64 = 0;
    loop {
        let nonce_hex = format!("{nonce:x}");
        let id = event_id_hex(pubkey, created_at, kind, tags, content, &nonce_hex, target_bits);
        if meets_target(&id, target_bits) {
            return Some(vec!["nonce".into(), nonce_hex, format!("{target_bits}")]);
        }
        nonce += 1;
        if nonce % 4096 == 0 && std::time::Instant::now() > deadline {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meets_target_math() {
        assert!(meets_target("00000abc", 18));
        assert!(!meets_target("00000abc", 21));
        // 16 zero bits + 0x8 (binary 1000, no leading zero bits)
        assert!(!meets_target("00008abc", 17));
        // 16 zero bits + 0x4 (binary 0100, one leading zero bit)
        assert!(meets_target("00004abc", 17));
        assert!(!meets_target("00004abc", 18));
        assert!(meets_target("deadbeef", 0));
        assert!(!meets_target("deadbeef", 1));
    }
}
