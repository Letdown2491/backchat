//! The window: a people rail (spaces + everyone in range), the active
//! space's conversation, and a composer that routes by space.
//!
//! Design model: two public spaces with one hue each, mesh (BLE,
//! mint) and channel (geohash over relays, sky), plus private
//! conversations (violet). The rail is the navigation: no back buttons,
//! no hidden panes; people, presence, and unread are always visible.

use crate::input::{Submit, TextInput};
use crate::store::{ChatLine, ChatStore, PersonRow};
use backchat_ble::engine::MeshEvent;
use backchat_nostr::relay::RelayEvent;
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, ClickEvent, Context, Entity, Focusable, FontWeight,
    ParentElement, Render, ScrollHandle, SharedString, Styled, Window,
};

// ---- palette: dark surfaces + one hue per transport ----------------------
const BG: u32 = 0x0b0e12; // window
const SURFACE: u32 = 0x12161c; // rail, composer field
const RAISED: u32 = 0x1a2330; // my-message bubble, hover
const TEXT: u32 = 0xe2e8f0;
const DIM: u32 = 0x77828f;
const MESH: u32 = 0x63d69c; // nearby / BLE
const RELAY: u32 = 0x6ea8fe; // channel / relays
const PRIVATE: u32 = 0xb8a1f5; // DMs

/// Commands the slash menu completes while typing.
const COMMANDS: [&str; 7] = ["/join", "/nick", "/msg", "/dm", "/back", "/peers", "/help"];

/// Which conversation fills the main area, and where the composer sends.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Space {
    /// BLE mesh: everyone in radio range (mint).
    Nearby,
    /// Geohash channel over relays (sky).
    Channel,
    /// Private conversation (violet).
    Person([u8; 8], String),
}

impl Space {
    fn hue(&self) -> u32 {
        match self {
            Space::Nearby => MESH,
            Space::Channel => RELAY,
            Space::Person(..) => PRIVATE,
        }
    }
}

pub struct ChatView {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    /// Plain scrollable log (no virtualization: uniform_list assumes equal
    /// item heights and clips message bodies).
    log_scroll: ScrollHandle,
    space: Space,
    last_count: usize,
    last_space: Space,
    /// Channel browser open (the `#` space's discovery view).
    browsing: bool,
    last_browsing: bool,
}

