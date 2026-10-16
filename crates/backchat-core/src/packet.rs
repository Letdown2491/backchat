//! The packet codec. Port of `BitFoundation/BinaryProtocol.swift` +
//! `BitchatPacket.swift`.
//!
//! Wire format (big-endian throughout):
//!
//! ```text
//! v1: [version u8][type u8][ttl u8][timestamp u64][flags u8][len u16]
//! v2: [version u8][type u8][ttl u8][timestamp u64][flags u8][len u32]
//!     [sender 8B]([recipient 8B])([route_count u8 + hops])([orig_size])
//!     [payload][signature 64B]
//! ```
//!
//! Only Noise frames are padded by the *caller* (see `padding`); this module's
//! `encode` takes a `padding: bool` exactly like upstream.

use crate::compression;
use crate::limits::MAX_FRAMED_FILE_BYTES;
use crate::message_type::MessageType;
use crate::padding;
use crate::peer_id::MESH_ID_LEN;

pub const V1_HEADER_SIZE: usize = 14;
pub const V2_HEADER_SIZE: usize = 16;
pub const SENDER_ID_SIZE: usize = 8;
pub const RECIPIENT_ID_SIZE: usize = 8;
pub const SIGNATURE_SIZE: usize = 64;

pub const FLAG_HAS_RECIPIENT: u8 = 0x01;
pub const FLAG_HAS_SIGNATURE: u8 = 0x02;
pub const FLAG_IS_COMPRESSED: u8 = 0x04;
pub const FLAG_HAS_ROUTE: u8 = 0x08;
pub const FLAG_IS_RSR: u8 = 0x10;

/// Route is only supported for v2+ packets.
pub const MAX_ROUTE_HOPS: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub version: u8,
    pub kind: u8,
    pub ttl: u8,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
    pub sender_id: [u8; MESH_ID_LEN],
    pub recipient_id: Option<[u8; MESH_ID_LEN]>,
    /// v2 only. Route bytes are NOT included in the payload length field.
    pub route: Option<Vec<[u8; MESH_ID_LEN]>>,
    pub is_rsr: bool,
    /// Decoded payload (compression undone on decode).
    pub payload: Vec<u8>,
    pub signature: Option<[u8; SIGNATURE_SIZE]>,
}

impl Packet {
    pub fn new(
        kind: MessageType,
        sender_id: [u8; MESH_ID_LEN],
        timestamp: u64,
        payload: Vec<u8>,
        ttl: u8,
    ) -> Self {
        Self {
            version: 1,
            kind: kind as u8,
            ttl,
            timestamp,
            sender_id,
            recipient_id: None,
            route: None,
            is_rsr: false,
            payload,
            signature: None,
        }
    }

    /// Encoding used for signing: fixed TTL 0, no signature, no RSR flag.
    /// TTL is excluded because relays decrement it without invalidating
    /// the Ed25519 signature.
    ///
    /// Upstream signs `BinaryProtocol.encode(unsignedPacket)` whose `padding`
    /// parameter defaults to TRUE — even when the packet is *sent* unpadded
    /// (`BLEOutboundPacketPolicy.padsBLEFrame` returns false for announces and
    /// messages). So signatures cover the whole-frame bucket-padded form;
    /// sign unpadded and every client rejects your packets.
    pub fn to_signing_bytes(&self) -> Result<Vec<u8>, PacketEncodeError> {
        let unsigned = Packet {
            signature: None,
            is_rsr: false,
            ttl: 0,
            ..self.clone()
        };
        unsigned.encode(true)
    }

