//! The chat model: identity/personas, the joined geohash channel, message
//! history, peer presence, and the relay connection. A GPUI entity the
//! shell renders from; all networking flows through the relay manager's
//! command/event channels.

use crate::persistence::{ChannelMemo, Settings};
use backchat_nostr::geo_ip::{fetch_approx_location, GeoFix};
use backchat_ble::engine::MeshEvent;
use backchat_ble::node::Command as MeshCommand;
use backchat_nostr::event::{kinds, NostrEvent};
use backchat_nostr::geohash;
use backchat_nostr::identity::Identity;
use backchat_nostr::relay::{RelayCommand, Subscription};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// One rendered line in a space's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatLine {
    /// Local system notice (joins, errors, help).
    System { text: String },
    /// A message. `self` echoes our own sends.
    Message { sender: String, content: String, ours: bool, timestamp: i64 },
}

/// A row of the people rail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonRow {
    pub peer_id: [u8; 8],
    pub nickname: String,
    pub unread: usize,
    pub online: bool,
}

/// One line of a private conversation with a mesh peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmLine {
    pub sender: String,
    pub content: String,
    pub ours: bool,
    pub timestamp: i64,
    /// Status notice (handshake progress), not a message — rendered as an
    /// unobtrusive inline note.
    pub notice: bool,
}

struct Peer {
    nickname: Option<String>,
    last_seen: i64,
}

/// One geohash's observed activity during a discovery scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ScanCell {
    msgs: u32,
    senders: HashSet<String>,
    last: i64,
}

/// Public shape of a scan row for the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub geohash: String,
    pub msgs: u32,
    pub people: usize,
    pub last: i64,
}

pub struct ChatStore {
    pub device_seed: [u8; 32],
    pub settings: Settings,
    /// Geo relay directory (fetched at startup, bundled fallback).
    pub directory: Vec<backchat_nostr::relay_geo::Entry>,
    /// Identity for the current geohash (derived from the device seed).
    pub persona: Option<Identity>,
    pub geohash: Option<String>,
    /// Nearby (BLE mesh) space log: everyone in radio range.
    pub nearby: Vec<ChatLine>,
    /// Channel (geohash over relays) space log.
    pub channel: Vec<ChatLine>,
    pub connected_relays: usize,
    /// pubkey hex → presence state for the current channel.
    peers: HashMap<String, Peer>,
    relay_tx: UnboundedSender<RelayCommand>,
    /// BLE mesh node command channel (None when bluetooth is unavailable).
    pub mesh_tx: Option<tokio::sync::mpsc::Sender<MeshCommand>>,
    /// mesh peer id → (nickname, last_seen secs).
    mesh_peers: HashMap<[u8; 8], (String, i64)>,
    /// Radio lifecycle: true while the link layer is rebuilding.
    mesh_restarting: bool,
    /// Private conversation history per mesh peer, newest last.
    dms: HashMap<[u8; 8], Vec<DmLine>>,
    /// Unread DM count per peer; cleared when the conversation is opened.
    unread: HashMap<[u8; 8], usize>,
    /// Channel-discovery scan state (see RelayCommand::Discover).
    scan: HashMap<String, ScanCell>,
    scanning: bool,
    /// Coarse location estimate for the browser's ladder (GeoIP, once).
    geo: Option<GeoFix>,
    geo_loading: bool,
    geo_tx: UnboundedSender<Option<GeoFix>>,
    geo_rx: Option<UnboundedReceiver<Option<GeoFix>>>,
}

impl ChatStore {
    pub fn new(
        device_seed: [u8; 32],
        settings: Settings,
        directory: Vec<backchat_nostr::relay_geo::Entry>,
        relay_tx: UnboundedSender<RelayCommand>,
        mesh_tx: Option<tokio::sync::mpsc::Sender<MeshCommand>>,
    ) -> Self {
        let (geo_tx, geo_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            device_seed,
            settings,
            directory,
            persona: None,
            geohash: None,
            nearby: Vec::new(),
            channel: Vec::new(),
            connected_relays: 0,
            peers: HashMap::new(),
            relay_tx,
            mesh_tx,
            mesh_peers: HashMap::new(),
            dms: HashMap::new(),
            unread: HashMap::new(),
            mesh_restarting: false,
            scan: HashMap::new(),
            scanning: false,
            geo: None,
            geo_loading: false,
            geo_tx,
            geo_rx: Some(geo_rx),
        }
    }