impl ChatView {
    /// Build the shell and start the pumps that forward relay/mesh events
    /// into the store.
    pub fn new(
        store: Entity<ChatStore>,
        mut relay_rx: tokio::sync::mpsc::UnboundedReceiver<RelayEvent>,
        mesh_rx: Option<tokio::sync::mpsc::Receiver<MeshEvent>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pump_store = store.downgrade();
        let mut geo_rx = store.update(cx, |s, _| s.take_geo_rx());
        let mut geo_done = geo_rx.is_none();
        cx.spawn(async move |this, cx| {
            loop {
                tokio::select! {
                    event = relay_rx.recv() => {
                        let Some(event) = event else {
                            return; // manager gone
                        };
                        let _ = pump_store.update(cx, |store, _| match event {
                            RelayEvent::Received { event, .. } => store.on_relay_event(event),
                            RelayEvent::Status { delta } => store.on_status(delta),
                        });
                    }
                    fix = geo_rx.as_mut().expect("geo_rx present unless done").recv(), if !geo_done => {
                        match fix {
                            Some(fix) => {
                                let _ = pump_store.update(cx, |store, _| store.set_geo_fix(fix));
                            }
                            None => {} // fetch failed; retry on next browse
                        }
                        geo_done = true; // one-shot either way
                    }
                }
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();

        // BLE mesh pump.
        if let Some(mut mesh_rx) = mesh_rx {
            let mesh_store = store.downgrade();
            cx.spawn(async move |this, cx| {
                loop {
                    let Some(event) = mesh_rx.recv().await else {
                        return; // node gone
                    };
                    // A DM for the conversation that's currently open is
                    // read on arrival; don't leave an unread badge up.
                    let sender = if let MeshEvent::PrivateMessage { sender_id, .. } = &event {
                        Some(*sender_id)
                    } else {
                        None
                    };
                    let _ = mesh_store.update(cx, |store, _| store.on_mesh_event(event));
                    let _ = this.update(cx, |view, cx| {
                        if let (Some(sender), Space::Person(peer, _)) = (sender, &view.space) {
                            if sender == *peer {
                                view.store.update(cx, |store, _| store.mark_dm_read(peer));
                            }
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        }

        // Presence heartbeat: announce us every 60 s while a channel is open.
        {
            let heartbeat_store = store.downgrade();
            cx.spawn(async move |_, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(60))
                        .await;
                    let _ = heartbeat_store.update(cx, |store, _| store.heartbeat());
                }
            })
            .detach();
        }

        let input = cx.new(|cx| TextInput::focused("", "", cx));
        Self {
            store,
            input,
            log_scroll: ScrollHandle::new(),
            space: Space::Nearby,
            last_count: 0,
            last_space: Space::Nearby,
            browsing: false,
            last_browsing: false,
        }
    }

    /// Enter (or the send button): interpret the input as a command or a
    /// message routed to the current space.
    fn handle_input(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).content.to_string();
        self.input.update(cx, |input, _| input.reset());
        if text.trim().is_empty() {
            return;
        }

        if let Some(rest) = text.strip_prefix('/') {
            self.store.update(cx, |store, _| {
                let mut parts = rest.splitn(2, ' ');
                match (parts.next(), parts.next()) {
                    (Some("join"), Some(arg)) if arg.trim().is_empty() => {
                        store.begin_browse();
                        self.browsing = true;
                        self.space = Space::Channel;
                    }
                    (Some("join"), Some(arg)) => store.join(arg),
                    (Some("nick"), Some(arg)) => store.set_nickname(arg),
                    (Some("msg"), Some(rest)) => {
                        let mut parts = rest.splitn(2, ' ');
                        match (parts.next(), parts.next()) {
                            (Some(nick), Some(dm)) => store.send_mesh_dm(nick, dm),
                            _ => store.log_nearby("usage: /msg <nick> <text>"),
                        }
                    }
                    (Some("dm"), Some(nick)) => {
                        let nick = nick.trim();
                        match store.open_dm(nick) {
                            Some(peer) => self.space = Space::Person(peer.0, peer.1),
                            None => store.log_nearby(format!("no mesh peer named {nick}")),
                        }
                    }
                    (Some("back"), _) => self.space = Space::Nearby,
                    (Some("peers"), _) => {
                        let names = store.people(crate::store::now_secs())
                            .into_iter()
                            .filter(|p| p.online)
                            .map(|p| p.nickname)
                            .collect::<Vec<_>>();
                        if names.is_empty() {
                            store.log_nearby("nobody in range right now");
                        } else {
                            store.log_nearby(format!("in range: {}", names.join(", ")));
                        }
                    }
                    (Some("help"), _) => store.system_line(
                        "commands: /join <geohash> · /nick <name> · /dm <nick> · /msg <nick> <text> · /peers · /back",
                    ),
                    (Some("join"), None) => {
                        store.begin_browse();
                        self.browsing = true;
                        self.space = Space::Channel;
                    }
                    (Some("nick"), None) => store.log_nearby("usage: /nick <name>"),
                    (Some("msg"), None) => store.log_nearby("usage: /msg <nick> <text>"),
                    (Some("dm"), None) => store.log_nearby("usage: /dm <nick>"),
                    (Some(other), _) => {
                        store.log_nearby(format!("unknown command /{other}"))
                    }
                    (None, _) => {}
                }
            });
        } else {
            match &self.space {
                Space::Nearby => self.store.update(cx, |store, _| store.send_nearby(&text)),
                Space::Channel => self.store.update(cx, |store, _| store.send_channel(&text)),
                Space::Person(peer, _) => {
                    let peer = *peer;
                    self.store.update(cx, |store, _| store.send_dm_to(peer, &text));
                }
            }
        }
        cx.notify();
    }
}

impl Focusable for ChatView {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = crate::store::now_secs();
        let (people, mesh_peers, mesh_up, restarting, relays, here, geohash, unread) =
            self.store.update(cx, |store, _| {
                (
                    store.people(now),
                    store.mesh_peer_count(now),
                    store.mesh_is_up(),
                    store.mesh_is_restarting(),
                    store.connected_relays,
                    store.peers_here(now),
                    store.geohash.clone(),
                    store.total_unread(),
                )
            });
        let space = self.space.clone();

        // The channel browser replaces the log when open.
        let browsing = self.browsing && matches!(space, Space::Channel);

        // Logs for the active space (unused while browsing).
        let lines: Vec<ChatLine> = if browsing {
            Vec::new()
        } else {
            match &space {
                Space::Nearby => self.store.update(cx, |store, _| store.nearby.clone()),
                Space::Channel => self.store.update(cx, |store, _| store.channel.clone()),
                Space::Person(peer, _) => self.store.update(cx, |store, _| {
                    store
                        .dm_history(peer)
                        .iter()
                        .map(|dm| {
                            if dm.notice {
                                ChatLine::System { text: dm.content.clone() }
                            } else {
                                ChatLine::Message {
                                    sender: dm.sender.clone(),
                                    content: dm.content.clone(),
                                    ours: dm.ours,
                                    timestamp: dm.timestamp,
                                }
                            }
                        })
                        .collect()
                }),
            }
        };

        if browsing {
            self.last_count = 0;
            self.last_space = space.clone();
            self.last_browsing = true;
        } else {
            // Follow the tail only when the reader is at the bottom, or just
            // switched spaces / left the browser; never yank someone reading
            // history.
            let count = lines.len();
            let at_bottom =
                self.log_scroll.offset().y >= self.log_scroll.max_offset().height - px(2.0);
            let switched = self.last_space != space || self.last_browsing != browsing;
            if count > 0 && (switched || (count != self.last_count && at_bottom)) {
                self.log_scroll.scroll_to_item(count - 1);
            }
            self.last_count = count;
            self.last_space = space.clone();
            self.last_browsing = false;
        }

        // Contextual composer hint.
        let placeholder: SharedString = match &space {
            Space::Nearby => "message everyone in range. /help lists commands".into(),
            Space::Channel if browsing => "/join <geohash>: pick a row above or type one".into(),
            Space::Channel => match &geohash {
                Some(g) => format!("message #{g}").into(),
                None => "/join <geohash> to start chatting".into(),
            },
            Space::Person(_, nick) => format!("private to {nick} · end-to-end encrypted").into(),
        };
        self.input.update(cx, |input, _| input.set_placeholder(placeholder));
        let input_text = self.input.read(cx).content.to_string();

        // Header: space identity + one status line of meta.
        let (title, meta, _meta_hue) = match &space {
            Space::Nearby => (
                "mesh".to_string(),
                if restarting {
                    "radio restarting\u{2026}".to_string()
                } else if !mesh_up {
                    "radio off".to_string()
                } else {
                    format!("{mesh_peers} in range")
                },
                MESH,
            ),
            Space::Channel if browsing => (
                "channels".to_string(),
                if relays > 0 {
                    format!("discovery on \u{b7} {relays} relays")
                } else {
                    "connecting\u{2026}".to_string()
                },
                RELAY,
            ),
            Space::Channel => match &geohash {
                Some(g) => (
                    format!("#{g}"),
                    format!("{relays} relays \u{b7} {here} here"),
                    RELAY,
                ),
                None => ("channel".to_string(), "not joined".to_string(), RELAY),
            },
            Space::Person(_, nick) => (
                nick.clone(),
                "private \u{b7} end-to-end encrypted".to_string(),
                PRIVATE,
            ),
        };

        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &Submit, _window, cx| this.handle_input(cx)))
            // ---- Rail -----------------------------------------------------
            .child(
                div()
                    .w(px(216.))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .bg(rgb(SURFACE))
                    .border_r_1()
                    .border_color(rgb(0x1d232c))
                    .py_3()
                    // wordmark
                    .child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_size(px(13.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(TEXT))
                            .child("Backchat"),
                    )
                    // spaces
                    .child(space_row(
                        "mesh",
                        "\u{25c7} mesh",
                        MESH,
                        space == Space::Nearby,
                        Some(if restarting {
                            "\u{25cb} restarting\u{2026}".to_string()
                        } else if !mesh_up {
                            "off".to_string()
                        } else {
                            format!("{mesh_peers}")
                        }),
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.space = Space::Nearby;
                            this.store.update(cx, |store, _| store.end_browse());
                            this.browsing = false;
                            cx.notify();
                        }),
                    ))
                    .children({
                        // Up to five channel rows: the current one first,
                        // then recently visited cells from history.
                        let mut chans: Vec<String> = Vec::new();
                        if let Some(g) = &geohash {
                            chans.push(g.clone());
                        }
                        let history: Vec<String> = self
                            .store
                            .read(cx)
                            .channel_history()
                            .iter()
                            .map(|m| m.geohash.clone())
                            .collect();
                        for gh in history {
                            if chans.len() >= 5 {
                                break;
                            }
                            if !chans.contains(&gh) {
                                chans.push(gh);
                            }
                        }
                        let rows: Vec<gpui::AnyElement> = chans
                            .iter()
                            .map(|gh| {
                                let current = Some(gh.as_str()) == geohash.as_deref();
                                let selected = current && !browsing;
                                space_row(
                                    &format!("ch-{gh}"),
                                    &format!("#{gh}"),
                                    RELAY,
                                    selected,
                                    if current { Some(format!("{here}")) } else { None },
                                    {
                                        let gh = gh.clone();
                                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            this.space = Space::Channel;
                                            if current {
                                                // Re-clicking the channel you're
                                                // in opens discovery.
                                                this.store
                                                    .update(cx, |store, _| store.begin_browse());
                                                this.browsing = true;
                                            } else {
                                                this.store.update(cx, |store, _| store.join(&gh));
                                                this.browsing = false;
                                            }
                                            cx.notify();
                                        })
                                    },
                                )
                                .into_any_element()
                            })
                            .collect();
                        // Keep the browser reachable when there's room.
                        if chans.len() < 5 {
                            let mut rows = rows;
                            rows.push(space_row(
                                    "discover",
                                    &if chans.is_empty() {
                                        "# join a channel".to_string()
                                    } else {
                                        "+ discover".to_string()
                                    },
                                    RELAY,
                                    browsing,
                                    None,
                                    cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.space = Space::Channel;
                                        this.store.update(cx, |store, _| store.begin_browse());
                                        this.browsing = true;
                                        cx.notify();
                                    }),
                            )
                            .into_any_element());
                            rows
                        } else {
                            rows
                        }
                    })
                    // people
                    .child(
                        div()
                            .px_4()
                            .pt_4()
                            .pb_2()
                            .text_size(px(10.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(DIM))
                            .child(if unread > 0 {
                                format!("PEOPLE \u{b7} {unread} unread")
                            } else {
                                format!("PEOPLE \u{b7} {mesh_peers} in range")
                            }),
                    )
                    .children(people.iter().map(|row| person_row(row, &space, cx)))
                    .when(people.is_empty(), |rail| {
                        rail.child(
                            div()
                                .px_4()
                                .text_size(px(12.))
                                .italic()
                                .text_color(rgb(DIM))
                                .child("nobody yet. peers in bluetooth range appear here"),
                        )
                    })
                    .child(div().flex_1())
                    // own identity at the rail's foot
                    .child(
                        div()
                            .px_4()
                            .text_size(px(11.))
                            .text_color(rgb(DIM))
                            .child(
                                self.store.read(cx).nickname_or_id(),
                            ),
                    ),
            )
            // ---- Main area -------------------------------------------------
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    // space header
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(rgb(0x1d232c))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(space.hue()))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(DIM))
                                    .child(meta),
                            ),
                    )
                    // conversation (or channel browser)
                    .child(
                        div()
                            .id("log")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.log_scroll)
                            .flex()
                            .flex_col()
                            .py_3()
                            .children(if browsing {
                                self.render_browse(cx)
                            } else {
                                self.render_space(&space, lines, cx)
                            }),
                    )
                    // slash menu
                    .when(input_text.starts_with('/') && !input_text.contains(' '), |root| {
                        let typed = input_text.to_lowercase();
                        let matches: Vec<&str> = COMMANDS
                            .iter()
                            .copied()
                            .filter(|c| c.starts_with(&typed))
                            .collect();
                        if matches.is_empty() {
                            root
                        } else {
                            root.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap_1()
                                    .px_4()
                                    .pb_1()
                                    .children(matches.into_iter().map(|cmd| {
                                        div()
                                            .id(SharedString::from(format!("cmd-{cmd}")))
                                            .px_2()
                                            .py_0p5()
                                            .rounded(px(6.))
                                            .bg(rgb(SURFACE))
                                            .text_size(px(12.))
                                            .text_color(rgb(space.hue()))
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(RAISED)))
                                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                let fill = format!("{cmd} ");
                                                this.input
                                                    .update(cx, |input, _| input.set_text(&fill));
                                                cx.notify();
                                            }))
                                            .child(cmd)
                                    })),
                            )
                        }
                    })
                    // composer
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_3()
                            .bg(rgb(SURFACE))
                            .child(
                                div()
                                    .flex_1()
                                    .px_2()
                                    .py_1p5()
                                    .rounded(px(8.))
                                    .bg(rgb(BG))
                                    .border_1()
                                    .border_color(rgb(0x1d232c))
                                    .child(self.input.clone()),
                            )
                            .child(
                                div()
                                    .id("send")
                                    .px_3()
                                    .py_1p5()
                                    .rounded(px(8.))
                                    .bg(rgb(space.hue()))
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(BG))
                                    .cursor_pointer()
                                    .hover(|s| s.opacity(0.85))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.handle_input(cx);
                                    }))
                                    .child("\u{25b8}"),
                            ),
                    ),
            )
    }
}

