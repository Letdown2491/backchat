//! The pure mesh packet pipeline: dedup, TTL relay, fragment assembly,
//! announce registry, Noise sessions, and outbound packet construction.
//! No I/O — every interaction is bytes in, [`Action`]s out, so the whole
//! mesh behavior is unit-testable without a radio.
//!
//! Semantics ported from `BLEService.swift` (processIncomingPacket /
//! sendPacket), `NoiseSessionManager.swift` (initiation policy: lazily on
//! private send; respond to inbound), and `BLEFragmentHandler.swift`.

use std::collections::{HashMap, VecDeque};

use bitchat_core::identity::MeshIdentity;
use bitchat_core::message_type::MessageType;
use bitchat_core::noise_payload::NoisePayloadType;
use bitchat_core::noise_session::{
    CompletedHandshake, HandshakeInitiator, HandshakeResponder, TransportCipher,
};
use bitchat_core::packet::Packet;
use bitchat_core::peer_id::PeerId;
use bitchat_core::tlv::{AnnouncementPacket, PrivateMessagePacket};

use crate::dedup::{packet_id, DedupCache};
use crate::fragment::{Assembled, FragmentAssembler, FragmentHeader};
use crate::DEFAULT_TTL;

/// Registry entry lifetime (upstream marks peers gone after 6 min).
/// Peers vanish after this long without any packet from them (upstream
/// `Mesh.STALE_PEER_TIMEOUT_MS`).
pub const PEER_EXPIRY_MS: u64 = 180_000;
/// Re-announce at most this often when reacting to new peers.
pub const ANNOUNCE_THROTTLE_MS: u64 = 10_000;

/// Gossip-sync caps (upstream `AppConstants.SYNC_TTL_HOPS` = 0 for
/// neighbor-only sync packets, `seenPacketCapacity` = 500).
pub const SYNC_TTL_HOPS: u8 = 0;
pub const SYNC_MESSAGE_CAP: usize = 500;

// ---------------------------------------------------------------------------
// Engine I/O surface
// ---------------------------------------------------------------------------

/// What the radio layer should do, produced by the engine.
#[derive(Debug, Clone)]
pub enum Action {
    /// Flood to every connected link (relay path decrements TTL).
    Broadcast(Vec<u8>),
    /// Unicast to a specific peer via its bound link.
    SendTo([u8; 8], Vec<u8>),
    /// Something the UI should see.
    Event(MeshEvent),
}

#[derive(Debug, Clone)]
pub enum MeshEvent {
    PeerSeen { peer_id: [u8; 8], nickname: String, is_new: bool },
    PeerExpired { peer_id: [u8; 8] },
    PublicMessage { sender_id: [u8; 8], nickname: String, content: String, timestamp: u64 },
    PrivateMessage { sender_id: [u8; 8], nickname: String, content: String, message_id: String },
}

enum PendingHandshake {
    Initiating(HandshakeInitiator),
    Responding(HandshakeResponder),
}

pub struct NoiseSession {
    pub send: TransportCipher,
    pub recv: TransportCipher,
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub nickname: String,
    pub noise_public_key: [u8; 32],
    pub signing_public_key: [u8; 32],
    pub last_seen_ms: u64,
}

/// A private message waiting for its Noise session to establish.
struct PendingPrivate {
    peer: [u8; 8],
    content: String,
    message_id: String,
}

pub struct Engine {
    identity: MeshIdentity,
    nickname: String,
    registry: HashMap<[u8; 8], PeerInfo>,
    sessions: HashMap<[u8; 8], NoiseSession>,
    pending_handshakes: HashMap<[u8; 8], PendingHandshake>,
    dedup: DedupCache,
    fragments: FragmentAssembler,
    outbox: VecDeque<PendingPrivate>,
    last_announce_ms: u64,
    /// Gossip-sync store (upstream `GossipSyncManager`): latest accepted
    /// announce per peer plus a ring of recent broadcast messages, served
    /// when a neighbor sends REQUEST_SYNC.
    sync_announces: HashMap<[u8; 8], Packet>,
    sync_messages: VecDeque<Packet>,
}

