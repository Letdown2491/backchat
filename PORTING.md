# PORTING.md — tracking upstream bitchat

Upstream: https://github.com/permissionlesstech/bitchat (Swift/SwiftUI, iOS/macOS)
Snapshot this document was written against: `5e9287f`, 2026-09-24
Whitepaper: `WHITEPAPER.md` v2.0 (2026-07-06)

bitchat-rs is a Rust/GPUI reimplementation targeting Linux, not a code fork.
Upstream commits are never merged; the upstream remote exists only for
diffing. When upstream changes, this file decides whether we care.

## The rule

Every upstream file is one of two things:

1. **Wire-behavior** — defines bytes that must match what iOS/Android/other
   clients put on the air. Changes here require porting.
2. **Platform plumbing** — Swift/CoreBluetooth/Keychain/SwiftUI concerns.
   Changes here are ignored unless they reveal a protocol assumption.

Section 2 files are only ever looked at when a section 1 file or a test
changes, to figure out *why*.

## Porting procedure (when an upstream release lands)

1. `git fetch upstream` and diff `main` against the snapshot hash below.
2. For every touched file in the **Wire-behavior map** below:
   a. Update `protocol/` notes (our mirror of the relevant spec section).
   b. Port changed/new test vectors into `fixtures/` (JSON, hex strings).
   c. Make `bitchat-core` tests pass against the new fixtures.
3. Diff `WHITEPAPER.md` — spec changes without code changes are still
   wire changes.
4. Bump the snapshot hash in this file.
5. If a *transport semantic* changed (TTL clamps, jitter ranges, dedup
   windows, fanout rules), re-check `bitchat-core::mesh` — these are
   interop-relevant even though no byte format changes.

## Wire-behavior map

Upstream path → what it defines → where it lands in bitchat-rs → fixtures.

### localPackages/BitFoundation — the wire-format core (highest value)

| Upstream file | Defines | bitchat-rs module | Fixtures |
|---|---|---|---|
| `BinaryProtocol.swift` | packet header v1 (14 B) / v2 (16 B), flag bits, big-endian fields, zlib compression bit + original-size preamble, v2 source route encoding | `core::packet` | `BinaryProtocolTests`, `BinaryProtocolPaddingTests`, `PacketPayloadLimitsTests` |
| `BitchatPacket.swift` | packet struct; signing representation (TTL=0, no signature, no RSR flag) | `core::packet` | — |
| `MessageType.swift` | message type table (0x01–0x2C, incl. gaps: 0x05 recycled history) | `core::message_type` | — |
| `MessagePadding.swift` | noise frames only, PKCS#7-style, buckets 256/512/1024/2048, unpadded if >255 pad bytes | `core::padding` | `BinaryProtocolPaddingTests` |
| `CompressionUtil.swift` | zlib "should compress > 256 bytes" policy | `core::compression` | — |
| `PeerID.swift` | prefixed hex peer IDs (`mesh:`, `noise:`, `nostr:`, `nostr_`, `group_`, `bridge:`), 8-byte mesh IDs = first 8 bytes of SHA-256 fingerprint of Noise static key | `core::peer_id` | `PeerIDTests` |
| `PeerCapabilities.swift` | capability bit flags, canonical minimal encoding | `core::capabilities` | `PeerCapabilitiesTests` |
| `PacketPayloadLimits.swift` | per-type payload ceilings, max deflate ratio | `core::limits` | `PacketPayloadLimitsTests` |
| `CourierEnvelope.swift` | sealed store-and-forward envelope, HMAC day tag, copy budget | `core::courier` (M3) | `CourierEnvelopeTests`, `CourierVectorTests` |
| `AnnounceV2Packet.swift` | identity-free presence (epoch + recognition tags), type 0x2C | `core::announce_v2` (later) | `AnnounceV2PacketTests` |
| `MeshPingPayload.swift`, `PrekeyBundle.swift`, `FileTransferLimits.swift` | misc wire types | `core::` (later) | respective tests |

### bitchat/Protocols

