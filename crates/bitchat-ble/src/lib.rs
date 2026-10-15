//! bitchat-ble: the Bluetooth LE mesh transport for Linux.
//!
//! Upstream sources of truth (see PORTING.md):
//! - GATT service UUIDs — mainnet `F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C`,
//!   testnet `F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5A` (`BLEService.swift`)
//! - 512 B link MTU, ~469 B fragments (`BitchatProtocol.swift`, whitepaper §4.4)
//! - Mesh semantics: TTL clamps (origin 7; ≥6 links cap 5; ≤2 links full
//!   depth), dedup (LRU 1000 / 5 min), relay jitter 10–220 ms, fanout
//!   subsetting ~log₂(degree) with split horizon (whitepaper §4.2)
//!
//! On Linux everything goes through BlueZ over D-Bus (`bluer`): the GATT
//! server (`GattManager1`) and LE advertising (`LEAdvertisingManager1`) give
//! us the peripheral role. The flatpak sandbox must grant
//! `--system-talk-name=org.bluez` — the permission is a publish-time
//! decision, see self/ metadata.

// TODO(M2): link (central + peripheral roles), fragment planner/assembly,
// dedup + relay policy, announce loop, connection scheduler. Port upstream
// BLE* semantics; byte-level work lives in bitchat-core.