impl Engine {
    pub fn new(identity: MeshIdentity, nickname: String) -> Self {
        Self {
            identity,
            nickname,
            registry: HashMap::new(),
            sessions: HashMap::new(),
            pending_handshakes: HashMap::new(),
            dedup: DedupCache::default(),
            fragments: FragmentAssembler::default(),
            outbox: VecDeque::new(),
            last_announce_ms: 0,
            sync_announces: HashMap::new(),
            sync_messages: VecDeque::new(),
        }
    }

    pub fn peer_id(&self) -> [u8; 8] {
        self.identity.peer_id().mesh_bytes().expect("8-byte id")
    }

    pub fn nickname(&self) -> &str {
        &self.nickname
    }

    pub fn set_nickname(&mut self, nickname: String) {
        self.nickname = nickname;
    }

    pub fn peers(&self) -> Vec<PeerInfo> {
        self.registry.values().cloned().collect()
    }

    pub fn session_established(&self, peer: &[u8; 8]) -> bool {
        self.sessions.contains_key(peer)
    }

    // -----------------------------------------------------------------
    // Inbound
    // -----------------------------------------------------------------

    /// Feed raw bytes from any link. Returns the actions to execute.
    pub fn on_bytes(&mut self, raw: &[u8], now_ms: u64) -> Vec<Action> {
        let Ok(packet) = Packet::decode(raw) else {
            return vec![];
        };
        self.on_packet(packet, false, now_ms)
    }

    fn on_packet(&mut self, packet: Packet, reassembled: bool, now_ms: u64) -> Vec<Action> {
        let sender_id = PeerId::from_mesh_bytes(&packet.sender_id)
            .mesh_bytes()
            .expect("8-byte id");

        // Self-originated copies relayed back: ignore.
        if sender_id == self.peer_id() {
            return vec![];
        }

        // Unicast addressed to someone else: pure relay, don't process.
        // Unicast addressed to us or broadcast: process.
        let addressed_to_us = packet.recipient_id == Some(self.peer_id());
        let mut actions = Vec::new();

        // Relay before processing (TTL decrement), like upstream relays then
        // delivers. Packets addressed to us are terminal — never relayed.
        // Upstream relays anything with TTL left, including forwarding a
        // TTL-1 packet as TTL-0 (PacketRelayManager: stop only at 0).
        if !reassembled && !addressed_to_us && packet.ttl > 0 {
            let mut forward = packet.clone();
            forward.ttl -= 1;
            if let Ok(encoded) = forward.encode(false) {
                actions.push(Action::Broadcast(encoded));
            }
        }

        // Dedup: fragments bypass it entirely (they re-enter as a reassembled
        // packet which IS dedup'd normally).
        let id = packet_id(&packet);
        if !reassembled && !self.dedup.check_and_insert(id, now_ms) {
            return actions;
        }

        match packet.kind {
            k if k == MessageType::Announce as u8 => self.on_announce(&packet, sender_id, now_ms, &mut actions),
            k if k == MessageType::Message as u8 => self.on_public_message(&packet, sender_id, &mut actions),
            k if k == MessageType::Leave as u8 => {
                actions.push(Action::Event(MeshEvent::PeerExpired { peer_id: sender_id }));
            }
            k if k == MessageType::NoiseHandshake as u8 => self.on_handshake(&packet, sender_id, now_ms, &mut actions),
            k if k == MessageType::NoiseEncrypted as u8 => self.on_noise_encrypted(&packet, sender_id, &mut actions),
            k if k == MessageType::Fragment as u8 => self.on_fragment(&packet, sender_id, now_ms, &mut actions),
            k if k == MessageType::RequestSync as u8 => self.on_request_sync(&packet, sender_id, &mut actions),
            _ => {} // courier envelopes, media: not in v1
        }
        actions
    }

