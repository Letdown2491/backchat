//! Fragment chunking (outbound planning) and assembly (inbound).
//!
//! Wire format — fragment packet payload:
//! `[8-byte fragment id][2-byte index BE][2-byte total BE][1-byte original
//! type][chunk]`, total ≤ 10 000 (`BLEFragmentHeader`). Fragments carry the
//! *full original packet bytes*; the reassembled bytes are re-decoded as a
//! complete packet (`BLEFragmentHandler`). Assembly is keyed by
//! (sender, fragment id) and bypasses the packet deduplicator.

use std::collections::{BTreeMap, HashMap};

use bitchat_core::packet::Packet;

use crate::{BLE_DEFAULT_FRAGMENT_SIZE, BLE_MAX_WRITE};

/// Fragment payload chunks smaller than this signal an abusive stream.
pub const MAX_TOTAL_FRAGMENTS: usize = 10_000;
/// Max simultaneous assemblies (upstream `maxInFlightAssemblies`).
pub const MAX_IN_FLIGHT_ASSEMBLIES: usize = 8;
/// Reassembly size cap (upstream enforces per-type ceilings; one generous
/// cap covers every type we accept).
pub const ASSEMBLY_SIZE_LIMIT: usize = 256 * 1024;
/// Abandon assemblies with no progress for this long.
pub const ASSEMBLY_EXPIRY_MS: u64 = 30_000;

/// Outbound: split `encoded` (the full packet binary) into fragment packets.
/// `recipient` is inherited from the original packet (None ⇒ broadcast).
pub fn plan_fragments(
    sender_id: [u8; 8],
    recipient: Option<[u8; 8]>,
    timestamp: u64,
    ttl: u8,
    encoded: &[u8],
) -> Vec<Packet> {
    let total = encoded.len().div_ceil(BLE_DEFAULT_FRAGMENT_SIZE);
    let mut frag_id = [0u8; 8];
    // Fragment ids only need to be unique per sender per few minutes.
    let mut id_seed = [sender_id[..4].to_vec(), encoded.len().to_be_bytes().to_vec(), (timestamp as u128 & 0xFFFF_FFFF_FFFF).to_be_bytes().to_vec()].concat();
    let digest = <sha2::Sha256 as sha2::Digest>::digest(&id_seed);
    frag_id.copy_from_slice(&digest[..8]);
    id_seed.clear();

    (0..total)
        .map(|i| {
            let start = i * BLE_DEFAULT_FRAGMENT_SIZE;
            let end = ((start + BLE_DEFAULT_FRAGMENT_SIZE) as usize).min(encoded.len());
            let chunk = &encoded[start..end];
            let mut payload = Vec::with_capacity(13 + chunk.len());
            payload.extend_from_slice(&frag_id);
            payload.extend_from_slice(&(i as u16).to_be_bytes());
            payload.extend_from_slice(&(total as u16).to_be_bytes());
            // Original type comes from the encoded packet's type byte.
            payload.push(encoded[1]);
            payload.extend_from_slice(chunk);
            let mut fragment = Packet::new(
                bitchat_core::message_type::MessageType::Fragment,
                sender_id,
                timestamp,
                payload,
                ttl,
            );
            fragment.recipient_id = recipient;
            fragment
        })
        .collect()
}

/// Parse a fragment packet's payload header.
pub struct FragmentHeader {
    pub sender: [u8; 8],
    pub id: [u8; 8],
    pub index: u16,
    pub total: u16,
    pub original_type: u8,
    pub data: Vec<u8>,
    pub is_broadcast: bool,
}

impl FragmentHeader {
    pub fn parse(packet: &Packet) -> Option<Self> {
        if packet.payload.len() < 13 {
            return None;
        }
        let index = u16::from_be_bytes([packet.payload[8], packet.payload[9]]);
        let total = u16::from_be_bytes([packet.payload[10], packet.payload[11]]);
        if total == 0 || total as usize > MAX_TOTAL_FRAGMENTS || index >= total {
            return None;
        }
        let is_broadcast = match packet.recipient_id {
            None => true,
            Some(r) => r == [0xFF; 8],
        };
        Some(Self {
            sender: packet.sender_id,
            id: packet.payload[..8].try_into().unwrap(),
            index,
            total,
            original_type: packet.payload[12],
            data: packet.payload[13..].to_vec(),
            is_broadcast,
        })
    }
}

