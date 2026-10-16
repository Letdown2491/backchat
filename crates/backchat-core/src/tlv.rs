//! Wire TLVs. Port of `bitchat/Protocols/Packets.swift`.
//!
//! All TLV sections are `type u8 | length u8 | value`. Announce decoding is
//! tolerant (unknown TLVs skipped); `AuthenticatedPeerState` decoding is
//! strict (canonical capabilities required, duplicates rejected).

use crate::capabilities::PeerCapabilities;
use crate::peer_id::MESH_ID_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnouncementPacket {
    pub nickname: String,
    /// Noise static public key (Curve25519).
    pub noise_public_key: [u8; 32],
    /// Ed25519 signing public key.
    pub signing_public_key: [u8; 32],
    /// Up to 10 direct-neighbor 8-byte peer IDs.
    pub direct_neighbors: Option<Vec<[u8; MESH_ID_LEN]>>,
    /// nil when absent (old clients).
    pub capabilities: Option<PeerCapabilities>,
    /// Rendezvous geohash cell this peer bridges (advertises `.bridge`).
    pub bridge_geohash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlvType {
    Nickname = 0x01,
    NoisePublicKey = 0x02,
    SigningPublicKey = 0x03,
    DirectNeighbors = 0x04,
    Capabilities = 0x05,
    BridgeGeohash = 0x06,
}

impl TlvType {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Self::Nickname,
            0x02 => Self::NoisePublicKey,
            0x03 => Self::SigningPublicKey,
            0x04 => Self::DirectNeighbors,
            0x05 => Self::Capabilities,
            0x06 => Self::BridgeGeohash,
            _ => return None,
        })
    }
}

impl AnnouncementPacket {
    pub fn encode(&self) -> Option<Vec<u8>> {
        let nickname = self.nickname.as_bytes();
        if nickname.len() > 255 {
            return None;
        }
        let mut data = Vec::with_capacity(2 + nickname.len() + 2 + 32 + 2 + 32);

        data.push(TlvType::Nickname as u8);
        data.push(nickname.len() as u8);
        data.extend_from_slice(nickname);

        data.push(TlvType::NoisePublicKey as u8);
        data.push(32);
        data.extend_from_slice(&self.noise_public_key);

        data.push(TlvType::SigningPublicKey as u8);
        data.push(32);
        data.extend_from_slice(&self.signing_public_key);

        if let Some(neighbors) = &self.direct_neighbors
            && !neighbors.is_empty()
        {
            // capped at 10 upstream; count must survive the u8 length byte
            let bytes = neighbors
                .iter()
                .take(10)
                .flat_map(|n| n.iter().copied())
                .collect::<Vec<u8>>();
            data.push(TlvType::DirectNeighbors as u8);
            data.push(bytes.len() as u8);
            data.extend_from_slice(&bytes);
        }

        if let Some(caps) = &self.capabilities {
            let encoded = caps.encode();
            if encoded.len() <= 255 {
                data.push(TlvType::Capabilities as u8);
                data.push(encoded.len() as u8);
                data.extend_from_slice(&encoded);
            }
        }

        if let Some(cell) = &self.bridge_geohash {
            let cell = cell.as_bytes();
            if !cell.is_empty() && cell.len() <= 12 {
                data.push(TlvType::BridgeGeohash as u8);
                data.push(cell.len() as u8);
                data.extend_from_slice(cell);
            }
        }

        Some(data)
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let mut offset = 0;
        let mut nickname: Option<String> = None;
        let mut noise_public_key: Option<[u8; 32]> = None;
        let mut signing_public_key: Option<[u8; 32]> = None;
        let mut direct_neighbors: Option<Vec<[u8; MESH_ID_LEN]>> = None;
        let mut capabilities: Option<PeerCapabilities> = None;
        let mut bridge_geohash: Option<String> = None;

        while offset + 2 <= data.len() {
            let type_raw = data[offset];
            let length = data[offset + 1] as usize;
            offset += 2;
            let value = data.get(offset..offset + length)?;
            offset += length;

            let Some(tlv) = TlvType::from_u8(type_raw) else {
                continue; // tolerant decoder for forward compatibility
            };
            match tlv {
                TlvType::Nickname => nickname = String::from_utf8(value.to_vec()).ok(),
                TlvType::NoisePublicKey => {
                    noise_public_key = <[u8; 32]>::try_from(value).ok()
                }
                TlvType::SigningPublicKey => {
                    signing_public_key = <[u8; 32]>::try_from(value).ok()
                }
                TlvType::DirectNeighbors => {
                    if !value.is_empty() && value.len() % MESH_ID_LEN == 0 {
                        direct_neighbors = Some(
                            value
                                .chunks_exact(MESH_ID_LEN)
                                .map(|c| c.try_into().unwrap())
                                .collect(),
                        );
                    }
                }
                TlvType::Capabilities => capabilities = Some(PeerCapabilities::decode(value)),
                TlvType::BridgeGeohash => {
                    if value.len() <= 12 {
                        bridge_geohash = String::from_utf8(value.to_vec()).ok();
                    }
                }
            }
        }

        Some(Self {
            nickname: nickname?,
            noise_public_key: noise_public_key?,
            signing_public_key: signing_public_key?,
            direct_neighbors,
            capabilities,
            bridge_geohash,
        })
    }
}

/// State that is authoritative only because it was carried inside an
/// established Noise session: proves possession of the advertised Noise
/// static key. Wire format v1:
/// `[version=0x01][type][length][value]...`, strict canonical decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPeerStatePacket {
    pub capabilities: PeerCapabilities,
    pub signing_public_key: [u8; 32],
}