| Upstream file | Defines | bitchat-rs module | Fixtures |
|---|---|---|---|
| `BitchatProtocol.swift` | `NoisePayloadType` table (first decrypted payload byte): 0x01 pm, 0x02 read receipt, 0x03 delivered, 0x06/0x07 group, 0x08 voice, 0x10/0x11 verify, 0x12 vouch, 0x20 privateFile (0x09 legacy alias: decode-only, never emit), 0x21 authenticatedPeerState | `core::noise_payload` | — |
| `Packets.swift` | Announcement TLVs (0x01 nickname, 0x02 noise key, 0x03 signing key, 0x04 ≤10 neighbors, 0x05 capabilities, 0x06 bridge geohash; unknown TLVs skipped), `AuthenticatedPeerStatePacket` v1 (strict: rejects non-canonical), `PrivateMessagePacket` TLVs (0x00 id, 0x01 content) | `core::tlv` | `PacketsTests` |
| `BitchatFilePacket.swift`, `NostrCarrierPacket.swift`, `BoardPackets.swift`, `VouchAttestation.swift`, `VoiceBurstPacket.swift` | later-milestone wire types | M4+ | matching tests in `bitchatTests/Protocols/` |
| `Geohash.swift`, `LocationChannel.swift`, `MeshMessageIdentity.swift` | geohash channel naming/presence | `nostr::geohash` | `LocationChannelsTests` |

### bitchat/Noise

| Upstream file | Defines | bitchat-rs module | Fixtures |
|---|---|---|---|
| `NoiseProtocol.swift` | `Noise_XX_25519_ChaChaPoly_SHA256` handshake, prologue construction, key derivation details | `core::noise` (wraps `snow`) | `NoiseTestVectors.json` (already snow/cacophony format) |
| `NoiseSession.swift`, `NoiseSessionState.swift`, `SecureNoiseSession.swift` | session semantics: rekey, replay windows, nonce handling | `core::noise::session` | `NoiseCoverageTests` |
| `NoiseSessionManager.swift` | session lifecycle, handshake initiation policy | `core::noise::manager` | — |
| `NoiseRateLimiter.swift`, `NoiseSecurity*.swift` | abuse/security rails (behavioral, not wire) | port loosely | — |
| `../Services/NoiseEncryptionService.swift` | glues Noise to mesh; payload padding buckets | `core::noise` + `mesh` | `NoiseEncryptionTests` |

### bitchat/Nostr — internet transport

| Upstream file | Defines | bitchat-rs module | Fixtures |
|---|---|---|---|
| `NostrEmbeddedBitChat.swift` | `bitchat1:` base64url embedding of whole `BitchatPacket`s (type noiseEncrypted, ttl 7); random unlinkable sender IDs for geohash DMs | `nostr::embedded` | — |
| Nostr private envelope (see whitepaper §5.3; kind 13 seal → 14 inner → 1059 envelope, one-time outer key) | `v2:` + base64url(24 B nonce ‖ XChaCha20-Poly1305 ct ‖ 16 B tag); keys from secp256k1 ECDH + HKDF-SHA256 reusing "nip44-v2" info label (NOT NIP-44) | `nostr::envelope` | `XChaCha20Poly1305CompatTests`, `bitchatTests/Nostr/Fixtures` |
| `NostrIdentity.swift`, `NostrIdentityBridge.swift` | secp256k1 keys, npub/nsec, geohash ephemeral identities | `nostr::identity` | — |
| `NostrPoW.swift`, `Base64URLCoding.swift`, `Bech32.swift` | encoding utilities | `nostr::` | respective tests |
| `NostrRelayManager.swift`, `GeoRelayDirectory.swift` | relay selection, subscriptions, 24 h lookback | `nostr::relay` | `GeoRelayDirectoryTests` |

### bitchat/Noise — session layer (M2)

| Upstream file | Defines | bitchat-rs module | Fixtures |
|---|---|---|---|
| `Noise/NoiseProtocol.swift` | Hand-rolled XX: raw 32-B protocol name as `ck`/`h` seed, `mixPreMessageKeys()` → h=SHA256(name), sequential per-key handshake AEAD nonces, transport cipher split (send/recv) | `core::noise_session` | snow cross-checks (`tests/noise_session.rs`, `tests/snow_debug.rs`), `tests/noise_vectors.rs` |
| `Noise/NoiseCipherState.swift` | Extracted-nonce transport frame `BE32(nonce) ‖ ct ‖ tag`, ChaCha nonce `00000000‖LE64(counter)`, 1024-bit sliding replay window, MAX_SEND_NONCE = u32::MAX−1 | `core::noise_session::{TransportCipher, ReplayWindow}` | same |
| `Services/NoiseSessionManager.swift` | Lazy initiation (handshake on first private send), session clearing on decrypt failure → re-handshake | `ble::engine` (v1 skips upstream rekey) | `ble::tests/engine.rs` |

### BLE mesh engine + link (M2)