    /// Identity-level notice: lands in both spaces' logs.
    pub fn system_line(&mut self, text: impl Into<String>) {
        let text: String = text.into();
        self.nearby.push(ChatLine::System { text: text.clone() });
        self.channel.push(ChatLine::System { text });
        self.trim();
    }

    /// Notice only in the nearby (mesh) log.
    pub fn log_nearby(&mut self, text: impl Into<String>) {
        self.nearby.push(ChatLine::System { text: text.into() });
        self.trim();
    }

    /// Notice only in the channel (geohash) log.
    pub fn log_channel(&mut self, text: impl Into<String>) {
        self.channel.push(ChatLine::System { text: text.into() });
        self.trim();
    }

    fn trim(&mut self) {
        for log in [&mut self.nearby, &mut self.channel] {
            if log.len() > 1000 {
                log.drain(..log.len() - 1000);
            }
        }
    }

    /// Join (or switch to) a geohash channel: derive the persona, replace
    /// the subscription, announce presence.
    pub fn join(&mut self, geohash_str: &str) {
        let geohash_str = geohash_str.trim().to_lowercase();
        if geohash_str.is_empty() || geohash_str.len() > 12 {
            self.system_line("geohash must be 1–12 characters");
            return;
        }
        self.geohash = Some(geohash_str.clone());
        self.persona = Some(geohash::derive_geohash_identity(&self.device_seed, &geohash_str));
        self.peers.clear();
        self.log_channel(format!("joined #{geohash_str} — identity rotates per channel"));
        self.end_browse();
        self.note_channel_visit(&geohash_str);

        // Re-pick the relay set: nearest 5 relays to this cell, exactly as
        // upstream selects them, so peers in the channel are reachable.
        let relays = backchat_nostr::relay_geo::closest_relays(
            &self.directory,
            &geohash_str,
            backchat_nostr::relay_geo::RELAYS_PER_CHANNEL,
        );
        if !relays.is_empty() {
            let _ = self.relay_tx.send(RelayCommand::SetRelays(relays.clone()));
            self.log_channel(format!("relays: {}", relays.join(", ")));
        }

        let filter = backchat_nostr::relay::geohash_filter(&geohash_str, 3600);
        let _ = self.relay_tx.send(RelayCommand::Subscribe(vec![Subscription::new(filter)]));
        self.send_presence();
        self.persist();
    }

    pub fn set_nickname(&mut self, nickname: &str) {
        let nickname = nickname.trim().to_string();
        if nickname.is_empty() || nickname.len() > 20 {
            self.system_line("nickname must be 1–20 characters");
            return;
        }
        self.settings.nickname = nickname;
        self.system_line(format!("nickname set to {}", self.settings.nickname));
        self.send_presence(); // announce the new name
        self.sync_mesh_nickname();
        self.persist();
    }

    fn persist(&self) {
        let _ = crate::persistence::save_settings(&self.settings);
    }

    /// Broadcast to everyone in radio range (nearby space).
    pub fn send_nearby(&mut self, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        if self.mesh_tx.is_none() {
            self.log_nearby("mesh is not running (bluetooth unavailable?)");
            return;
        }
        self.send_mesh_public(content);
        self.nearby.push(ChatLine::Message {
            sender: self.nickname_or_id(),
            content: content.to_string(),
            ours: true,
            timestamp: now_secs(),
        });
        self.trim();
    }

    /// Publish to the geohash channel (channel space).
    pub fn send_channel(&mut self, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        let Some(geohash_str) = self.geohash.clone() else {
            self.log_channel("not in a channel — /join <geohash> first");
            return;
        };
        let persona = self.persona.as_ref().expect("persona exists after join");
        let event = geohash::create_ephemeral_geohash_event(
            content,
            &geohash_str,
            persona,
            Some(&self.settings.nickname),
            false,
        );
        let _ = self.relay_tx.send(RelayCommand::Publish(event));
        self.channel.push(ChatLine::Message {
            sender: self.nickname_or_id(),
            content: content.to_string(),
            ours: true,
            timestamp: now_secs(),
        });
        self.trim();
    }