const AUTH_PEER_STATE_VERSION: u8 = 1;

impl AuthenticatedPeerStatePacket {
    pub fn encode(&self) -> Option<Vec<u8>> {
        let capability_bytes = self.capabilities.encode();
        if capability_bytes.is_empty() || capability_bytes.len() > 8 {
            return None;
        }
        let mut data = vec![AUTH_PEER_STATE_VERSION];
        data.push(0x01);
        data.push(capability_bytes.len() as u8);
        data.extend_from_slice(&capability_bytes);
        data.push(0x02);
        data.push(32);
        data.extend_from_slice(&self.signing_public_key);
        Some(data)
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.first() != Some(&AUTH_PEER_STATE_VERSION) {
            return None;
        }
        let mut offset = 1;
        let mut capabilities: Option<PeerCapabilities> = None;
        let mut signing_public_key: Option<[u8; 32]> = None;

        while offset < data.len() {
            if offset + 2 > data.len() {
                return None;
            }
            let type_raw = data[offset];
            let length = data[offset + 1] as usize;
            offset += 2;
            let value = data.get(offset..offset + length)?;
            offset += length;

            match type_raw {
                0x01 => {
                    if capabilities.is_some() || !PeerCapabilities::is_canonical(value) {
                        return None;
                    }
                    capabilities = Some(PeerCapabilities::decode(value));
                }
                0x02 => {
                    if signing_public_key.is_some() {
                        return None;
                    }
                    signing_public_key = <[u8; 32]>::try_from(value).ok();
                }
                _ => {} // unknown TLVs skipped
            }
        }

        Some(Self {
            capabilities: capabilities?,
            signing_public_key: signing_public_key?,
        })
    }
}

/// TLV-encoded private message (inside Noise payloads and `bitchat1:`
/// Nostr embeddings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateMessagePacket {
    pub message_id: String,
    pub content: String,
}

impl PrivateMessagePacket {
    pub fn encode(&self) -> Option<Vec<u8>> {
        let id = self.message_id.as_bytes();
        let content = self.content.as_bytes();
        if id.len() > 255 || content.len() > 255 {
            return None;
        }
        let mut data = Vec::with_capacity(4 + id.len() + content.len());
        data.push(0x00);
        data.push(id.len() as u8);
        data.extend_from_slice(id);
        data.push(0x01);
        data.push(content.len() as u8);
        data.extend_from_slice(content);
        Some(data)
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        let mut offset = 0;
        let mut message_id: Option<String> = None;
        let mut content: Option<String> = None;

        while offset + 2 <= data.len() {
            let type_raw = data[offset];
            let length = data[offset + 1] as usize;
            offset += 2;
            let value = data.get(offset..offset + length)?;
            offset += length;

            match type_raw {
                0x00 => message_id = String::from_utf8(value.to_vec()).ok(),
                0x01 => content = String::from_utf8(value.to_vec()).ok(),
                _ => return None, // strict: upstream rejects unknown TLVs here
            }
        }

        Some(Self { message_id: message_id?, content: content? })
    }
}

// TODO(M1): ReadReceipt/RequestSync/file packet TLVs and the rest of
// bitchat/Protocols land with their milestones. See PORTING.md.

#[cfg(test)]
mod tests {
    use super::*;

    fn announce() -> AnnouncementPacket {
        AnnouncementPacket {
            nickname: "tester".into(),
            noise_public_key: [1u8; 32],
            signing_public_key: [2u8; 32],
            direct_neighbors: Some(vec![[3u8; 8]]),
            capabilities: Some(PeerCapabilities::PREKEYS | PeerCapabilities::BRIDGE),
            bridge_geohash: Some("dr5rs".into()),
        }
    }

    #[test]
    fn announce_roundtrip() {
        let bytes = announce().encode().unwrap();
        assert_eq!(AnnouncementPacket::decode(&bytes), Some(announce()));
    }

    #[test]
    fn announce_skips_unknown_tlv() {
        let mut bytes = announce().encode().unwrap();
        // splice an unknown TLV (0x7F, 2 bytes, [0xAA, 0xBB]) after the first
        // one (nickname = 0x01, len byte + "tester" = 8 bytes in)
        bytes.splice(8..8, [0x7F, 0x02, 0xAA, 0xBB]);
        let decoded = AnnouncementPacket::decode(&bytes).unwrap();
        assert_eq!(decoded.nickname, "tester");
    }

    #[test]
    fn auth_peer_state_rejects_non_canonical() {
        let pkt = AuthenticatedPeerStatePacket {
            capabilities: PeerCapabilities::PREKEYS,
            signing_public_key: [2u8; 32],
        };
        let bytes = pkt.encode().unwrap();
        assert_eq!(AuthenticatedPeerStatePacket::decode(&bytes), Some(pkt));
        // trailing-zero capabilities byte is non-canonical -> rejected
        let mut bad = bytes.clone();
        bad.splice(2..2, [0x01, 0x00]);
        assert_eq!(AuthenticatedPeerStatePacket::decode(&bad), None);
    }

    #[test]
    fn private_message_roundtrip() {
        let pkt = PrivateMessagePacket {
            message_id: "abc123".into(),
            content: "hello, mesh".into(),
        };
        let bytes = pkt.encode().unwrap();
        assert_eq!(PrivateMessagePacket::decode(&bytes), Some(pkt));
    }
}
