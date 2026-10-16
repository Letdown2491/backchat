//! The window shell: header (channel, peers, relay status), the message
//! pane, and the input row. Owns the ChatStore entity and the relay pump.

use crate::input::{Submit, TextInput};
use crate::store::{ChatLine, ChatStore};
use bitchat_ble::engine::MeshEvent;
use bitchat_nostr::relay::RelayEvent;
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, ClickEvent, SharedString, Context, Entity, Focusable, FontWeight,
    ParentElement, Render, ScrollHandle, Styled, Window,
};

const BG: u32 = 0x101418;
const BG_HEADER: u32 = 0x1a2028;
const BG_MINE: u32 = 0x1c2b3a;
const FG: u32 = 0xd8dee9;
const FG_DIM: u32 = 0x6b7684;
const ACCENT: u32 = 0x4fc08d;
const INPUT_BG: u32 = 0x1a2028;

/// Commands the slash menu completes while typing.
const COMMANDS: [&str; 7] = ["/join", "/nick", "/msg", "/dm", "/back", "/peers", "/help"];

pub struct ChatView {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    /// Plain scrollable log — no virtualization (uniform_list assumed equal
    /// item heights and clipped message bodies; virtualized list flakes too).
    log_scroll: ScrollHandle,
    last_line_count: usize,
    /// Which pane the log area shows: the mesh/channel log, the
    /// conversations list, or one open private conversation.
    pane: Pane,
    last_pane: Pane,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pane {
    Log,
    DmList,
    Dm([u8; 8], String),
}

impl ChatView {
    /// Build the shell and start the pump that forwards relay-manager
    /// events into the store.
    pub fn new(
        store: Entity<ChatStore>,
        mut relay_rx: tokio::sync::mpsc::UnboundedReceiver<RelayEvent>,
        mesh_rx: Option<tokio::sync::mpsc::Receiver<MeshEvent>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pump_store = store.downgrade();
        cx.spawn(async move |this, cx| {
            loop {
                let Some(event) = relay_rx.recv().await else {
                    return; // manager gone
                };
                let _ = pump_store.update(cx, |store, _| match event {
                    RelayEvent::Received { event, .. } => store.on_relay_event(event),
                    RelayEvent::Status { delta } => store.on_status(delta),
                });
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();

        // BLE mesh pump: forwards mesh events into the store.
        if let Some(mut mesh_rx) = mesh_rx {
            let mesh_store = store.downgrade();
            cx.spawn(async move |this, cx| {
                loop {
                    let Some(event) = mesh_rx.recv().await else {
                        return; // node gone
                    };
                    // A DM for the conversation that's currently open is
                    // read on arrival — don't leave an unread badge up.
                    let sender =
                        if let MeshEvent::PrivateMessage { sender_id, .. } = &event {
                            Some(*sender_id)
                        } else {
                            None
                        };
                    let _ = mesh_store.update(cx, |store, _| store.on_mesh_event(event));
                    let _ = this.update(cx, |view, cx| {
                        if let (Some(sender), Pane::Dm(peer, _)) = (sender, &view.pane) {
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
        // Uses a gpui timer (this future runs on gpui's executor, not tokio).
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

        let input = cx.new(|cx| {
            TextInput::focused("", "message — or /join <geohash>, /dm <nick>, /peers, /help", cx)
        });
        Self {
            store,
            input,
            log_scroll: ScrollHandle::new(),
            last_line_count: 0,
            pane: Pane::Log,
            last_pane: Pane::Log,
        }
    }

    /// Enter was pressed: interpret the input as a command or a message.
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
                    (Some("join"), Some(arg)) => store.join(arg),
                    (Some("nick"), Some(arg)) => store.set_nickname(arg),
                    (Some("msg"), Some(rest)) => {
                        let mut parts = rest.splitn(2, ' ');
                        match (parts.next(), parts.next()) {
                            (Some(nick), Some(dm)) => store.send_mesh_dm(nick, dm),
                            _ => store.system_line("usage: /msg <nick> <text>"),
                        }
                    }
                    (Some("dm"), Some(nick)) => {
                        let nick = nick.trim();
                        match store.open_dm(nick) {
                            Some((peer, name)) => {
                                store.mark_dm_read(&peer);
                                self.pane = Pane::Dm(peer, name);
                            }
                            None => store.system_line(format!("no mesh peer named {nick} — /peers")),
                        }
                    }
                    (Some("back"), _) => self.pane = Pane::Log,
                    (Some("peers"), _) => {
                        let now = crate::store::now_secs();
                        let names = store.mesh_peer_names(now);
                        if names.is_empty() {
                            store.system_line("no mesh peers nearby (is bluetooth up? phone advertising?)");
                        } else {
                            store.system_line(format!("mesh peers: {}", names.join(", ")));
                        }
                    }
                    (Some("help"), _) => {
                        store.system_line(
                            "commands: /join <geohash> · /nick <name> · /msg <nick> <text> · /dm <nick> · /back · /peers · /help",
                        );
                    }
                    (Some("join"), None) => store.system_line("usage: /join <geohash>"),
                    (Some("nick"), None) => store.system_line("usage: /nick <name>"),
                    (Some("msg"), None) => store.system_line("usage: /msg <nick> <text>"),
                    (Some("dm"), None) => store.system_line("usage: /dm <nick>"),
                    (Some(other), _) => {
                        store.system_line(format!("unknown command /{other} — /help"))
                    }
                    (None, _) => {}
                }
            });
        } else if let Pane::Dm(peer, _) = &self.pane {
            // In a DM pane, plain input is a private message.
            let peer = *peer;
            self.store.update(cx, |store, _| store.send_dm_to(peer, &text));
        } else {
            self.store.update(cx, |store, _| store.send_message(&text));
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
        let now = now_secs();
        let (channel, peers, status, mesh_peers, unread, overview, mesh_up, restarting) =
            self.store.update(cx, |store, _| {
                (
                    store.geohash.clone(),
                    store.peers_here(now),
                    store.connected_relays,
                    store.mesh_peer_count(now),
                    store.total_unread(),
                    store.dm_overview(now),
                    store.mesh_is_up(),
                    store.mesh_is_restarting(),
                )
            });
        let pane = self.pane.clone();
        let lines: Vec<ChatLine> = self.store.update(cx, |store, _| store.lines.clone());
        let dm_lines: Vec<crate::store::DmLine> = match &pane {
            Pane::Dm(peer, _) => {
                self.store.update(cx, |store, _| store.dm_history(peer).to_vec())
            }
            _ => vec![],
        };
        let display_count = match &pane {
            Pane::Dm(..) => dm_lines.len(),
            Pane::DmList => overview.len(),
            Pane::Log => lines.len(),
        };
        // Follow the tail only when the reader is already at the bottom (or
        // just switched panes) — never yank someone who scrolled up to read.
        // Follow the tail only when the reader is already at the bottom (or
        // just switched panes) — never yank someone who scrolled up to read.
        let at_bottom = self.log_scroll.offset().y >= self.log_scroll.max_offset().height - px(2.0);
        let pane_changed = self.last_pane != pane;
        if display_count > 0 && (pane_changed || (display_count != self.last_line_count && at_bottom))
        {
            self.log_scroll.scroll_to_item(display_count - 1);
        }
        self.last_line_count = display_count;
        self.last_pane = pane.clone();

        // Header title (and a back affordance when away from the log).
        let title = match &pane {
            Pane::Dm(_, nick) => format!("\u{1f512} {nick}"),
            Pane::DmList => "conversations".to_string(),
            Pane::Log => match &channel {
                Some(g) => format!("#{g}"),
                None => "bitchat-rs".to_string(),
            },
        };

        let input_text = self.input.read(cx).content.to_string();

        // Contextual input hint per pane.
        let placeholder: SharedString = match &pane {
            Pane::Log => "message — or /join <geohash>, /dm <nick>, /peers, /help".into(),
            Pane::DmList => "pick a conversation above — or /dm <nick>".into(),
            Pane::Dm(_, nick) => format!("private to {nick} — /back returns").into(),
        };
        self.input.update(cx, |input, _| input.set_placeholder(placeholder));

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(FG))
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(|this, _: &Submit, _window, cx| this.handle_input(cx)))
            // Header
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .bg(rgb(BG_HEADER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(pane != Pane::Log, |header| {
                                header.child(
                                    div()
                                        .id("back")
                                        .px_1()
                                        .rounded(px(4.))
                                        .text_color(rgb(FG_DIM))
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(rgb(FG)))
                                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.pane = Pane::Log;
                                            cx.notify();
                                        }))
                                        .child("\u{2190}"),
                                )
                            })
                            .child(
                                div()
                                    .text_color(rgb(ACCENT))
                                    .font_weight(FontWeight::BOLD)
                                    .child(title),
                            )
                            .when(pane == Pane::Log, |header| {
                                header
                                    .child(
                                        div()
                                            .id("here")
                                            .text_size(px(12.))
                                            .text_color(rgb(FG_DIM))
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(rgb(FG)))
                                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                let names = this.store.update(cx, |store, _| store.channel_nicknames(crate::store::now_secs()));
                                                this.store.update(cx, |store, _| {
                                                    if names.is_empty() {
                                                        store.system_line("nobody else in this channel yet");
                                                    } else {
                                                        store.system_line(format!("in the channel: {}", names.join(", ")));
                                                    }
                                                });
                                                cx.notify();
                                            }))
                                            .child(format!("{peers} here")),
                                    )
                                    .child(
                                        div()
                                            .id("mesh-state")
                                            .text_size(px(12.))
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(rgb(ACCENT)))
                                            .text_color(
                                                if restarting {
                                                    rgb(FG_DIM)
                                                } else if mesh_peers > 0 {
                                                    rgb(ACCENT)
                                                } else {
                                                    rgb(FG_DIM)
                                                },
                                            )
                                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                                this.pane = Pane::DmList;
                                                cx.notify();
                                            }))
                                            .child(if restarting {
                                                "\u{25cb} mesh restarting\u{2026}".to_string()
                                            } else if mesh_peers > 0 {
                                                format!("\u{25c9} {mesh_peers} mesh")
                                            } else if mesh_up {
                                                "\u{25c8} mesh".to_string()
                                            } else {
                                                "\u{2013} mesh off".to_string()
                                            }),
                                    )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(if status > 0 { rgb(ACCENT) } else { rgb(FG_DIM) })
                                    .child(if status > 0 {
                                        format!("\u{25cf} {status} relays")
                                    } else {
                                        "\u{25cb} connecting\u{2026}".to_string()
                                    }),
                            )
                            .child(
                                div()
                                    .id("open-dms")
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(6.))
                                    .bg(rgb(if pane == Pane::DmList { BG_MINE } else { BG_HEADER }))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .text_color(if pane == Pane::DmList || unread > 0 {
                                        rgb(ACCENT)
                                    } else {
                                        rgb(FG)
                                    })
                                    .hover(|s| s.text_color(rgb(ACCENT)))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.pane = Pane::DmList;
                                        cx.notify();
                                    }))
                                    .child(if unread > 0 {
                                        format!("\u{1f512} {unread}")
                                    } else {
                                        "\u{1f512} dms".to_string()
                                    }),
                            ),
                    ),
            )
            // Log / conversations pane
            .child(
                div()
                    .id("log")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.log_scroll)
                    .flex()
                    .flex_col()
                    .children(match &pane {
                        Pane::DmList => {
                            let rows: Vec<AnyElement> = overview
                                .iter()
                                .map(|row| {
                                    div()
                                        .id(SharedString::from(format!("dm-row-{}", hex::encode(row.peer_id))))
                                        .px_3()
                                        .py_1_5()
                                        .rounded(px(6.))
                                        .mr_2()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(BG_HEADER)))
                                        .on_click({
                                            let peer = (row.peer_id, row.nickname.clone());
                                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                let (peer_id, name) = peer.clone();
                                                this.store.update(cx, |store, _| store.mark_dm_read(&peer_id));
                                                this.pane = Pane::Dm(peer_id, name);
                                                cx.notify();
                                            })
                                        })
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_size(px(13.))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(if row.online { rgb(ACCENT) } else { rgb(FG_DIM) })
                                                        .child(row.nickname.clone()),
                                                )
                                                .when(!row.online, |r| {
                                                    r.child(
                                                        div().text_size(px(11.)).text_color(rgb(FG_DIM)).child("offline"),
                                                    )
                                                })
                                                .when(row.unread > 0, |r| {
                                                    r.child(
                                                        div()
                                                            .px_1_5()
                                                            .rounded_full()
                                                            .bg(rgb(ACCENT))
                                                            .text_size(px(11.))
                                                            .text_color(rgb(BG))
                                                            .font_weight(FontWeight::BOLD)
                                                            .child(format!("{}", row.unread)),
                                                    )
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .text_color(rgb(FG_DIM))
                                                .text_ellipsis()
                                                .child(row.preview.clone()),
                                        )
                                        .into_any_element()
                                })
                                .collect();
                            if rows.is_empty() {
                                vec![div()
                                    .px_3()
                                    .text_size(px(12.))
                                    .italic()
                                    .text_color(rgb(FG_DIM))
                                    .child("no one on the mesh yet — peers appear here (and in the log) as they're discovered")
                                    .into_any_element()]
                            } else {
                                rows
                            }
                        }
                        pane @ (Pane::Dm(..) | Pane::Log) => {
                            // Unified renderer for both history panes: bubble
                            // rows with day separators; the log additionally
                            // gets clickable sender names and system pills.
                            let entries: Vec<(i64, String, String, bool, bool, bool)> =
                                match pane {
                                    Pane::Dm(peer, _) => dm_lines
                                        .iter()
                                        .map(|dm| {
                                            (
                                                dm.timestamp,
                                                dm.sender.clone(),
                                                dm.content.clone(),
                                                dm.ours,
                                                dm.notice,
                                                true,
                                            )
                                        })
                                        .collect(),
                                    _ => lines
                                        .iter()
                                        .map(|line| match line {
                                            ChatLine::System { text } => {
                                                (0, String::new(), text.clone(), false, true, false)
                                            }
                                            ChatLine::Message { sender, content, ours, timestamp, mesh } => {
                                                (*timestamp, sender.clone(), content.clone(), *ours, false, *mesh)
                                            }
                                        })
                                        .collect(),
                                };

                            // First-run orientation: before any real content,
                            // tell the reader what this screen is and what
                            // the two transports need.
                            let quiet_start = !matches!(pane, Pane::Dm(..))
                                && entries.iter().all(|(_, _, _, _, notice, _)| *notice)
                                && entries.len() <= 2;

                            let mut kids: Vec<AnyElement> = Vec::new();
                            if quiet_start {
                                for (title, hint) in [
                                    ("\u{25c8} mesh", "peers in bluetooth range appear automatically — on a phone, bitchat must be foreground with the screen on"),
                                    ("\u{25cf} channel", "/join <geohash> to chat over relays — works with or without the mesh"),
                                    ("\u{1f512} dms", "click a sender's name (or /dm <nick>) for end-to-end encrypted private chat"),
                                ] {
                                    kids.push(
                                        div()
                                            .px_3()
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap_0p5()
                                                    .child(
                                                        div()
                                                            .text_size(px(12.))
                                                            .font_weight(FontWeight::SEMIBOLD)
                                                            .text_color(rgb(ACCENT))
                                                            .child(title),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_size(px(12.))
                                                            .text_color(rgb(FG_DIM))
                                                            .child(hint),
                                                    ),
                                            )
                                            .into_any_element(),
                                    );
                                }
                                kids.push(
                                    div().px_3().py_1().into_any_element(),
                                );
                            }

                            let mut last_day: Option<i64> = None;
                            for (ts, sender, content, ours, notice, dm) in entries {
                                // Day separator whenever the calendar day
                                // changes (system notices carry no time).
                                if ts > 0 {
                                    let day = ts / 86_400;
                                    if last_day != Some(day) {
                                        last_day = Some(day);
                                        kids.push(day_divider(day_label(day)).into_any_element());
                                    }
                                }
                                if notice {
                                    // Inline status note, unobtrusive.
                                    kids.push(
                                        div()
                                            .px_3()
                                            .text_size(px(11.))
                                            .italic()
                                            .text_color(rgb(FG_DIM))
                                            .child(content)
                                            .into_any_element(),
                                    );
                                    continue;
                                }
                                if !dm {
                                    // System pill in the main log: centered,
                                    // clearly not a conversation turn.
                                    kids.push(system_pill(content).into_any_element());
                                    continue;
                                }
                                let bg = if ours { BG_MINE } else { BG };
                                let mut who = div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .items_baseline()
                                    .when(!ours && !dm, |w| {
                                        // mesh provenance marker on public log messages
                                        w.child(
                                            div().text_size(px(11.)).text_color(rgb(FG_DIM)).child("\u{25c8}"),
                                        )
                                    });
                                let name_div = div()
                                    .text_size(px(12.))
                                    .text_color(rgb(ACCENT))
                                    .font_weight(FontWeight::SEMIBOLD);
                                if dm || ours {
                                    who = who.child(name_div.child(sender.clone()));
                                } else {
                                    // Sender names are a shortcut into a
                                    // private conversation.
                                    let sender = sender.clone();
                                    who = who.child(
                                        name_div
                                            .id(SharedString::from(format!("who-{sender}")))
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(rgb(FG)))
                                            .on_click({
                                                let sender = sender.clone();
                                                cx.listener(move |this, _: &ClickEvent, _, cx| {
                                                    let resolved = this.store.update(cx, |store, _| {
                                                        if store.nickname_or_id() == sender {
                                                            None
                                                        } else {
                                                            store.resolve_peer(&sender)
                                                        }
                                                    });
                                                    match resolved {
                                                        Some((peer_id, name)) => {
                                                            this.store.update(cx, |store, _| store.mark_dm_read(&peer_id));
                                                            this.pane = Pane::Dm(peer_id, name);
                                                            cx.notify();
                                                        }
                                                        None => this.store.update(cx, |store, _| {
                                                            store.system_line(format!(
                                                                "{sender} isn't on the mesh right now"
                                                            ))
                                                        }),
                                                    }
                                                })
                                            })
                                            .child(sender),
                                    );
                                }
                                kids.push(
                                    div()
                                        .px_3()
                                        .py_1()
                                        .mr_2()
                                        .bg(rgb(bg))
                                        .rounded(px(6.))
                                        .child(who.child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(rgb(FG_DIM))
                                                .child(format_time(ts)),
                                        ))
                                        .child(div().text_size(px(14.)).child(content))
                                        .into_any_element(),
                                );
                            }
                            kids
                        }
                    }),
                )
                // Input row            )
            // Slash menu: complete commands while the first word is being
            // typed. Click a suggestion to fill the input.
            .when(input_text.starts_with('/') && !input_text.contains(' '), |root| {
                let typed = input_text.to_lowercase();
                let matches: Vec<&str> = COMMANDS
                    .iter()
                    .copied()
                    .filter(|c| c.starts_with(&typed))
                    .collect();
                if !matches.is_empty() {
                    root.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_1()
                            .px_3()
                            .py_1()
                            .bg(rgb(BG_HEADER))
                            .children(matches.into_iter().map(|cmd| {
                                div()
                                    .id(SharedString::from(format!("cmd-{cmd}")))
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(6.))
                                    .bg(rgb(BG))
                                    .text_size(px(12.))
                                    .text_color(rgb(ACCENT))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(BG_MINE)))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        let fill = format!("{cmd} ");
                                        this.input.update(cx, |input, _| input.set_text(&fill));
                                        cx.notify();
                                    }))
                                    .child(cmd)
                                    
                            })),
                    )
                } else {
                    root
                }
            })
            // Input row
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_3()
                    .py_2()
                    .bg(rgb(INPUT_BG))
                    .child(div().flex_1().child(self.input.clone())),
            )
    }
}

/// Centered, dim pill — a system notice is not a conversation turn.
fn system_pill(text: String) -> gpui::Div {
    div()
        .flex()
        .justify_center()
        .child(
            div()
                .px_2()
                .py_0p5()
                .my_0p5()
                .rounded(px(8.))
                .bg(rgb(BG_HEADER))
                .text_size(px(11.))
                .italic()
                .text_color(rgb(FG_DIM))
                .child(text),
        )
}

/// Thin day separator between calendar days.
fn day_divider(label: String) -> gpui::Div {
    div().flex().justify_center().py_1().child(
        div()
            .px_2()
            .rounded(px(8.))
            .text_size(px(10.))
            .text_color(rgb(FG_DIM))
            .child(label),
    )
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

fn now_secs() -> i64 {    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// UTC HH:MM for the message gutter (the rumor timestamp, not relay time).
fn format_time(unix_secs: i64) -> String {
    let secs = unix_secs.rem_euclid(86400);
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