struct Assembly {
    started_ms: u64,
    chunks: BTreeMap<u16, Vec<u8>>,
    total: u16,
    bytes: usize,
}

/// Inbound fragment assembly keyed by (sender, fragment id).
#[derive(Default)]
pub struct FragmentAssembler {
    assemblies: HashMap<([u8; 8], [u8; 8]), Assembly>,
}

pub enum Assembled {
    /// Fragment stored; not yet complete.
    Stored,
    /// All fragments present; `data` is the full original packet binary.
    Complete(Vec<u8>),
}

impl FragmentAssembler {
    /// Returns [`Assembled::Complete`] exactly once per assembly.
    pub fn append(&mut self, header: &FragmentHeader, now_ms: u64) -> Assembled {
        self.remove_expired(now_ms);

        let key = (header.sender, header.id);
        if !self.assemblies.contains_key(&key) {
            if self.assemblies.len() >= MAX_IN_FLIGHT_ASSEMBLIES {
                // Evict the oldest assembly to make room.
                let oldest = self
                    .assemblies
                    .iter()
                    .min_by_key(|(_, a)| a.started_ms)
                    .map(|(k, _)| *k);
                if let Some(oldest) = oldest {
                    self.assemblies.remove(&oldest);
                }
            }
            self.assemblies.insert(
                key,
                Assembly { started_ms: now_ms, chunks: BTreeMap::new(), total: header.total, bytes: 0 },
            );
        }
        let asm = self.assemblies.get_mut(&key).expect("just inserted");

        let replaced = asm.chunks.get(&header.index).map_or(0, |c| c.len());
        if asm.bytes - replaced + header.data.len() > ASSEMBLY_SIZE_LIMIT {
            self.assemblies.remove(&key);
            return Assembled::Stored;
        }
        asm.bytes += header.data.len() - replaced;
        asm.chunks.insert(header.index, header.data.clone());

        if asm.chunks.len() == asm.total as usize {
            let asm = self.assemblies.remove(&key).expect("just used");
            let data: Vec<u8> = asm.chunks.into_values().flatten().collect();
            Assembled::Complete(data)
        } else {
            Assembled::Stored
        }
    }

    fn remove_expired(&mut self, now_ms: u64) {
        self.assemblies
            .retain(|_, a| now_ms.saturating_sub(a.started_ms) < ASSEMBLY_EXPIRY_MS);
    }
}

/// Whether an original packet needs fragmenting for the link MTU.
pub fn needs_fragmentation(encoded_len: usize) -> bool {
    encoded_len > BLE_MAX_WRITE
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitchat_core::message_type::MessageType;

    #[test]
    fn roundtrip_multi_fragment_broadcast() {
        // Incompressible bytes (core compresses payloads on encode).
        let mut x: u64 = 0x243F_6A88_85A3_08D3;
        let payload: Vec<u8> = (0..BLE_DEFAULT_FRAGMENT_SIZE * 2 + 100)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 0xFF) as u8
            })
            .collect();
        let packet = Packet::new(MessageType::Message, [7; 8], 42, payload, 7);
        let encoded = packet.encode(false).unwrap();

        let frags = plan_fragments([7; 8], None, 42, 7, &encoded);
        assert_eq!(frags.len(), 3);
        assert!(frags.iter().all(|f| f.recipient_id.is_none()));
        assert!(frags.iter().all(|f| f.kind == MessageType::Fragment as u8));

        let mut asm = FragmentAssembler::default();
        let mut result = Assembled::Stored;
        for f in &frags {
            let header = FragmentHeader::parse(f).unwrap();
            assert!(header.is_broadcast);
            result = asm.append(&header, 0);
        }
        let Assembled::Complete(data) = result else {
            panic!("expected completion");
        };
        assert_eq!(data, encoded);
    }

    #[test]
    fn small_packet_needs_no_fragmentation() {
        let packet = Packet::new(MessageType::Message, [7; 8], 42, b"hi".to_vec(), 7);
        assert!(!needs_fragmentation(packet.encode(false).unwrap().len()));
    }

    #[test]
    fn directed_fragment_carries_recipient() {
        let mut packet = Packet::new(MessageType::Message, [7; 8], 42, vec![0; 1000], 7);
        packet.recipient_id = Some([9; 8]);
        let encoded = packet.encode(false).unwrap();
        let frags = plan_fragments([7; 8], Some([9; 8]), 42, 7, &encoded);
        assert!(frags.iter().all(|f| f.recipient_id == Some([9; 8])));
    }
}
