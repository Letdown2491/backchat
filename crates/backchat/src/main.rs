//! backchat: geohash channels over Nostr, in a GPUI shell.
//!
//! Startup sequence: load/persist device state, spin the tokio runtime on
//! a background thread, spawn the relay manager, build the store + window.

mod assets;
mod chat;
mod input;
mod niri;
mod persistence;
mod store;
mod titlebar;

use gpui::prelude::*;
use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px,
    size,
};
use store::ChatStore;

fn main() {
    let device_seed = match persistence::load_or_create_device_seed() {
        Ok(seed) => seed,
        Err(e) => {
            eprintln!("backchat: {e}");
            std::process::exit(1);
        }
    };
    let settings = persistence::load_settings();

    // Geo relay directory: remote fetch with bundled fallback (blocking,
    // bounded by the fetch timeout; happens before the window opens).
    let directory = backchat_nostr::relay_geo::load_directory();

    // Tokio runtime on a background thread; the relay manager's tasks live
    // here. We block this thread forever once the handles are handed off.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let relays: Vec<String> = if settings.relays.is_empty() {
        backchat_nostr::FALLBACK_RELAYS.iter().map(|s| s.to_string()).collect()
    } else {
        settings.relays.clone()
    };
    let (relay_tx, relay_rx) = runtime
        .block_on(async { backchat_nostr::relay::spawn_relay_manager(relays) });

    // BLE mesh node: needs BlueZ on the system bus; degrades to None when
    // bluetooth is unavailable (container, no adapter, …).
    // Same derivation as the store's display name, so the nickname the
    // phone sees in its mesh list matches what we type and /msg-able.
    let mesh_nickname = if settings.nickname.is_empty() {
        format!("anon-{}", hex::encode(&device_seed[..2]))
    } else {
        settings.nickname.clone()
    };
    let mesh = match runtime.block_on(backchat_ble::node::spawn(backchat_ble::MeshConfig {
        nickname: mesh_nickname,
        ..Default::default()
    })) {
        Ok(handle) => {
            eprintln!("backchat: BLE mesh node running");
            Some(handle)
        }
        Err(e) => {
            eprintln!("backchat: BLE mesh disabled: {e}");
            None
        }
    };

    std::thread::Builder::new()
        .name("tokio-runtime".into())
        .spawn(move || runtime.block_on(std::future::pending::<()>()))
        .expect("spawn tokio thread");

    let mesh_cmd_tx = mesh.as_ref().map(|m| m.cmd_tx.clone());
    let mesh_events = mesh.map(|m| m.events);

    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        cx.bind_keys(input::key_bindings());

        let bounds = Bounds::centered(None, size(px(720.), px(760.)), cx);
        // the Wayland app_id: niri rules and the flatpak's icon
        // association match against it, and inside the flatpak the
        // FLATPAK_ID is what the desktop file expects
        let app_id =
            std::env::var("FLATPAK_ID").unwrap_or_else(|_| "backchat".to_string());
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // client-side decorations: the compositor frames
                // nothing, our titlebar owns the row (the Wayland
                // default is server decorations, which would double-
                // frame with the bar drawn anyway)
                window_decorations: Some(WindowDecorations::Client),
                // the compositor reads this for the taskbar title
                titlebar: Some(TitlebarOptions {
                    title: Some("backchat".into()),
                    ..Default::default()
                }),
                app_id: Some(app_id.clone().into()),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(|cx| {
                    let store = cx.new(|_cx| {
                        let mut store = ChatStore::new(
                            device_seed,
                            settings.clone(),
                            directory.clone(),
                            relay_tx.clone(),
                            mesh_cmd_tx.clone(),
                        );
                        if !store.settings.nickname.is_empty() {
                            store.system_line(format!(
                                "you are {}. /nick to change",
                                store.settings.nickname
                            ));
                        }
                        store
                    });
                    // Auto-join the persisted channel.
                    let geohash = store.read(cx).settings.geohash.clone();
                    if !geohash.is_empty() {
                        store.update(cx, |store, _| store.join(&geohash));
                    }
                    chat::ChatView::new(store, relay_rx, mesh_events, cx)
                })
            },
        )
        .expect("open window");
        cx.activate(true);
    });
}
