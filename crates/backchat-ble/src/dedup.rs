//! Packet deduplication: `PacketIdUtil.computeId` (SHA-256 over
//! `type ‖ senderID ‖ timestamp(BE) ‖ payload`) with an LRU cap of 1000
//! entries and 5-minute expiry (`PacketDeduplicator`).

use std::collections::{HashMap, VecDeque};

use backchat_core::packet::Packet;
use sha2::{Digest, Sha256};

pub const DEDUP_CAP: usize = 1000;
pub const DEDUP_EXPIRY_MS: u64 = 300_000;

/// First 16 bytes of SHA-256(type ‖ sender ‖ timestamp BE ‖ payload).
pub fn packet_id(packet: &Packet) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update([packet.kind]);
    hasher.update(packet.sender_id);
    hasher.update(packet.timestamp.to_be_bytes());
    hasher.update(&packet.payload);
    hasher.finalize()[..16].try_into().expect("16 bytes")
}

pub struct DedupCache {
    seen: HashMap<[u8; 16], u64>,
    order: VecDeque<([u8; 16], u64)>,
}

impl Default for DedupCache {
    fn default() -> Self {
        Self { seen: HashMap::new(), order: VecDeque::new() }
    }
}

impl DedupCache {
    /// Returns true (and records) if this packet id has NOT been seen.
    /// `now_ms` drives expiry; entries older than [`DEDUP_EXPIRY_MS`] and
    /// entries beyond the LRU cap are dropped.
    pub fn check_and_insert(&mut self, id: [u8; 16], now_ms: u64) -> bool {
        // Lazily evict expired head entries.
        while let Some(ts) = self.order.front().map(|(_, t)| *t) {
            if now_ms.saturating_sub(ts) < DEDUP_EXPIRY_MS {
                break;
            }
            if let Some((removed, _)) = self.order.pop_front() {
                self.seen.remove(&removed);
            }
        }
        if self.seen.contains_key(&id) {
            return false;
        }
        self.seen.insert(id, now_ms);
        self.order.push_back((id, now_ms));
        while self.seen.len() > DEDUP_CAP {
            if let Some((oldest, _)) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backchat_core::message_type::MessageType;

    fn sample_packet(ts: u64) -> Packet {
        Packet::new(MessageType::Message, [1; 8], ts, b"hello".to_vec(), 7)
    }

    #[test]
    fn rejects_duplicate_then_expires() {
        let mut cache = DedupCache::default();
        let id = packet_id(&sample_packet(123));
        assert!(cache.check_and_insert(id, 1000));
        assert!(!cache.check_and_insert(id, 1001));
        // After expiry, allowed again.
        assert!(cache.check_and_insert(id, 1000 + DEDUP_EXPIRY_MS + 1));
    }

    #[test]
    fn distinct_timestamps_are_distinct_packets() {
        let mut cache = DedupCache::default();
        assert!(cache.check_and_insert(packet_id(&sample_packet(1)), 0));
        assert!(cache.check_and_insert(packet_id(&sample_packet(2)), 0));
    }

    #[test]
    fn lru_cap_evicts_oldest() {
        let mut cache = DedupCache::default();
        for i in 0..(DEDUP_CAP + 10) as u64 {
            let pkt = Packet::new(MessageType::Message, [1; 8], i, vec![], 7);
            assert!(cache.check_and_insert(packet_id(&pkt), 0));
        }
        assert_eq!(cache.seen.len(), DEDUP_CAP);
        // The oldest entry (timestamp 0) was evicted; re-insert succeeds.
        let pkt = Packet::new(MessageType::Message, [1; 8], 0, vec![], 7);
        assert!(cache.check_and_insert(packet_id(&pkt), 0));
    }
}