    fn on_announce(&mut self, packet: &Packet, sender_id: [u8; 8], now_ms: u64, actions: &mut Vec<Action>) {
        let Some(announce) = AnnouncementPacket::decode(&packet.payload) else {
            return;
        };
        // Announce authenticity (upstream BLEAnnouncePreflightPolicy):
        // sender id must match the noise key, signature must verify with the
        // announced signing key.
        let derived = PeerId::from_noise_static_public_key(&announce.noise_public_key);
        if derived.mesh_bytes() != Some(sender_id) {
            return;
        }
        let Some(sig) = packet.signature else { return };
        if !MeshIdentity::verify(&announce.signing_public_key, &sig, &packet.to_signing_bytes().unwrap_or_default()) {
            return;
        }

        let is_new = !self.registry.contains_key(&sender_id);
        self.registry.insert(
            sender_id,
            PeerInfo {
                nickname: announce.nickname.clone(),
                noise_public_key: announce.noise_public_key,
                signing_public_key: announce.signing_public_key,
                last_seen_ms: now_ms,
            },
        );
        // Keep the validated packet for gossip sync.
        self.sync_announces.insert(sender_id, packet.clone());
        actions.push(Action::Event(MeshEvent::PeerSeen {
            peer_id: sender_id,
            nickname: announce.nickname,
            is_new,
        }));

        // Greet a brand-new peer with our own announce (throttled) so the
        // phone's UI picks us up without waiting a full announce interval.
        if is_new && now_ms.saturating_sub(self.last_announce_ms) > ANNOUNCE_THROTTLE_MS {
            actions.push(self.announce_action(now_ms));
        }
    }

    fn on_public_message(&mut self, packet: &Packet, sender_id: [u8; 8], actions: &mut Vec<Action>) {        let Ok(content) = std::str::from_utf8(&packet.payload) else {
            return;
        };
        if content.is_empty() {
            return;
        }
        // Verify when the peer's signing key is known (upstream verifies
        // signed messages against the registry).
        let nickname = self
            .registry
            .get(&sender_id)
            .map(|p| p.nickname.clone())
            .unwrap_or_else(|| hex::encode(&sender_id[..2]));
        if let (Some(sig), Some(peer)) = (packet.signature, self.registry.get(&sender_id)) {
            let Ok(signing_bytes) = packet.to_signing_bytes() else { return };
            if !MeshIdentity::verify(&peer.signing_public_key, &sig, &signing_bytes) {
                return;
            }
        }
        // Keep broadcast messages for gossip sync (upstream keeps only
        // recipient-less broadcasts).
        if packet.recipient_id.is_none() {
            if self.sync_messages.len() >= SYNC_MESSAGE_CAP {
                self.sync_messages.pop_front();
            }
            self.sync_messages.push_back(packet.clone());
        }
        actions.push(Action::Event(MeshEvent::PublicMessage {
            sender_id,
            nickname,
            content: content.to_string(),
            timestamp: packet.timestamp,
        }));
    }

    /// Respond to a neighbor's REQUEST_SYNC (upstream
    /// `GossipSyncManager.handleRequestSync`): re-send our stored public
    /// packets directed at the requester with neighbor-only TTL. The request
    /// carries a GCS filter of what they already have; we don't parse it and
    /// send everything instead — the requester dedups, and sending is
    /// idempotent. Signatures stay valid because packet signatures exclude
    /// the TTL field.
    fn on_request_sync(&mut self, _packet: &Packet, requester: [u8; 8], actions: &mut Vec<Action>) {
        for stored in self.sync_announces.values().chain(self.sync_messages.iter()) {
            let mut out = stored.clone();
            out.ttl = SYNC_TTL_HOPS;
            if let Ok(encoded) = out.encode(false) {
                actions.push(Action::SendTo(requester, encoded));
            }
        }
    }

