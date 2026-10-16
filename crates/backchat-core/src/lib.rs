//! backchat-core: the wire-behavior heart of backchat.
//!
//! Pure, deterministic, no I/O. Everything here is checked against upstream
//! fixtures (see `fixtures/` and `PORTING.md`). If a byte you emit wouldn't
//! be understood by the iOS/Android clients, the fix belongs in this crate.
//!
//! Module map (see PORTING.md for the upstream source of each):
//! - [`packet`]: BinaryProtocol port: header v1/v2, flags, compression, route
//! - [`message_type`]: message type table with its reserved-history rules
//! - [`peer_id`]: prefixed hex peer IDs and stable derivation
//! - [`padding`]: Noise-frame bucket padding
//! - [`capabilities`]: PeerCapabilities bits, canonical encoding
//! - [`limits`]: per-type payload ceilings and deflate ratio checks
//! - [`tlv`]: announce / private message / authenticated peer state TLVs
//! - [`noise`]: Noise_XX_25519_ChaChaPoly_SHA256 sessions (via snow)
//! - [`noise_payload`]: typed payloads inside noiseEncrypted
//! - [`compression`]: zlib policy (compress when payload > 256 bytes)

pub mod capabilities;
pub mod compression;
pub mod identity;
pub mod limits;
pub mod message_type;
pub mod noise;
pub mod noise_payload;
pub mod noise_session;
pub mod packet;
pub mod padding;
pub mod peer_id;
pub mod tlv;

pub use packet::{Packet, PacketDecodeError, PacketEncodeError};
pub use peer_id::PeerId;
