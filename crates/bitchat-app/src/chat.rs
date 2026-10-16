//! The window shell: header (channel, peers, relay status), the message
//! pane, and the input row. Owns the ChatStore entity and the relay pump.

use crate::input::{Submit, TextInput};
use crate::store::{ChatLine, ChatStore};
use bitchat_ble::engine::MeshEvent;
use bitchat_nostr::relay::RelayEvent;
use gpui::{
    div, prelude::*, px, rgb, App, Context, Entity, Focusable, FontWeight, ParentElement, Render,
    ScrollHandle, Styled, Window,
};

const BG: u32 = 0x101418;
const BG_HEADER: u32 = 0x1a2028;
const BG_MINE: u32 = 0x1c2b3a;
const FG: u32 = 0xd8dee9;
const FG_DIM: u32 = 0x6b7684;
const ACCENT: u32 = 0x4fc08d;
const INPUT_BG: u32 = 0x1a2028;

pub struct ChatView {
    store: Entity<ChatStore>,
    input: Entity<TextInput>,
    /// Plain scrollable log — no virtualization (uniform_list assumed equal
    /// item heights and clipped message bodies; virtualized list flakes too).
    log_scroll: ScrollHandle,
    last_line_count: usize,
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
                    let _ = mesh_store.update(cx, |store, _| store.on_mesh_event(event));
                    let _ = this.update(cx, |_, cx| cx.notify());
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
            TextInput::focused("", "message — or /join <geohash>, /nick <name>, /help", cx)
        });
        Self {
            store,
            input,
            log_scroll: ScrollHandle::new(),
            last_line_count: 0,
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
                            "commands: /join <geohash> · /nick <name> · /msg <nick> <text> · /peers · /help",
                        );
                    }
                    (Some("join"), None) => store.system_line("usage: /join <geohash>"),
                    (Some("nick"), None) => store.system_line("usage: /nick <name>"),
                    (Some("msg"), None) => store.system_line("usage: /msg <nick> <text>"),
                    (Some(other), _) => {
                        store.system_line(format!("unknown command /{other} — /help"))
                    }
                    (None, _) => {}
                }
            });
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
        let (channel, peers, status, mesh_peers) = self.store.update(cx, |store, _| {
            (
                store.geohash.clone(),
                store.peers_here(now),
                store.connected_relays,
                store.mesh_peer_count(now),
            )
        });
        let lines: Vec<ChatLine> = self.store.update(cx, |store, _| store.lines.clone());
        let line_count = lines.len();
        if line_count != self.last_line_count {
            // Follow the tail: minimally reveal the newest line each frame
            // the log grows.
            if line_count > 0 {
                self.log_scroll.scroll_to_item(line_count - 1);
            }
            self.last_line_count = line_count;
        }

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
                            .child(
                                div()
                                    .text_color(rgb(ACCENT))
                                    .font_weight(FontWeight::BOLD)
                                    .child(match &channel {
                                        Some(g) => format!("#{g}"),
                                        None => "bitchat-rs".to_string(),
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(FG_DIM))
                                    .child(format!("{peers} here")),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(if mesh_peers > 0 { rgb(ACCENT) } else { rgb(FG_DIM) })
                                    .child(if mesh_peers > 0 {
                                        format!("◍ {mesh_peers} mesh")
                                    } else {
                                        "◍ mesh".to_string()
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if status > 0 { rgb(ACCENT) } else { rgb(FG_DIM) })
                            .child(if status > 0 {
                                format!("● {status} relays")
                            } else {
                                "○ connecting…".to_string()
                            }),
                    ),
            )
            // Message pane
            .child(
                div()
                    .id("log")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.log_scroll)
                    .flex()
                    .flex_col()
                    .children(lines.iter().map(|line| match line {
                        ChatLine::System { text } => div()
                            .px_3()
                            .text_size(px(12.))
                            .italic()
                            .text_color(rgb(FG_DIM))
                            .child(text.clone())
                            .into_any_element(),
                        ChatLine::Message { sender, content, ours, timestamp } => {
                            let bg = if *ours { BG_MINE } else { BG };
                            div()
                                .px_3()
                                .py_1()
                                .mr_2()
                                .bg(rgb(bg))
                                .rounded(px(6.))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .items_baseline()
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .text_color(rgb(ACCENT))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(sender.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(rgb(FG_DIM))
                                                .child(format_time(*timestamp)),
                                        ),
                                )
                                .child(
                                    div().text_size(px(14.)).child(content.clone()),
                                )
                                .into_any_element()
                        }
                    })),
            )
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// UTC HH:MM for the message gutter (the rumor timestamp, not relay time).
fn format_time(unix_secs: i64) -> String {
    let secs = unix_secs.rem_euclid(86400);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}
