//! bitchat-ble: the Bluetooth LE mesh transport for Linux.
//!
//! Upstream sources of truth (see PORTING.md):
//! - GATT service UUIDs — mainnet `F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C`,
//!   testnet `F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5A` (`BLEService.swift`)
//! - characteristic `A1B2C3D4-E5F6-4A5B-8C9D-0E1F2A3B4C5D`, 512 B link MTU
//! - fragment chunking ~469 B (`TransportConfig.bleDefaultFragmentSize`),
//!   fragments carry the *full original packet bytes* (`BLEFragmentHandler`
//!   re-decodes them), keyed by (sender, 8-byte fragment id)
//! - dedup: SHA-256(type ‖ sender ‖ timestamp ‖ payload), LRU 1000 / 5 min
//!   (`PacketIdUtil.computeId` + `PacketDeduplicator`)
//! - Noise sessions: `bitchat-core::noise_session` (wire-exact XX with the
//!   deployed extracted-nonce transport frame)
//!
//! Architecture mirrors upstream's split: [`engine`] is the pure packet
//! pipeline (dedup, TTL relay, fragments, sessions, registry — no I/O,
//! fully unit-testable with virtual packet flows), [`link`] is the bluer
//! radio (peripheral advertise + GATT *and* central scan + connect on one
//! adapter), and [`node`] glues them together with channels for the app.

pub mod dedup;
pub mod engine;
pub mod fragment;
pub mod link;
pub mod node;

use uuid::Uuid;

/// Mainnet GATT service UUID (`BLEService.swift`).
pub const SERVICE_UUID: Uuid =
    Uuid::from_u128(0xF47B_5E2D_4A9E_4C5A_9B3F_8E1D_2C3A_4B5C);
/// Testnet GATT service UUID — iOS DEBUG builds advertise this.
pub const SERVICE_UUID_TESTNET: Uuid =
    Uuid::from_u128(0xF47B_5E2D_4A9E_4C5A_9B3F_8E1D_2C3A_4B5A);
/// The one GATT characteristic carrying all framed packets.
pub const CHARACTERISTIC_UUID: Uuid =
    Uuid::from_u128(0xA1B2_C3D4_E5F6_4A5B_8C9D_0E1F_2A3B_4C5D);

/// Upstream `bleMaxMTU`.
pub const BLE_MAX_MTU: usize = 512;
/// Upstream `bleDefaultFragmentSize`: payload chunk inside fragment packets.
pub const BLE_DEFAULT_FRAGMENT_SIZE: usize = 469;
/// Max ATT write on a 512 MTU link (MTU minus 3 ATT header bytes).
pub const BLE_MAX_WRITE: usize = BLE_MAX_MTU - 3;
/// Default mesh TTL (`messageTTLDefault`).
pub const DEFAULT_TTL: u8 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshNetwork {
    Mainnet,
    Testnet,
}

impl MeshNetwork {
    pub fn service_uuid(self) -> Uuid {
        match self {
            MeshNetwork::Mainnet => SERVICE_UUID,
            MeshNetwork::Testnet => SERVICE_UUID_TESTNET,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MeshConfig {
    pub nickname: String,
    pub network: MeshNetwork,
    /// Milliseconds between automatic announces (upstream throttles harder).
    pub announce_interval_ms: u64,
    /// Peer considered gone after this long without an announce.
    pub peer_expiry_ms: u64,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            nickname: "anon".to_string(),
            network: MeshNetwork::Mainnet,
            announce_interval_ms: 15_000,
            peer_expiry_ms: 360_000,
        }
    }
}