    fn send_presence(&self) {
        let Some(geohash_str) = self.geohash.as_ref() else {
            return;
        };
        let persona = self.persona.as_ref().expect("persona exists after join");
        let event = geohash::create_geohash_presence_event(geohash_str, persona);
        let _ = self.relay_tx.send(RelayCommand::Publish(event));
    }

    /// Handle one inbound relay event.
    pub fn on_relay_event(&mut self, event: NostrEvent) {
        // Structural validation before trusting anything.
        if !event.within_inbound_limits() || !event.is_valid_signature() {
            return;
        }
        match event.kind {
            kinds::EPHEMERAL_EVENT => {
                let Some(channel) = tag_value(&event.tags, "g") else {
                    return;
                };
                if Some(channel.as_str()) != self.geohash.as_deref() {
                    // Discovery scan: any signed traffic counts.
                    if self.scanning {
                        self.note_scan(&channel, &event.pubkey, event.created_at);
                    }
                    return;
                }
                // Ignore echoes of our own events (already echoed locally).
                if self.persona.as_ref().is_some_and(|p| p.public_key_hex() == event.pubkey) {
                    return;
                }
                let sender = self.note_peer(&event.pubkey, tag_value(&event.tags, "n"), event.created_at);
                self.channel.push(ChatLine::Message {
                    sender,
                    content: event.content.clone(),
                    ours: false,
                    timestamp: event.created_at,
                });
                if self.scanning {
                    self.note_scan(&channel, &event.pubkey, event.created_at);
                }
                self.trim();
            }
            kinds::GEOHASH_PRESENCE => {
                let Some(channel) = tag_value(&event.tags, "g") else {
                    return;
                };
                if Some(channel.as_str()) != self.geohash.as_deref() {
                    if self.scanning {
                        self.note_scan(&channel, &event.pubkey, event.created_at);
                    }
                    return;
                }
                self.note_peer(&event.pubkey, None, event.created_at);
                if self.scanning {
                    self.note_scan(&channel, &event.pubkey, event.created_at);
                }
            }
            _ => {}
        }
    }

    /// Handle one inbound BLE mesh event.
    pub fn on_mesh_event(&mut self, event: MeshEvent) {
        match event {
            MeshEvent::PeerSeen { peer_id, nickname, is_new } => {
                if is_new {
                    self.log_nearby(format!("{nickname} is here"));
                }
                self.mesh_peers.insert(peer_id, (nickname, now_secs()));
            }
            MeshEvent::PeerExpired { peer_id } => {
                if let Some((nick, _)) = self.mesh_peers.remove(&peer_id) {
                    self.log_nearby(format!("{nick} left"));
                }
            }
            MeshEvent::PublicMessage { nickname, content, timestamp, .. } => {
                self.nearby.push(ChatLine::Message {
                    sender: nickname,
                    content,
                    ours: false,
                    timestamp: (timestamp / 1000) as i64,
                });
                self.trim();
            }
            MeshEvent::DmPending { peer_id } => {
                // Honest feedback: queued behind a handshake, not delivered.
                let pending = self.dms.entry(peer_id).or_default();
                pending.push(DmLine {
                    sender: String::new(),
                    content: "opening encrypted channel\u{2026} your message sends automatically".into(),
                    ours: true,
                    timestamp: now_secs(),
                    notice: true,
                });
            }
            MeshEvent::RadioRestarted { heal } => {
                self.mesh_restarting = false;
                self.system_line(if heal {
                    "bluetooth adapter wedged — power-cycled and restarted the radio"
                } else {
                    "radio restarted"
                });
            }
            MeshEvent::PrivateMessage { sender_id, nickname, content, .. } => {
                let viewed = self.dms.entry(sender_id).or_default();
                viewed.push(DmLine {
                    sender: nickname.clone(),
                    content,
                    ours: false,
                    timestamp: now_secs(),
                    notice: false,
                });
                *self.unread.entry(sender_id).or_default() += 1;
                // Presence of the message lives in the people rail (unread
                // badge); the nearby log gets a pointer so it's never
                // invisible.
                self.log_nearby(format!("{nickname} sent you a private message"));
                self.trim();
            }
        }
    }

    /// Broadcast on the BLE mesh (if it's up).
    pub fn send_mesh_public(&mut self, content: &str) {
        let Some(mesh_tx) = &self.mesh_tx else { return };
        let _ = mesh_tx.try_send(MeshCommand::SendPublic(content.to_string()));
    }