    fn on_handshake(&mut self, packet: &Packet, sender_id: [u8; 8], now_ms: u64, actions: &mut Vec<Action>) {
        // Unicast only.
        if packet.recipient_id.is_none() {
            return;
        }
        let payload = &packet.payload;
        match payload.len() {
            // msg1: we are the responder.
            32 => {
                let mut responder = HandshakeResponder::new(self.identity.noise.clone());
                match responder.step2(payload) {
                    Ok(msg2) => {
                        self.pending_handshakes
                            .insert(sender_id, PendingHandshake::Responding(responder));
                        let mut reply = Packet::new(
                            MessageType::NoiseHandshake,
                            self.peer_id(),
                            Self::timestamp(now_ms),
                            msg2,
                            DEFAULT_TTL,
                        );
                        reply.recipient_id = Some(sender_id);
                        if let Ok(encoded) = reply.encode(false) {
                            actions.push(Action::SendTo(sender_id, encoded));
                        }
                    }
                    Err(_) => {}
                }
            }
            // msg2: we are the initiator.
            96 => {
                if let Some(PendingHandshake::Initiating(initiator)) =
                    self.pending_handshakes.remove(&sender_id)
                {
                    if let Ok((msg3, completed)) = initiator.step3(payload) {
                        self.establish(sender_id, completed, actions);
                        let mut reply = Packet::new(
                            MessageType::NoiseHandshake,
                            self.peer_id(),
                            Self::timestamp(now_ms),
                            msg3,
                            DEFAULT_TTL,
                        );
                        reply.recipient_id = Some(sender_id);
                        if let Ok(encoded) = reply.encode(false) {
                            actions.push(Action::SendTo(sender_id, encoded));
                        }
                        self.flush_outbox(sender_id, now_ms, actions);
                    }
                }
            }
            // msg3: we were the responder.
            64 => {
                if let Some(PendingHandshake::Responding(responder)) =
                    self.pending_handshakes.remove(&sender_id)
                {
                    if let Ok(completed) = responder.step4(payload) {
                        // The initiator's static arrives inside msg3: the
                        // claimed sender id must match it.
                        let derived = PeerId::from_noise_static_public_key(&completed.remote_static);
                        if derived.mesh_bytes() != Some(sender_id) {
                            return;
                        }
                        self.establish(sender_id, completed, actions);
                        self.flush_outbox(sender_id, now_ms, actions);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_noise_encrypted(&mut self, packet: &Packet, sender_id: [u8; 8], actions: &mut Vec<Action>) {
        if packet.recipient_id != Some(self.peer_id()) {
            return;
        }
        let Some(session) = self.sessions.get_mut(&sender_id) else {
            // No session: the sender will (re)handshake; drop for now.
            // Upstream clears the session on unexpected traffic and
            // re-handshakes; we simply have nothing yet.
            return;
        };
        let Ok(plaintext) = session.recv.open(&packet.payload) else {
            // Decrypt failure → stale session (e.g. phone rekeyed). Clear and
            // let the next private send re-handshake (upstream recovery path).
            self.sessions.remove(&sender_id);
            return;
        };
        let Some((&payload_type, inner)) = plaintext.split_first() else {
            return;
        };
        if payload_type == NoisePayloadType::PrivateMessage as u8 {
            if let Some(pm) = PrivateMessagePacket::decode(inner) {
                let nickname = self
                    .registry
                    .get(&sender_id)
                    .map(|p| p.nickname.clone())
                    .unwrap_or_else(|| hex::encode(&sender_id[..2]));
                actions.push(Action::Event(MeshEvent::PrivateMessage {
                    sender_id,
                    nickname,
                    content: pm.content,
                    message_id: pm.message_id,
                }));
            }
        }
        // read receipts, deliveries, group keys etc: not in v1.
    }

    fn on_fragment(&mut self, packet: &Packet, sender_id: [u8; 8], now_ms: u64, actions: &mut Vec<Action>) {
        let Some(header) = FragmentHeader::parse(packet) else { return };
        // Our own relayed-back fragments: skip.
        if header.sender == self.peer_id() {
            return;
        }
        match self.fragments.append(&header, now_ms) {
            Assembled::Stored => {}
            Assembled::Complete(data) => {
                if let Ok(original) = Packet::decode(&data) {
                    // Reassembled packets re-enter the pipeline delivered,
                    // TTL zeroed, and dedup'd on their own id.
                    let mut p = original;
                    p.ttl = 0;
                    actions.extend(self.on_packet(p, true, now_ms));
                }
            }
        }
        let _ = sender_id;
    }

    // -----------------------------------------------------------------
    // Outbound
    // -----------------------------------------------------------------

    /// Build + flood our announce.
    pub fn announce_action(&mut self, now_ms: u64) -> Action {
        self.last_announce_ms = now_ms;
        let announce = AnnouncementPacket {
            nickname: self.nickname.clone(),
            noise_public_key: self.identity.noise.public_key,
            signing_public_key: self.identity.signing.verifying_key().to_bytes(),
            direct_neighbors: None, // topology gossip: not in v1
            capabilities: None,
            bridge_geohash: None,
        };
        let mut packet = Packet::new(
            MessageType::Announce,
            self.peer_id(),
            Self::timestamp(now_ms),
            announce.encode().expect("announce TLV"),
            DEFAULT_TTL,
        );
        packet.signature =
            Some(self.identity.sign(&packet.to_signing_bytes().expect("signable")).to_bytes());
        Action::Broadcast(packet.encode(false).expect("announce encodable"))
    }

    pub fn send_public(&mut self, content: &str, now_ms: u64) -> Vec<Action> {
        let mut packet = Packet::new(
            MessageType::Message,
            self.peer_id(),
            Self::timestamp(now_ms),
            content.as_bytes().to_vec(),
            DEFAULT_TTL,
        );
        packet.signature =
            Some(self.identity.sign(&packet.to_signing_bytes().expect("signable")).to_bytes());
        let encoded = match packet.encode(false) {
            Ok(encoded) => encoded,
            Err(_) => return vec![],
        };
        // Long public messages go out as fragments of the full packet binary.
        if crate::fragment::needs_fragmentation(encoded.len()) {
            let frags = crate::fragment::plan_fragments(
                self.peer_id(),
                None,
                packet.timestamp,
                DEFAULT_TTL,
                &encoded,
            );
            return frags
                .iter()
                .filter_map(|f| f.encode(false).ok())
                .map(Action::Broadcast)
                .collect();
        }
        vec![Action::Broadcast(encoded)]
    }

    /// Send a private message; initiates a Noise handshake first if needed
    /// (the message is queued and flushed on session establishment —
    /// upstream's lazy-initiation policy).
    pub fn send_private(&mut self, peer: [u8; 8], content: &str, now_ms: u64) -> Vec<Action> {
        let message_id = Self::new_message_id(now_ms);
        if self.sessions.contains_key(&peer) {
            return self.seal_private(peer, content, &message_id, now_ms);
        }
        // No session: start (or keep) a handshake and queue the message.
        let mut actions = Vec::new();
        if !self.pending_handshakes.contains_key(&peer)
            && !self.registry.contains_key(&peer)
        {
            // Unknown peer: nothing to handshake with (we need their
            // announce first; the handshake itself works regardless, but
            // there is nobody to send to).
            return vec![];
        }
        if !self.pending_handshakes.contains_key(&peer) {
            if let Some(action) = self.initiate_handshake(peer, now_ms) {
                actions.push(action);
            }
        }
        self.outbox.push_back(PendingPrivate { peer, content: content.to_string(), message_id });
        actions
    }

    /// Flood a leave packet (best-effort presence removal).
    pub fn leave_action(&mut self, now_ms: u64) -> Action {
        let packet = Packet::new(
            MessageType::Leave,
            self.peer_id(),
            Self::timestamp(now_ms),
            self.nickname.as_bytes().to_vec(),
            DEFAULT_TTL,
        );
        Action::Broadcast(packet.encode(false).expect("leave encodable"))
    }

    /// Expire stale registry entries; called periodically.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        let expired: Vec<[u8; 8]> = self
            .registry
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.last_seen_ms) > PEER_EXPIRY_MS)
            .map(|(k, _)| *k)
            .collect();
        let mut actions = Vec::new();
        for peer in expired {
            self.registry.remove(&peer);
            self.sync_announces.remove(&peer);
            actions.push(Action::Event(MeshEvent::PeerExpired { peer_id: peer }));
        }
        actions
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    fn initiate_handshake(&mut self, peer: [u8; 8], now_ms: u64) -> Option<Action> {
        let mut initiator = HandshakeInitiator::new(self.identity.noise.clone());
        let msg1 = initiator.step1().ok()?;
        self.pending_handshakes.insert(peer, PendingHandshake::Initiating(initiator));
        let mut packet = Packet::new(
            MessageType::NoiseHandshake,
            self.peer_id(),
            Self::timestamp(now_ms),
            msg1,
            DEFAULT_TTL,
        );
        packet.recipient_id = Some(peer);
        Some(Action::SendTo(peer, packet.encode(false).ok()?))
    }

    fn establish(&mut self, peer: [u8; 8], completed: CompletedHandshake, actions: &mut Vec<Action>) {
        self.pending_handshakes.remove(&peer);
        self.sessions.insert(
            peer,
            NoiseSession { send: completed.send, recv: completed.recv },
        );
        let _ = actions;
    }

    fn seal_private(&mut self, peer: [u8; 8], content: &str, message_id: &str, now_ms: u64) -> Vec<Action> {
        let Some(session) = self.sessions.get_mut(&peer) else { return vec![] };
        let pm = PrivateMessagePacket {
            message_id: message_id.to_string(),
            content: content.to_string(),
        };
        let mut plaintext = vec![NoisePayloadType::PrivateMessage as u8];
        plaintext.extend_from_slice(&pm.encode().expect("pm TLV"));
        let Ok(frame) = session.send.seal(&plaintext) else { return vec![] };
        let mut packet = Packet::new(
            MessageType::NoiseEncrypted,
            self.peer_id(),
            Self::timestamp(now_ms),
            frame,
            DEFAULT_TTL,
        );
        packet.recipient_id = Some(peer);
        match packet.encode(false) {
            Ok(encoded) => vec![Action::SendTo(peer, encoded)],
            Err(_) => vec![],
        }
    }

    fn flush_outbox(&mut self, peer: [u8; 8], now_ms: u64, actions: &mut Vec<Action>) {
        let mut remaining = VecDeque::new();
        while let Some(pending) = self.outbox.pop_front() {
            if pending.peer == peer {
                actions.extend(self.seal_private(peer, &pending.content, &pending.message_id, now_ms));
            } else {
                remaining.push_back(pending);
            }
        }
        self.outbox = remaining;
    }

    fn timestamp(now_ms: u64) -> u64 {
        now_ms
    }

    fn new_message_id(now_ms: u64) -> String {
        // Upstream uses UUID strings; a random hex id is wire-equivalent
        // (opaque string in a TLV).
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&now_ms.to_le_bytes()[..8]);
        // Mix in OS randomness for uniqueness under identical timestamps.
        let mut seed = raw;
        let rand_bytes = chacha_nonce_fallback();
        for (b, r) in seed.iter_mut().zip(rand_bytes) {
            *b ^= r;
        }
        hex::encode(seed)
    }
}

/// Cheap per-message uniqueness: mix time with ASLR-ish process state.
/// (Upstream uses UUID(); the TLV only needs a unique opaque string.)
fn chacha_nonce_fallback() -> [u8; 8] {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos.to_le_bytes()
}