/// One rail row: a transport space with a live count.
fn space_row(
    id: &str,
    label: &str,
    hue: u32,
    selected: bool,
    badge: Option<String>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!("space-{id}")))
        .mx_2()
        .px_3()
        .py_1p5()
        .rounded(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .cursor_pointer()
        .when(selected, |row| row.bg(rgb(RAISED)))
        .when(!selected, |row| row.hover(|s| s.bg(rgb(0x161c24))))
        .on_click(on_click)
        .child(
            div()
                .text_size(px(13.))
                .font_weight(if selected {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if selected { rgb(hue) } else { rgb(TEXT) })
                .child(label.to_string()),
        )
        .when_some(badge, |row, badge| {
            row.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(DIM))
                    .child(badge),
            )
        })
}

/// One rail row: a person with presence dot and unread badge.
fn person_row(
    row: &PersonRow,
    active: &Space,
    cx: &mut Context<ChatView>,
) -> AnyElement {
    let selected = matches!(active, Space::Person(peer, _) if *peer == row.peer_id);
    let hue = if selected { PRIVATE } else { TEXT };
    div()
        .id(SharedString::from(format!("person-{}", hex::encode(row.peer_id))))
        .mx_2()
        .px_3()
        .py_1p5()
        .rounded(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(selected, |r| r.bg(rgb(RAISED)))
        .when(!selected, |r| r.hover(|s| s.bg(rgb(0x161c24))))
        .on_click({
            let peer = (row.peer_id, row.nickname.clone());
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                let (peer_id, name) = peer.clone();
                this.store
                    .update(cx, |store, _| store.mark_dm_read(&peer_id));
                this.space = Space::Person(peer_id, name);
                cx.notify();
            })
        })
        // presence dot
        .child(
            div()
                .text_size(px(9.))
                .text_color(if row.online { rgb(MESH) } else { rgb(DIM) })
                .child(if row.online { "\u{25cf}" } else { "\u{25cb}" }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(13.))
                .text_color(if row.online { rgb(hue) } else { rgb(DIM) })
                .text_ellipsis()
                .child(row.nickname.clone()),
        )
        .when(row.unread > 0, |r| {
            r.child(
                div()
                    .px_1p5()
                    .rounded_full()
                    .bg(rgb(PRIVATE))
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(BG))
                    .child(format!("{}", row.unread)),
            )
        })
        .into_any_element()
}