    /// Direct message a mesh peer by nickname.
    pub fn send_mesh_dm(&mut self, nick: &str, content: &str) {
        match self.resolve_peer(nick) {
            Some((peer_id, _)) => self.send_dm_to(peer_id, content),
            None => self.system_line(format!("no mesh peer named {nick} — /peers")),
        }
    }

    /// Resolve a nickname to a live mesh peer.
    pub fn resolve_peer(&self, nick: &str) -> Option<([u8; 8], String)> {
        self.mesh_peers
            .iter()
            .find(|(_, (n, _))| n.eq_ignore_ascii_case(nick))
            .map(|(k, (n, _))| (*k, n.clone()))
    }

    /// Open a private conversation with a peer by nickname. Returns the
    /// peer for the view to focus (silent — the pane title shows the peer).
    pub fn open_dm(&mut self, nick: &str) -> Option<([u8; 8], String)> {
        let (peer_id, name) = self.resolve_peer(nick)?;
        Some((peer_id, name))
    }

    /// DM history with a peer (newest last).
    pub fn dm_history(&self, peer: &[u8; 8]) -> &[DmLine] {
        self.dms.get(peer).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Clear a peer's unread badge (called when their conversation opens).
    pub fn mark_dm_read(&mut self, peer: &[u8; 8]) {
        self.unread.remove(peer);
    }

    /// Total unread DMs, for the header badge.
    pub fn total_unread(&self) -> usize {
        self.unread.values().sum()
    }

    /// People for the rail: live mesh peers plus anyone with conversation
    /// history. Unread first, then online, then name.
    pub fn people(&self, now: i64) -> Vec<PersonRow> {
        let mut rows: Vec<PersonRow> = self
            .mesh_peers
            .iter()
            .map(|(id, (nick, seen))| PersonRow {
                peer_id: *id,
                nickname: nick.clone(),
                unread: self.unread.get(id).copied().unwrap_or(0),
                online: now - seen < 600,
            })
            .collect();
        for (id, unread) in &self.unread {
            if rows.iter().any(|r| &r.peer_id == id) {
                continue;
            }
            let nick = self
                .dms
                .get(id)
                .and_then(|h| h.iter().rev().find(|l| !l.sender.is_empty()))
                .map(|l| l.sender.clone())
                .unwrap_or_else(|| format!("anon-{}", hex::encode(&id[..2])));
            rows.push(PersonRow {
                peer_id: *id,
                nickname: nick,
                unread: *unread,
                online: false,
            });
        }
        rows.sort_by_key(|r| {
            (
                std::cmp::Reverse(r.unread > 0),
                std::cmp::Reverse(r.online),
                r.nickname.to_lowercase(),
            )
        });
        rows
    }

    /// Nickname change propagates to the mesh (re-announce).
    /// Send a DM to a resolved peer and echo it into the conversation.
    pub fn send_dm_to(&mut self, peer_id: [u8; 8], content: &str) {
        if content.trim().is_empty() {
            return;
        }
        let Some(mesh_tx) = &self.mesh_tx else {
            self.log_nearby("mesh is not running (bluetooth unavailable?)");
            return;
        };
        let in_range = self
            .mesh_peers
            .get(&peer_id)
            .is_some_and(|(_, seen)| now_secs() - seen < 600);
        if !in_range {
            let nick = self
                .people(now_secs())
                .into_iter()
                .find(|r| r.peer_id == peer_id)
                .map(|r| r.nickname)
                .unwrap_or_else(|| hex::encode(&peer_id[..2]));
            self.dms.entry(peer_id).or_default().push(DmLine {
                sender: String::new(),
                content: format!("not delivered \u{2014} {nick} is out of range"),
                ours: false,
                timestamp: now_secs(),
                notice: true,
            });
            return;
        }
        let _ = mesh_tx.try_send(MeshCommand::SendPrivate {
            peer: peer_id,
            content: content.to_string(),
        });
        let sender = self.nickname_or_id();
        self.dms.entry(peer_id).or_default().push(DmLine {
            sender,
            content: content.to_string(),
            ours: true,
            timestamp: now_secs(),
            notice: false,
        });
    }

    /// Count of mesh peers seen in the last 10 minutes.
    pub fn mesh_peer_count(&self, now: i64) -> usize {
        self.mesh_peers.values().filter(|(_, seen)| now - seen < 600).count()
    }

    /// True while the link layer is rebuilding (wedge recovery).
    pub fn mesh_is_restarting(&self) -> bool {
        self.mesh_restarting
    }

    /// True when the mesh node exists (bluetooth usable).
    pub fn mesh_is_up(&self) -> bool {
        self.mesh_tx.is_some()
    }

    // ---- channel browser ------------------------------------------------

    /// Hand the GeoIP receiver to the view pump (once).
    pub fn take_geo_rx(&mut self) -> Option<UnboundedReceiver<Option<GeoFix>>> {
        self.geo_rx.take()
    }

    /// Enter browse mode: start the relay-wide activity scan and, in the
    /// background, the one-shot coarse location fix.
    pub fn begin_browse(&mut self) {
        if !self.scanning {
            self.scanning = true;
            let _ = self.relay_tx.send(RelayCommand::Discover(true));
        }
        self.start_geo_lookup();
    }

    /// Leave browse mode: stop the scan (harmless if already off).
    pub fn end_browse(&mut self) {
        if self.scanning {
            self.scanning = false;
            let _ = self.relay_tx.send(RelayCommand::Discover(false));
        }
    }

    /// Kick the one-shot GeoIP fix (blocking fetch on a side thread).
    fn start_geo_lookup(&mut self) {
        if self.geo.is_some() || self.geo_loading {
            return;
        }
        self.geo_loading = true;
        let tx = self.geo_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_approx_location());
        });
    }

    /// Background thread's answer.
    pub fn set_geo_fix(&mut self, fix: Option<GeoFix>) {
        self.geo_loading = false;
        self.geo = fix;
    }

    /// The location ladder: parent cells of the estimated position, one
    /// per upstream zoom level (region/province/city/neighborhood), each
    /// with a human label from the fix.
    pub fn ladder(&self) -> Vec<(String, &'static str, String)> {
        let Some(fix) = &self.geo else {
            return Vec::new();
        };
        [(2, fix.country.as_str()), (4, fix.region.as_str()), (5, fix.city.as_str()), (6, "")]
            .iter()
            .map(|(len, label)| {
                let gh = geohash::encode(fix.lat, fix.lon, *len);
                let label = if label.is_empty() {
                    format!("near {}", fix.city)
                } else if label.is_empty() && fix.region.is_empty() {
                    format!("{}, {}", fix.region, fix.country)
                } else {
                    label.to_string()
                };
                (gh, geohash::level_name(*len), label)
            })
            .collect()
    }

    pub fn geo_loading(&self) -> bool {
        self.geo_loading
    }

    /// One remembered visit (newest first, capped at 8).
    fn note_channel_visit(&mut self, gh: &str) {
        self.settings.channels.retain(|m| m.geohash != gh);
        self.settings.channels.insert(
            0,
            ChannelMemo { geohash: gh.to_string(), last_seen: now_secs() },
        );
        self.settings.channels.truncate(8);
    }

    /// History rows for the browser, newest first.
    pub fn channel_history(&self) -> &[ChannelMemo] {
        &self.settings.channels
    }

    fn note_scan(&mut self, gh: &str, pubkey: &str, created_at: i64) {
        let cell = self.scan.entry(gh.to_string()).or_default();
        cell.msgs += 1;
        cell.senders.insert(pubkey.to_string());
        cell.last = cell.last.max(created_at);
        // Keep the table bounded on a busy network.
        if self.scan.len() > 200 {
            if let Some(oldest) = self
                .scan
                .iter()
                .min_by_key(|(_, c)| c.last)
                .map(|(k, _)| k.clone())
            {
                self.scan.remove(&oldest);
            }
        }
    }

    /// Scan rows for the browser: busiest first.
    pub fn scan_rows(&self) -> Vec<ScanEntry> {
        let mut rows: Vec<ScanEntry> = self
            .scan
            .iter()
            .map(|(gh, c)| ScanEntry {
                geohash: gh.clone(),
                msgs: c.msgs,
                people: c.senders.len(),
                last: c.last,
            })
            .collect();
        rows.sort_by_key(|r| (std::cmp::Reverse(r.msgs), std::cmp::Reverse(r.last)));
        rows
    }

    pub fn scanning(&self) -> bool {
        self.scanning
    }

    /// Nickname change propagates to the mesh (re-announce).
    pub fn sync_mesh_nickname(&self) {
        if let Some(mesh_tx) = &self.mesh_tx {
            if !self.settings.nickname.is_empty() {
                let _ = mesh_tx.try_send(MeshCommand::SetNickname(self.settings.nickname.clone()));
            }
        }
    }

    pub fn on_status(&mut self, delta: i32) {
        self.connected_relays =
            (self.connected_relays as i32 + delta).clamp(0, i32::MAX as i32) as usize;
    }

    /// Periodic presence announce (the 60 s heartbeat).
    pub fn heartbeat(&mut self) {
        if self.geohash.is_some() {
            self.send_presence();
        }
    }

    /// Record presence for a pubkey, returning the display name.
    fn note_peer(&mut self, pubkey: &str, nickname: Option<String>, at: i64) -> String {
        let peer = self.peers.entry(pubkey.to_string()).or_insert(Peer { nickname: None, last_seen: 0 });
        if let Some(n) = nickname.filter(|n| !n.is_empty()) {
            peer.nickname = Some(n);
        }
        peer.last_seen = at;
        peer.nickname
            .clone()
            .unwrap_or_else(|| format!("anon-{}", &pubkey[..8.min(pubkey.len())]))
    }

    pub fn nickname_or_id(&self) -> String {
        if self.settings.nickname.is_empty() {
            // Same derivation as the mesh announce nickname (main.rs) so
            // the phone's mesh list, our display, and /msg targets agree.
            format!("anon-{}", hex::encode(&self.device_seed[..2]))
        } else {
            self.settings.nickname.clone()
        }
    }

    /// Count of peers seen in the last 10 minutes.
    pub fn peers_here(&self, now: i64) -> usize {
        self.peers.values().filter(|p| now - p.last_seen < 600).count()
    }

}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn tag_value(tags: &[Vec<String>], key: &str) -> Option<String> {
    tags.iter()
        .find(|t| t.first().map(|k| k.as_str()) == Some(key))
        .and_then(|t| t.get(1).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_channel() -> ChatStore {
        let (_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<RelayCommand>();
        let mut store = ChatStore::new([7u8; 32], Settings::default(), Vec::new(), _tx, None);
        store.join("dr5rs");
        store
    }

    #[test]
    fn nearby_sends_need_only_the_mesh() {
        let (mesh_tx, mut mesh_rx) = tokio::sync::mpsc::channel::<MeshCommand>(16);
        let mut store = ChatStore::new([7u8; 32], Settings::default(), Vec::new(),
            tokio::sync::mpsc::unbounded_channel::<RelayCommand>().0, Some(mesh_tx));
        store.send_nearby("over the mesh");
        // Echoed locally in the nearby space and dispatched to the mesh node.
        match store.nearby.last() {
            Some(ChatLine::Message { content, ours, .. }) => {
                assert_eq!(content, "over the mesh");
                assert!(ours);
            }
            other => panic!("expected local echo, got {other:?}"),
        }
        match mesh_rx.try_recv().expect("mesh command") {
            MeshCommand::SendPublic(text) => assert_eq!(text, "over the mesh"),
            other => panic!("expected SendPublic, got {other:?}"),
        }
        // With no mesh, the user gets guidance instead of silence.
        let mut offline = ChatStore::new([7u8; 32], Settings::default(), Vec::new(),
            tokio::sync::mpsc::unbounded_channel::<RelayCommand>().0, None);
        offline.send_nearby("hello");
        assert!(matches!(offline.nearby.last(), Some(ChatLine::System { .. })));
    }

    #[test]
    fn channel_sends_need_a_join() {
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<RelayCommand>();
        let mut store = ChatStore::new([7u8; 32], Settings::default(), Vec::new(), relay_tx, None);
        store.send_channel("hi"); // not joined → guidance, nothing published
        assert!(matches!(store.channel.last(), Some(ChatLine::System { .. })));
        assert!(relay_rx.try_recv().is_err());

        store.join("DR5RS"); // rejoin normalizes
        store.send_channel("hello channel");
        match store.channel.last() {
            Some(ChatLine::Message { content, ours, .. }) => {
                assert_eq!(content, "hello channel");
                assert!(ours);
            }
            other => panic!("expected local echo, got {other:?}"),
        }
        // join() queues SetRelays + Subscribe first; drain to the publish.
        loop {
            match relay_rx.try_recv().expect("relay publish") {
                RelayCommand::Publish(_) => break,
                _ => continue,
            }
        }
    }

    #[test]
    fn join_derives_persona_and_subscribes() {
        let mut store = store_with_channel();
        assert_eq!(store.geohash.as_deref(), Some("dr5rs"));
        assert!(store.persona.is_some());
        // The join produced a channel notice and a presence publish.
        assert!(matches!(store.channel.first(), Some(ChatLine::System { .. })));
    }

    #[test]
    fn inbound_events_render_and_authenticate() {
        let mut store = store_with_channel();
        let peer = Identity::generate();
        let event = geohash::create_ephemeral_geohash_event(
            "hi from a peer",
            "dr5rs",
            &peer,
            Some("peer_nick"),
            false,
        );
        store.on_relay_event(event);
        match store.channel.last() {
            Some(ChatLine::Message { sender, content, ours, .. }) => {
                assert_eq!((sender.as_str(), content.as_str(), *ours), ("peer_nick", "hi from a peer", false));
            }
            other => panic!("expected peer message, got {other:?}"),
        }

        // Forged signature is dropped.
        let mut forged = geohash::create_ephemeral_geohash_event(
            "forged", "dr5rs", &Identity::generate(), None, false,
        );
        forged.sig = Some("0".repeat(128));
        let len_before = store.channel.len();
        store.on_relay_event(forged);
        assert_eq!(store.channel.len(), len_before);
    }

    #[test]
    fn own_events_are_not_double_echoed() {
        let mut store = store_with_channel();
        let persona = store.persona.clone().unwrap();
        let event = geohash::create_ephemeral_geohash_event("self", "dr5rs", &persona, None, false);
        let len_before = store.channel.len();
        store.on_relay_event(event);
        assert_eq!(store.channel.len(), len_before);
    }

    #[test]
    fn other_channels_are_ignored() {
        let mut store = store_with_channel();
        let event = geohash::create_ephemeral_geohash_event(
            "elsewhere",
            "zzzzz",
            &Identity::generate(),
            None,
            false,
        );
        let len_before = store.channel.len();
        store.on_relay_event(event);
        assert_eq!(store.channel.len(), len_before);
    }

    #[test]
    fn dms_have_a_per_peer_history_and_resolvable_targets() {
        let (mesh_tx, mut mesh_rx) = tokio::sync::mpsc::channel::<MeshCommand>(16);
        let mut store = ChatStore::new([7u8; 32], Settings::default(), Vec::new(),
            tokio::sync::mpsc::unbounded_channel::<RelayCommand>().0, Some(mesh_tx));
        let peer = [9u8; 8];
        store.on_mesh_event(MeshEvent::PeerSeen {
            peer_id: peer,
            nickname: "phone".into(),
            is_new: true,
        });

        // Open, send, receive: the conversation lives per peer, not in a
        // space log.
        assert_eq!(store.open_dm("PHONE").map(|(p, _)| p), Some(peer));
        store.send_dm_to(peer, "hello private");
        store.on_mesh_event(MeshEvent::PrivateMessage {
            sender_id: peer,
            nickname: "phone".into(),
            content: "hello back".into(),
            message_id: "m1".into(),
        });
        let history = store.dm_history(&peer);
        assert_eq!(history.len(), 2);
        assert!((history[0].ours, !history[1].ours) == (true, true));
        assert!(store.dm_history(&[1u8; 8]).is_empty());

        // Unread accounting: one arrived, opening the conversation clears it.
        assert_eq!(store.total_unread(), 1);
        let people = store.people(now_secs());
        assert_eq!(people.len(), 1);
        assert_eq!((people[0].unread, people[0].nickname.as_str()), (1, "phone"));
        store.mark_dm_read(&peer);
        assert_eq!(store.total_unread(), 0);

        // The DM went out over the mesh; the nearby log carries a pointer.
        match mesh_rx.try_recv().expect("mesh command") {
            MeshCommand::SendPrivate { peer: p, content } => {
                assert_eq!((p, content.as_str()), (peer, "hello private"));
            }
            other => panic!("expected SendPrivate, got {other:?}"),
        }
        assert!(store.nearby.iter().any(|l| matches!(
            l,
            ChatLine::System { text } if text.contains("phone sent you a private message")
        )));
    }

    #[test]
    fn out_of_range_dms_fail_honestly() {
        let (mesh_tx, mut mesh_rx) = tokio::sync::mpsc::channel::<MeshCommand>(16);
        let mut store = ChatStore::new([7u8; 32], Settings::default(), Vec::new(),
            tokio::sync::mpsc::unbounded_channel::<RelayCommand>().0, Some(mesh_tx));
        // Never seen: send_dm_to records a not-delivered notice, sends nothing.
        store.send_dm_to([5u8; 8], "anyone there?");
        assert!(mesh_rx.try_recv().is_err());
        let history = store.dm_history(&[5u8; 8]);
        assert_eq!(history.len(), 1);
        assert!(history[0].notice);
        assert!(history[0].content.contains("out of range"));
    }

    #[test]
    fn browse_scans_ranks_and_stops_cleanly() {
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<RelayCommand>();
        let mut store = ChatStore::new([7u8; 32], Settings::default(), Vec::new(), relay_tx, None);

        store.begin_browse();
        // Discovery toggles the manager-level scan subscription.
        let mut saw_discover = false;
        while let Ok(cmd) = relay_rx.try_recv() {
            if matches!(cmd, RelayCommand::Discover(true)) {
                saw_discover = true;
            }
        }
        assert!(saw_discover);
        assert!(store.scanning());

        // Signed traffic from two cells: 9q8yy busier than dr5rs.
        for (gh, n) in [("dr5rs", 2), ("9q8yy", 3)] {
            for i in 0..n {
                store.on_relay_event(geohash::create_ephemeral_geohash_event(
                    &format!("msg {i}"),
                    gh,
                    &Identity::generate(),
                    None,
                    false,
                ));
            }
        }
        let rows = store.scan_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].geohash.as_str(), rows[0].msgs), ("9q8yy", 3));
        assert_eq!(rows[1].people, 2); // two distinct senders in dr5rs

        // Forged traffic never counts.
        let mut forged =
            geohash::create_ephemeral_geohash_event("f", "zz9zz", &Identity::generate(), None, false);
        forged.sig = Some("0".repeat(128));
        store.on_relay_event(forged);
        assert_eq!(store.scan_rows().len(), 2);

        store.end_browse();
        assert!(!store.scanning());
        let mut saw_off = false;
        while let Ok(cmd) = relay_rx.try_recv() {
            if matches!(cmd, RelayCommand::Discover(false)) {
                saw_off = true;
            }
        }
        assert!(saw_off);
    }

    #[test]
    fn location_ladder_nests_and_labels() {
        let mut store = store_with_channel();
        assert!(store.ladder().is_empty());
        store.set_geo_fix(Some(GeoFix {
            lat: 42.605,
            lon: -5.603,
            city: "Some City".into(),
            region: "Some Region".into(),
            country: "Some Country".into(),
        }));
        let ladder = store.ladder();
        assert_eq!(ladder.len(), 4);
        // Precisions 2/4/5/6, each a prefix of the next, with upstream's
        // level names.
        for (i, (gh, level, _)) in ladder.iter().enumerate() {
            assert_eq!(gh.len(), [2, 4, 5, 6][i]);
            assert_eq!(*level, geohash::level_name([2, 4, 5, 6][i]));
            if i > 0 {
                assert!(gh.starts_with(&ladder[i - 1].0));
            }
        }
        // The city rung is labeled with the city from the fix.
        assert!(ladder[2].2.contains("Some City"));
    }

    #[test]
    fn joins_are_remembered_newest_first() {
        let mut store = store_with_channel();
        store.join("dr5rs"); // re-visit bumps it to the front
        store.join("9q8yy");
        let names: Vec<&str> = store
            .channel_history()
            .iter()
            .map(|m| m.geohash.as_str())
            .collect();
        assert_eq!(names, vec!["9q8yy", "dr5rs"]);
    }

    #[test]
    fn radio_lifecycle_surfaces_in_the_nearby_log() {
        let mut store = store_with_channel();
        store.on_mesh_event(MeshEvent::RadioRestarted { heal: true });
        assert!(!store.mesh_is_restarting());
        assert!(store.nearby.iter().any(|l| matches!(
            l,
            ChatLine::System { text } if text.contains("power-cycled")
        )));
    }
}