    pub fn encode(&self, padding: bool) -> Result<Vec<u8>, PacketEncodeError> {
        match self.version {
            1 | 2 => {}
            v => return Err(PacketEncodeError::UnsupportedVersion(v)),
        }
        let length_field_bytes = if self.version == 2 { 4 } else { 2 };

        // Compression: only when beneficial and representable in the length field.
        let mut payload = self.payload.clone();
        let mut is_compressed = false;
        let mut original_payload_size: Option<usize> = None;
        if compression::should_compress(&payload) {
            let max_representable =
                if self.version == 2 { u32::MAX as usize } else { u16::MAX as usize };
            if payload.len() <= max_representable
                && let Some(compressed) = compression::compress(&payload)
            {
                original_payload_size = Some(payload.len());
                payload = compressed;
                is_compressed = true;
            }
        }

        // Route: v2+ only, sanitized to exactly 8 bytes per hop.
        let original_route = match self.version >= 2 {
            true => self.route.clone().unwrap_or_default(),
            false => Vec::new(),
        };
        if original_route.iter().any(|hop| hop.is_empty()) {
            return Err(PacketEncodeError::EmptyRouteHop);
        }
        let has_route = !original_route.is_empty();
        let route_length = if has_route { 1 + original_route.len() * MESH_ID_LEN } else { 0 };
        if original_route.len() > MAX_ROUTE_HOPS {
            return Err(PacketEncodeError::RouteTooLong);
        }

        let original_size_field_bytes = if is_compressed { length_field_bytes } else { 0 };
        // payloadLength in the header is payload-only (no route bytes).
        let payload_data_size = payload.len() + original_size_field_bytes;
        match self.version {
            1 if payload_data_size > u16::MAX as usize => {
                return Err(PacketEncodeError::PayloadTooLong(payload_data_size));
            }
            _ => {}
        }

        let header_size = if self.version == 2 { V2_HEADER_SIZE } else { V1_HEADER_SIZE };
        let mut data = Vec::with_capacity(
            header_size
                + MESH_ID_LEN
                + if self.recipient_id.is_some() { RECIPIENT_ID_SIZE } else { 0 }
                + route_length
                + payload_data_size
                + if self.signature.is_some() { SIGNATURE_SIZE } else { 0 }
                + 255,
        );

        data.push(self.version);
        data.push(self.kind);
        data.push(self.ttl);
        data.extend_from_slice(&self.timestamp.to_be_bytes());

        let mut flags = 0u8;
        if self.recipient_id.is_some() {
            flags |= FLAG_HAS_RECIPIENT;
        }
        if self.signature.is_some() {
            flags |= FLAG_HAS_SIGNATURE;
        }
        if is_compressed {
            flags |= FLAG_IS_COMPRESSED;
        }
        if has_route && self.version >= 2 {
            flags |= FLAG_HAS_ROUTE;
        }
        if self.is_rsr {
            flags |= FLAG_IS_RSR;
        }
        data.push(flags);

        if self.version == 2 {
            data.extend_from_slice(&(payload_data_size as u32).to_be_bytes());
        } else {
            data.extend_from_slice(&(payload_data_size as u16).to_be_bytes());
        }

        data.extend_from_slice(&self.sender_id);
        if let Some(recipient) = self.recipient_id {
            data.extend_from_slice(&recipient);
        }

        if has_route {
            data.push(original_route.len() as u8);
            for hop in &original_route {
                data.extend_from_slice(hop);
            }
        }

        if is_compressed {
            let original = original_payload_size.expect("compressed implies original size");
            if self.version == 2 {
                data.extend_from_slice(&(original as u32).to_be_bytes());
            } else {
                data.extend_from_slice(&(original as u16).to_be_bytes());
            }
        }
        data.extend_from_slice(&payload);

        if let Some(sig) = self.signature {
            data.extend_from_slice(&sig);
        }

        if padding {
            let target = padding::optimal_block_size(data.len());
            Ok(padding::pad(&data, target))
        } else {
            Ok(data)
        }
    }

    pub fn decode(data: &[u8]) -> Result<Self, PacketDecodeError> {
        // Robustness when padding wasn't applied: try as-is, then unpadded.
        match Packet::decode_core(data) {
            Ok(pkt) => Ok(pkt),
            Err(_) => Packet::decode_core(padding::unpad(data)),
        }
    }