| Upstream file | Defines | bitchat-rs module |
|---|---|---|
| `Sync/PacketIdUtil.swift`, `Services/BLEPacketDeduplicator`-equivalent | dedup id = SHA-256(type‖sender‖ts BE‖payload), LRU 1000 / 5 min | `ble::dedup` |
| `BLEFragmentAssemblyBuffer.swift`, `BLEFragmentHandler.swift` | fragment payload `[8B id][2B index][2B total][1B orig type][chunk]`, chunks of the FULL original packet binary, (sender,id) keyed assembly | `ble::fragment` |
| `BLEService.swift` (announce builder ~2745–2830, public send ~855–900) | announce TLV + Ed25519 `signPacket` over `toBinaryDataForSigning()`; public message = plain UTF-8 payload | `ble::engine` |
| `bitchat/Protocols/BitchatProtocol.swift:76` | NoisePayloadType byte INSIDE the noise plaintext (`[type][PrivateMessagePacket TLV]`) | `ble::engine::seal_private` |
| `BLEService.swift` (dual role) | peripheral advertise + GATT write-without-response/notify AND central scan/connect on one adapter | `ble::link` (bluer) |

Deliberate v1 deviations (documented in `ble::link`): all outbound frames
flood every link (no per-peer link tables); fanout subsetting, TTL clamps,
and gossip sync are not ported yet.

### Mesh semantics (behavioral, no fixed bytes — but interop-relevant)

| Upstream file | Defines |
|---|---|
| `Services/MessageDeduplicationService.swift`, `Utils/MessageDeduplicator.swift` | LRU 1000 entries, 5-minute expiry, key = sender+timestamp+type+payload digest |
| `Services/BLE/BLEOutboundPacketPolicy.swift`, `BLEFanoutSelector.swift`, `BLERouteForwardingPolicy.swift` | TTL clamps (origin 7; ≥6 links cap 5; ≤2 links full depth), fanout subsetting ~log₂(degree), split horizon |
| `Services/BLE/BLEFragmentHandler.swift`, `BLEFragmentAssemblyBuffer.swift`, `BLEOutboundFragmentPlanner.swift` | ~469-byte fragments, 8-byte fragment ID, 128 assemblies / 30 s / 1 MiB caps |
| `Services/MessageRouter.swift` | transport selection mesh→nostr→courier |
| `Sync/GCSFilter.swift`, `Sync/GossipSyncManager.swift` | 6 h sync window, ~15 s reconciliation |

### BLE link layer (Linux reimplementation, bluer)

| Constant | Value | Source |
|---|---|---|
| GATT service UUID (mainnet) | `F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C` | `BLEService.swift` |
| GATT service UUID (testnet) | `F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5A` | `BLEService.swift` |
| MTU / fragmentation | 512 B MTU, ~469 B fragments | `BitchatProtocol.swift`, whitepaper §4.4 |

Characteristic `A1B2C3D4-E5F6-4A5B-8C9D-0E1F2A3B4C5D` carries all framed
packets in both directions (phone writes → us; we notify → phone).
`ble::link` implements exactly this pairing surface; iOS DEBUG builds
advertise the TESTNET service UUID — pick the network with
`MeshConfig::network` if a phone pairs with nothing.

## Ignored by design (never port)

- `Views/`, `ViewModels/`, `App/` — SwiftUI/GPUI territory
- `Features/voice/`, `Features/media/` — M4+ at the earliest
- `Services/KeychainManager.swift` — Linux uses file-backed keyring
- `localPackages/Arti/` — Tor; consider later, Rust has `arti` crates
- Everything under `bitchatShareExtension/`, `Configs/`, `scripts/`

## Fixture inventory (port first, in this order)

1. `bitchatTests/Noise/NoiseTestVectors.json` — already in snow/cacophony
   JSON format; `snow`'s test harness consumes these nearly as-is.
2. `bitchatTests/Protocols/PacketsTests.swift` — announce/PM TLV round-trips.
3. `localPackages/BitFoundation/Tests/BitFoundationTests/BinaryProtocolTests.swift`
   + `BinaryProtocolPaddingTests.swift` — header/flag/padding golden bytes.
4. `bitchatTests/XChaCha20Poly1305CompatTests.swift` + `bitchatTests/Nostr/Fixtures`
   — the private-envelope scheme.
5. Everything else in `BitFoundationTests` as the modules land.

Fixture format: keep upstream's hex-string JSON shape where possible so a
changed upstream test diffs cleanly against ours.