/// A dim uppercase browser section label.
fn section_label(text: &str) -> AnyElement {
    div()
        .px_5()
        .pt_4()
        .pb_1()
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(DIM))
        .child(text.to_string())
        .into_any_element()
}

/// A quiet centered browser status line.
fn status_line(text: impl Into<String>) -> AnyElement {
    div()
        .px_5()
        .py_1()
        .text_size(px(12.))
        .italic()
        .text_color(rgb(DIM))
        .child(text.into())
        .into_any_element()
}

/// One click-to-join row of the channel browser. `key` scopes the element
/// id: a geohash can appear in more than one section (ladder, history,
/// scan), and duplicate ids corrupt gpui's element state, dead clicks.
fn browse_row(key: &str, gh: &str, meta: String, cx: &mut Context<ChatView>) -> AnyElement {
    div()
        .id(SharedString::from(format!("join-{key}-{gh}")))
        .mx_3()
        .px_3()
        .py_2()
        .rounded(px(8.))
        .flex()
        .flex_row()
        .items_baseline()
        .gap_3()
        .cursor_pointer()
        .hover(|r| r.bg(rgb(RAISED)))
        .on_click({
            let gh = gh.to_string();
            cx.listener(move |this, _: &ClickEvent, _, cx| {
                eprintln!("[browse] join {gh}");
                this.store.update(cx, |store, _| store.join(&gh));
                this.browsing = false;
                cx.notify();
            })
        })
        .child(
            div()
                .text_color(rgb(RELAY))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("#{gh}")),
        )
        .child(
            div()
                .ml_auto()
                .text_size(px(11.))
                .text_color(rgb(DIM))
                .child(meta),
        )
        .into_any_element()
}