    fn decode_core(raw: &[u8]) -> Result<Self, PacketDecodeError> {
        let version = *raw.first().ok_or(PacketDecodeError::TooShort)?;
        let header_size = match version {
            1 => V1_HEADER_SIZE,
            2 => V2_HEADER_SIZE,
            v => return Err(PacketDecodeError::UnsupportedVersion(v)),
        };
        let length_field_bytes = if version == 2 { 4 } else { 2 };
        if raw.len() < header_size + SENDER_ID_SIZE {
            return Err(PacketDecodeError::TooShort);
        }

        let mut offset = 1;
        let mut read = |n: usize| -> Result<&[u8], PacketDecodeError> {
            raw.get(offset..offset + n)
                .ok_or(PacketDecodeError::Truncated)
                .inspect(|_| offset += n)
        };

        let kind = read(1)?[0];
        let ttl = read(1)?[0];
        let timestamp = u64::from_be_bytes(read(8)?.try_into().unwrap());
        let flags = read(1)?[0];

        let has_recipient = flags & FLAG_HAS_RECIPIENT != 0;
        let has_signature = flags & FLAG_HAS_SIGNATURE != 0;
        let is_compressed = flags & FLAG_IS_COMPRESSED != 0;
        // HAS_ROUTE is only valid for v2+; ignore the flag on v1.
        let has_route = version >= 2 && flags & FLAG_HAS_ROUTE != 0;
        let is_rsr = flags & FLAG_IS_RSR != 0;

        let payload_length = if version == 2 {
            u32::from_be_bytes(read(4)?.try_into().unwrap()) as usize
        } else {
            u16::from_be_bytes(read(2)?.try_into().unwrap()) as usize
        };
        if payload_length > MAX_FRAMED_FILE_BYTES {
            return Err(PacketDecodeError::PayloadTooLong);
        }

        let sender_id = read(SENDER_ID_SIZE)?.try_into().unwrap();
        let recipient_id = if has_recipient {
            Some(read(RECIPIENT_ID_SIZE)?.try_into().unwrap())
        } else {
            None
        };

        // Route bytes are NOT counted in payload_length.
        let mut route: Option<Vec<[u8; MESH_ID_LEN]>> = None;
        if has_route {
            let route_count = read(1)?[0] as usize;
            let mut hops = Vec::with_capacity(route_count);
            for _ in 0..route_count {
                hops.push(read(MESH_ID_LEN)?.try_into().unwrap());
            }
            route = Some(hops);
        }

        let max_payload = crate::limits::max_payload_bytes(kind);
        let payload = if is_compressed {
            if payload_length < length_field_bytes {
                return Err(PacketDecodeError::Truncated);
            }
            let original_size = if version == 2 {
                u32::from_be_bytes(read(4)?.try_into().unwrap()) as usize
            } else {
                u16::from_be_bytes(read(2)?.try_into().unwrap()) as usize
            };
            if original_size > max_payload {
                return Err(PacketDecodeError::PayloadTooLong);
            }
            let compressed_size = payload_length - length_field_bytes;
            if compressed_size == 0 {
                return Err(PacketDecodeError::Truncated);
            }
            let compressed = read(compressed_size)?;
            if original_size > compressed_size * crate::limits::MAX_DEFLATE_RATIO {
                return Err(PacketDecodeError::ImpossibleCompression);
            }
            let decompressed = compression::decompress(compressed, original_size)
                .ok_or(PacketDecodeError::BadCompression)?;
            decompressed
        } else {
            if payload_length > max_payload {
                return Err(PacketDecodeError::PayloadTooLong);
            }
            read(payload_length)?.to_vec()
        };

        let signature = if has_signature {
            Some(read(SIGNATURE_SIZE)?.try_into().unwrap())
        } else {
            None
        };

        Ok(Self {
            version,
            kind,
            ttl,
            timestamp,
            sender_id,
            recipient_id,
            route,
            is_rsr,
            payload,
            signature,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PacketEncodeError {
    #[error("unsupported packet version {0}")]
    UnsupportedVersion(u8),
    #[error("payload too long: {0}")]
    PayloadTooLong(usize),
    #[error("route longer than 255 hops")]
    RouteTooLong,
    #[error("empty route hop")]
    EmptyRouteHop,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PacketDecodeError {
    #[error("packet too short")]
    TooShort,
    #[error("packet truncated mid-field")]
    Truncated,
    #[error("unsupported packet version {0}")]
    UnsupportedVersion(u8),
    #[error("payload exceeds per-type ceiling")]
    PayloadTooLong,
    #[error("declared original size impossible for any deflate stream")]
    ImpossibleCompression,
    #[error("malformed compressed payload")]
    BadCompression,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_type::MessageType;

    fn sender() -> [u8; 8] {
        [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]
    }

    #[test]
    fn v1_roundtrip_uncompressed() {
        let pkt = Packet::new(MessageType::Message, sender(), 1_759_000_000_000, b"hello".to_vec(), 7);
        let bytes = pkt.encode(false).unwrap();
        let decoded = Packet::decode(&bytes).unwrap();
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn signing_bytes_are_bucket_padded() {
        // Upstream signs BinaryProtocol.encode(unsigned) with padding:true —
        // the whole frame padded to the 256/512/1024/2048 bucket — even for
        // packets sent unpadded. Signing unpadded makes every client reject
        // our packets (announces, messages, everything signed).
        let pkt = Packet::new(MessageType::Message, sender(), 1_759_000_000_000, b"hello".to_vec(), 7);
        let signable = pkt.to_signing_bytes().unwrap();
        assert!(
            [256, 512, 1024, 2048].contains(&signable.len()),
            "signing bytes must fill a padding bucket, got {}",
            signable.len()
        );
    }

    #[test]
    fn v1_roundtrip_padded_noise_frame() {
        let mut pkt = Packet::new(
            MessageType::NoiseEncrypted,
            sender(),
            1_759_000_000_000,
            vec![0u8; 100],
            7,
        );
        pkt.recipient_id = Some([9u8; 8]);
        let bytes = pkt.encode(true).unwrap();
        assert_eq!(bytes.len(), 256); // bucket padding applies to noise frames
        let decoded = Packet::decode(&bytes).unwrap();
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn v2_roundtrip_with_route() {
        let mut pkt = Packet::new(MessageType::Message, sender(), 42, b"routed".to_vec(), 5);
        pkt.version = 2;
        pkt.route = Some(vec![[1u8; 8], [2u8; 8]]);
        let bytes = pkt.encode(false).unwrap();
        let decoded = Packet::decode(&bytes).unwrap();
        assert_eq!(decoded, pkt);
    }

    #[test]
    fn large_payload_compresses_and_roundtrips() {
        let body = b"aaaaaaaa".repeat(200); // compresses well
        let pkt = Packet::new(MessageType::Message, sender(), 42, body.clone(), 7);
        let bytes = pkt.encode(false).unwrap();
        assert!(bytes.len() < body.len() + V1_HEADER_SIZE + 8);
        assert_eq!(Packet::decode(&bytes).unwrap(), pkt);
    }

    #[test]
    fn rejects_oversized_per_type_ceiling() {
        // announce is capped at 4 KiB; hand-craft a header claiming a payload
        // far beyond that and decode must refuse it
        let mut raw = Vec::new();
        raw.push(1u8); // version
        raw.push(0x01); // announce
        raw.push(7); // ttl
        raw.extend_from_slice(&42u64.to_be_bytes());
        raw.push(0); // flags
        raw.extend_from_slice(&60000u16.to_be_bytes()); // claimed payload len
        raw.extend_from_slice(&[0u8; 8]); // sender
        assert!(Packet::decode(&raw).is_err());
    }

    #[test]
    fn unknown_version_rejected() {
        let raw = [3u8; 32];
        assert!(Packet::decode(&raw).is_err());
    }
}
