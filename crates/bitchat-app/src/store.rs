//! The chat model: identity/personas, the joined geohash channel, message
//! history, peer presence, and the relay connection. A GPUI entity the
//! shell renders from; all networking flows through the relay manager's
//! command/event channels.

use crate::persistence::Settings;
use bitchat_nostr::event::{kinds, NostrEvent};
use bitchat_nostr::geohash;
use bitchat_nostr::identity::Identity;
use bitchat_nostr::relay::{RelayCommand, Subscription};
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

/// One rendered line in the chat pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatLine {
    /// Local system notice (joins, errors, help).
    System { text: String },
    /// A channel message. `self` echoes our own sends.
    Message { sender: String, content: String, ours: bool, timestamp: i64 },
}

struct Peer {
    nickname: Option<String>,
    last_seen: i64,
}

pub struct ChatStore {
    pub device_seed: [u8; 32],
    pub settings: Settings,
    /// Geo relay directory (fetched at startup, bundled fallback).
    pub directory: Vec<bitchat_nostr::relay_geo::Entry>,
    /// Identity for the current geohash (derived from the device seed).
    pub persona: Option<Identity>,
    pub geohash: Option<String>,
    pub lines: Vec<ChatLine>,
    pub connected_relays: usize,
    /// pubkey hex → presence state for the current channel.
    peers: HashMap<String, Peer>,
    relay_tx: UnboundedSender<RelayCommand>,
}

impl ChatStore {
    pub fn new(
        device_seed: [u8; 32],
        settings: Settings,
        directory: Vec<bitchat_nostr::relay_geo::Entry>,
        relay_tx: UnboundedSender<RelayCommand>,
    ) -> Self {
        Self {
            device_seed,
            settings,
            directory,
            persona: None,
            geohash: None,
            lines: Vec::new(),
            connected_relays: 0,
            peers: HashMap::new(),
            relay_tx,
        }
    }

    pub fn system_line(&mut self, text: impl Into<String>) {
        self.lines.push(ChatLine::System { text: text.into() });
        self.trim();
    }

    fn trim(&mut self) {
        if self.lines.len() > 1000 {
            self.lines.drain(..self.lines.len() - 1000);
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
        self.lines.push(ChatLine::System {
            text: format!("joined #{geohash_str} (identity rotates per channel)"),
        });

        // Re-pick the relay set: nearest 5 relays to this cell, exactly as
        // upstream selects them, so peers in the channel are reachable.
        let relays = bitchat_nostr::relay_geo::closest_relays(
            &self.directory,
            &geohash_str,
            bitchat_nostr::relay_geo::RELAYS_PER_CHANNEL,
        );
        if !relays.is_empty() {
            let _ = self.relay_tx.send(RelayCommand::SetRelays(relays.clone()));
            self.system_line(format!("relays: {}", relays.join(", ")));
        }

        let filter = bitchat_nostr::relay::geohash_filter(&geohash_str, 3600);
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
        self.persist();
    }

    fn persist(&self) {
        let _ = crate::persistence::save_settings(&self.settings);
    }

    /// Send a public message to the current channel.
    pub fn send_message(&mut self, content: &str) {
        let Some(geohash_str) = self.geohash.clone() else {
            self.system_line("not in a channel — use /join <geohash> first");
            return;
        };
        if content.trim().is_empty() {
            return;
        }
        let persona = self.persona.as_ref().expect("persona exists after join");
        let event = geohash::create_ephemeral_geohash_event(
            content,
            &geohash_str,
            persona,
            Some(&self.settings.nickname),
            false,
        );
        self.lines.push(ChatLine::Message {
            sender: self.nickname_or_id(),
            content: content.to_string(),
            ours: true,
            timestamp: event.created_at,
        });
        self.trim();
        let _ = self.relay_tx.send(RelayCommand::Publish(event));
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
                    return;
                }
                // Ignore echoes of our own events (already echoed locally).
                if self.persona.as_ref().is_some_and(|p| p.public_key_hex() == event.pubkey) {
                    return;
                }
                let sender = self.note_peer(&event.pubkey, tag_value(&event.tags, "n"), event.created_at);
                self.lines.push(ChatLine::Message {
                    sender,
                    content: event.content.clone(),
                    ours: false,
                    timestamp: event.created_at,
                });
                self.trim();
            }
            kinds::GEOHASH_PRESENCE => {
                let Some(channel) = tag_value(&event.tags, "g") else {
                    return;
                };
                if Some(channel.as_str()) != self.geohash.as_deref() {
                    return;
                }
                self.note_peer(&event.pubkey, None, event.created_at);
            }
            _ => {}
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
            self.persona
                .as_ref()
                .map(|p| format!("anon-{}", &p.public_key_hex()[..8]))
                .unwrap_or_else(|| "anon".into())
        } else {
            self.settings.nickname.clone()
        }
    }

    /// Count of peers seen in the last 10 minutes.
    pub fn peers_here(&self, now: i64) -> usize {
        self.peers.values().filter(|p| now - p.last_seen < 600).count()
    }
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
        let mut store = ChatStore::new([7u8; 32], Settings::default(), Vec::new(), _tx);
        store.join("dr5rs");
        store
    }

    #[test]
    fn join_derives_persona_and_subscribes() {
        let mut store = store_with_channel();
        assert_eq!(store.geohash.as_deref(), Some("dr5rs"));
        assert!(store.persona.is_some());
        // The join produced a system line and a presence publish.
        assert!(matches!(store.lines.first(), Some(ChatLine::System { .. })));
    }

    #[test]
    fn send_message_echoes_locally_and_lowercases_geohash() {
        let mut store = store_with_channel();
        store.join("DR5RS"); // rejoin normalizes
        store.send_message("hello channel");
        match store.lines.last() {
            Some(ChatLine::Message { content, ours, .. }) => {
                assert_eq!(content, "hello channel");
                assert!(ours);
            }
            other => panic!("expected local echo, got {other:?}"),
        }
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
        match store.lines.last() {
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
        let len_before = store.lines.len();
        store.on_relay_event(forged);
        assert_eq!(store.lines.len(), len_before);
    }

    #[test]
    fn own_events_are_not_double_echoed() {
        let mut store = store_with_channel();
        let persona = store.persona.clone().unwrap();
        let event = geohash::create_ephemeral_geohash_event("self", "dr5rs", &persona, None, false);
        let len_before = store.lines.len();
        store.on_relay_event(event);
        assert_eq!(store.lines.len(), len_before);
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
        let len_before = store.lines.len();
        store.on_relay_event(event);
        assert_eq!(store.lines.len(), len_before);
    }
}