/// Compact relative age: "just now", "4m", "3h", "2d".
fn rel_time(secs: i64) -> String {
    if secs < 45 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

impl ChatView {
    /// The active space's conversation: day-separated rows with per-space
    /// styling (mine right-aligned bubbles, theirs clean lines) and designed
    /// empty states.
    fn render_space(
    &self,
    space: &Space,
    lines: Vec<ChatLine>,
    cx: &mut Context<Self>,
) -> Vec<AnyElement> {
    // Empty states first.
    if lines.is_empty() || lines.iter().all(|l| matches!(l, ChatLine::System { .. })) {
        let empty: Vec<(&str, String, String)> = match space {
            Space::Nearby => vec![
                ("\u{25c7}", "mesh".into(), "everyone in bluetooth range, right now".into()),
                ("", "people appear here automatically as they advertise. on a phone, keep bitchat open with the screen on".into(), String::new()),
                ("", "say something: it goes to every peer in range, relayed hop by hop".into(), String::new()),
            ],
            Space::Channel => match &self.store.read(cx).geohash {
                Some(g) => vec![
                    ("#", format!("#{g}"), "location-scoped room, carried by relays".into()),
                    ("", "messages are ephemeral: the last hour only".into(), String::new()),
                    ("", "your identity here rotates per channel; it's not your mesh name".into(), String::new()),
                ],
                None => vec![
                    ("#", "join a channel".into(), "rooms keyed by geohash".into()),
                    ("", "/join <geohash>, e.g. /join 9q8yy (San Francisco)".into(), String::new()),
                    ("", "works with or without the mesh: relays carry it".into(), String::new()),
                ],
            },
            Space::Person(_, nick) => vec![
                ("\u{25c7}", format!("private with {nick}"), "end-to-end encrypted (noise)".into()),
                ("", "the first message opens the encrypted channel. it may take a second".into(), String::new()),
                ("", "only you two can read these".into(), String::new()),
            ],
        };
        return empty
            .into_iter()
            .map(|(glyph, title, hint)| {
                div()
                    .px_5()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(space.hue()))
                                    .child(if glyph.is_empty() {
                                        title.clone()
                                    } else {
                                        format!("{glyph} {title}")
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(DIM))
                                    .child(hint.to_string()),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
    }

    let mut kids: Vec<AnyElement> = Vec::new();
    let mut last_day: Option<i64> = None;
    for line in lines {
        match line {
            ChatLine::System { text } => {
                kids.push(
                    div()
                        .flex()
                        .justify_center()
                        .py_0p5()
                        .child(
                            div()
                                .text_size(px(11.))
                                .italic()
                                .text_color(rgb(DIM))
                                .child(text),
                        )
                        .into_any_element(),
                );
            }
            ChatLine::Message { sender, content, ours, timestamp } => {
                if timestamp > 0 {
                    let day = timestamp / 86_400;
                    if last_day != Some(day) {
                        last_day = Some(day);
                        kids.push(
                            div()
                                .flex()
                                .justify_center()
                                .py_2()
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(DIM))
                                        .child(day_label(day)),
                                )
                                .into_any_element(),
                        );
                    }
                }
                let body = div()
                    .text_size(px(14.))
                    .child(content.clone());
                if ours {
                    kids.push(
                        div()
                            .flex()
                            .justify_end()
                            .px_5()
                            .py_0p5()
                            .child(
                                div()
                                    .max_w(px(420.))
                                    .px_3()
                                    .py_1p5()
                                    .rounded(px(12.))
                                    .bg(rgb(RAISED))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(space.hue()))
                                                    .child("you"),
                                            )
                                            .child(body),
                                    ),
                            )
                            .into_any_element(),
                    );
                } else {
                    kids.push(
                        div()
                            .px_5()
                            .py_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_baseline()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(rgb(space.hue()))
                                                    .child(sender.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(DIM))
                                                    .child(format_time(timestamp)),
                                            ),
                                    )
                                    .child(body),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
    }
    kids
    }

    /// The channel browser: three sections (location ladder, your
    /// channels, live activity), every row click-to-join. Typing
    /// "/join dr5" live-filters the rows to cells starting with dr5.
    fn render_browse(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let now = crate::store::now_secs();
        let input = self.input.read(cx).content.to_string();
        let filter = input
            .strip_prefix("/join ")
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(str::to_lowercase);
        let keep = |gh: &str| -> bool {
            filter.as_deref().is_none_or(|f| gh.starts_with(f))
        };

        let (geo_loading, scanning) = self
            .store
            .update(cx, |store, _| (store.geo_loading(), store.scanning()));
        let ladder = self.store.read(cx).ladder();
        let history: Vec<(String, i64)> = self
            .store
            .read(cx)
            .channel_history()
            .iter()
            .map(|m| (m.geohash.clone(), m.last_seen))
            .collect();
        let scan = self.store.read(cx).scan_rows();

        let ladder: Vec<_> = ladder.into_iter().filter(|(gh, ..)| keep(gh)).collect();
        let history: Vec<_> = history.into_iter().filter(|(gh, _)| keep(gh)).collect();
        let scan: Vec<_> = scan.into_iter().filter(|r| keep(&r.geohash)).collect();
        let nothing_matches =
            ladder.is_empty() && history.is_empty() && scan.is_empty();

        let mut kids: Vec<AnyElement> = Vec::new();

        if !ladder.is_empty() {
            kids.push(section_label("FROM YOUR LOCATION (EST.)"));
            kids.extend(ladder.into_iter().map(|(gh, level, label)| {
                let meta = if label.is_empty() {
                    level.to_string()
                } else {
                    format!("{level} \u{b7} {label}")
                };
                browse_row("loc", &gh, meta, cx)
            }));
        } else if geo_loading {
            kids.push(status_line("estimating your location\u{2026}"));
        }

        if !history.is_empty() {
            kids.push(section_label("YOUR CHANNELS"));
            kids.extend(history.into_iter().map(|(gh, last)| {
                browse_row("hist", &gh, format!("last visited {}", rel_time(now - last)), cx)
            }));
        }

        if !scan.is_empty() {
            kids.push(section_label("ACTIVE RIGHT NOW"));
            kids.extend(scan.into_iter().map(|r| {
                browse_row(
                    "scan",
                    &r.geohash,
                    format!(
                        "{} msg \u{b7} {} people \u{b7} {} ago",
                        r.msgs,
                        r.people,
                        rel_time(now - r.last)
                    ),
                    cx,
                )
            }));
        } else if scanning && filter.is_none() {
            kids.push(status_line("listening for active channels\u{2026}"));
        }

        if nothing_matches {
            if let Some(f) = &filter {
                kids.push(status_line(format!(
                    "no channels match {f}. press Enter to join it anyway"
                )));
            } else if !geo_loading {
                kids.push(status_line(
                    "nothing yet. or type /join <geohash> to go anywhere",
                ));
            }
        }

        kids
    }
}

/// Calendar-day label from a unix day number (no date crate; civil-from-days).
fn day_label(day: i64) -> String {
    let z = day + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {} {}", MONTHS[(m - 1) as usize], d, era * 400 + yoe as i64 + if m <= 2 { 1 } else { 0 })
}

fn format_time(unix_secs: i64) -> String {
    let secs = unix_secs.rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_labels_land_on_the_right_calendar_day() {
        assert_eq!(day_label(0), "Jan 1 1970");
        assert_eq!(day_label(19_723), "Jan 1 2024"); // 54y incl. leap days
        assert_eq!(day_label(20_741), "Oct 15 2026");
        assert_eq!(day_label(20_742), "Oct 16 2026");
    }
}
