//! The bluer radio layer: peripheral (advertise + local GATT) *and* central
//! (scan + connect) on one adapter, mirroring upstream's dual-role
//! `BLEService`.
//!
//! Routing: broadcast frames flood every live link (peripheral subscribers
//! + connected remote devices). Unicast frames (handshakes, DMs, sync
//! responses) go straight to a peer's known central link when one was
//! learned from its announces, falling back to the flood for peers only
//! reachable as peripheral subscribers (upstream keeps full per-peer link
//! tables; the flood fallback preserves that behavior where we can't tell).

use std::collections::HashMap;
use std::time::Duration;

/// Concurrent central links cap (upstream `MAX_CONNECTIONS_NORMAL`).
const MAX_CENTRAL_LINKS: usize = 8;
/// Skip peers weaker than this (upstream BALANCED power profile).
const MIN_RSSI: i32 = -85;
/// Don't reconnect to an address whose link ended in a hard failure for
/// this long. A phone that walked away re-advertises under a rotated RPA
/// minutes later; the phone-initiated path (it connects to our
/// advertisement) usually beats us there anyway, so central connects stay
/// patient and rare.
const CONNECT_BACKOFF_MS: u64 = 30_000;
/// A busy BlueZ rejects connects with "In Progress" / "Busy" while another
/// procedure (a disconnect winding down, an LE scan hop, another client's
/// connect) is still running. That is transient: short cooldown, no wedge
/// counting, and discovery re-finds the peer seconds later.
const TRANSIENT_BACKOFF_MS: u64 = 15_000;
/// Max remembered failed addresses (RPA churn from bystander devices is
/// otherwise unbounded over long runs).
const MAX_BLOCKED_ADDRESSES: usize = 128;
/// Bound one central connect attempt. bluetoothd's Connect() waits for the
/// peer's advertisement before issuing the LE connection command and never
/// times out at the D-Bus layer; a stale RPA (phones rotate every ~15 min)
/// would otherwise wedge the serialized connect slot indefinitely.
const CONNECT_TIMEOUT_SECS: u64 = 20;

fn ts() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("[link {:07.3}]", (ms % 1_000_000_000) as f64 / 1000.0)
}

use bluer::adv::{Advertisement, Type as AdvType};
use bluer::gatt::local::{
    characteristic_control, Application, Characteristic, CharacteristicControlEvent,
    CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicWrite,
    CharacteristicWriteMethod, Service,
};
use bluer::{AdapterEvent, DiscoveryFilter, DiscoveryTransport};
use futures::{pin_mut, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use backchat_core::message_type::MessageType;
use backchat_core::packet::Packet;

use crate::{MeshConfig, CHARACTERISTIC_UUID};

/// Raw packet bytes destined for the radio (broadcast in v1).
pub type OutboundTx = mpsc::Sender<Vec<u8>>;
/// Raw packet bytes received from any radio path.
pub type InboundTx = mpsc::Sender<Vec<u8>>;

/// Shared link state: peripheral notify writers (one per subscribed
/// central), connected remote devices (central role), and which mesh peer
/// each central link speaks for (learned from announces, used to route
/// unicast frames).
struct Links {
    writers: Vec<bluer::gatt::CharacteristicWriter>,
    remotes: HashMap<bluer::Address, mpsc::Sender<Vec<u8>>>,
    /// mesh peer id → the central address it was announced over.
    peer_addr: HashMap<[u8; 8], bluer::Address>,
    /// Reverse of `peer_addr`, for cleanup when a link ends.
    addr_peer: HashMap<bluer::Address, [u8; 8]>,
}

/// A running link layer. Holds the live bluer handles (dropping the
/// advertisement / GATT application handles *stops advertising* and
/// unregisters the server: they must stay alive for the link's lifetime).
/// Tear down by aborting [`LinkHandle::task`] and [`LinkHandle::discovery`].
pub struct LinkHandle {
    /// Raw packets received from any radio path.
    pub inbound_rx: mpsc::Receiver<Vec<u8>>,
    /// Raw packets to transmit.
    pub outbound_tx: OutboundTx,
    /// Consecutive adapter-level connect failures (NotReady) that suggest
    /// the adapter itself is wedged; cleared on any successful connect.
    pub wedge_failures: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// The outbound pump task.
    pub task: tokio::task::JoinHandle<()>,
    /// The discovery task.
    pub discovery: tokio::task::JoinHandle<()>,
    /// Live advertisement: dropping it stops advertising.
    _adv: bluer::adv::AdvertisementHandle,
    /// Live GATT application: dropping it unregisters the server.
    _gatt: bluer::gatt::local::ApplicationHandle,
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("bluer: {0}")]
    Bluer(#[from] bluer::Error),
    #[error("no bluetooth adapter")]
    NoAdapter,
}

/// Runs the link layer until the handle's tasks are aborted. When
/// `heal_power_cycle` is set (respawn after wedge detection), the adapter is
/// power-cycled first: an HCI reset clears zombie LE connections left behind
/// when a client died mid-connection: without it the controller stays deaf.
pub async fn spawn(config: &MeshConfig, heal_power_cycle: bool) -> Result<LinkHandle, LinkError> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    if heal_power_cycle {
        eprintln!("{} link: heal: power-cycling adapter", ts());
        let _ = adapter.set_powered(false).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    adapter.set_powered(true).await?;

    let service_uuid = config.network.service_uuid();

    // Diagnostic: BACKCHAT_NO_CENTRAL=1 (non-empty) runs peripheral-only
    // (advertise + GATT, never connect out). The phone connects to our
    // advertisement; this isolates whether our own central attempts are
    // suppressing it. Empty or unset means both roles, matching the
    // non-empty convention of BACKCHAT_DEBUG.
    let central_enabled = !std::env::var("BACKCHAT_NO_CENTRAL")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !central_enabled {
        eprintln!("{} link: central role disabled (BACKCHAT_NO_CENTRAL): peripheral only", ts());
    }

    // Startup sweep: bluetoothd keeps a peripheral connection alive after
    // our process exits, and the phone keeps its central side up too. The
    // phone then believes it is talking to a dead GATT registration and
    // never re-scans or reconnects, so every restart is deaf until the
    // link happens to die on its own (observed: the phone stayed connected
    // for the rest of a test day, writing packets to stale handles). Any
    // device still connected that advertises our service UUID is by
    // definition a leftover from a previous run: this instance has not
    // existed long enough to have live subscribers. Disconnecting it makes
    // the phone drop, re-scan, and come back fresh.
    match adapter.device_addresses().await {
        Ok(addrs) => {
            for addr in addrs {
                let device = match adapter.device(addr) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let connected = device.is_connected().await.unwrap_or(false);
                if !connected {
                    continue;
                }
                let serves_us = device
                    .uuids()
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                    .contains(&service_uuid);
                if serves_us {
                    eprintln!("{} link: startup sweep: disconnecting stale peer {addr}", ts());
                    let _ = device.disconnect().await;
                }
            }
        }
        Err(e) => eprintln!("{} link: startup sweep: device list unavailable: {e}", ts()),
    }

    let links = std::sync::Arc::new(tokio::sync::Mutex::new(Links {
        writers: Vec::new(),
        remotes: HashMap::new(),
        peer_addr: HashMap::new(),
        addr_peer: HashMap::new(),
    }));
    // BlueZ runs one connection procedure at a time: concurrent Connect
    // calls fail with "In Progress".
    let connect_lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
    let wedge_failures = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(256);
    let (outbound_tx, outbound_rx) = mpsc::channel::<Vec<u8>>(256);

    // ---- Peripheral: advertisement + local GATT ----------------------------
    let advertisement = Advertisement {
        advertisement_type: AdvType::Peripheral,
        service_uuids: [service_uuid].into_iter().collect(),
        discoverable: Some(true),
        local_name: Some(config.nickname.clone()),
        ..Default::default()
    };
    let adv_handle = adapter.advertise(advertisement).await?;

    let (char_control, char_control_handle) = characteristic_control();
    // Inbound packets arrive as characteristic writes from centrals. The
    // Fun method is load-bearing: with the Io method bluer's WriteValue
    // handler returns NotSupported, so bluetoothd drops every Write
    // Command from the phone before it ever reaches us (observed: phone
    // connects, subscribes, its announce never arrives, link dies).
    // Both write flags are set: Android bitchat writes without response,
    // accepting with-response too costs nothing.
    let inbound_for_write = inbound_tx.clone();
    let write_fun: bluer::gatt::local::CharacteristicWriteFun = Box::new(
        move |value: Vec<u8>, req: bluer::gatt::local::CharacteristicWriteRequest| {
            let inbound_tx = inbound_for_write.clone();
            Box::pin(async move {
                crate::radio_log!(
                    "{} peripheral: packet ({:?}, {} bytes) from {}",
                    ts(), req.op_type, value.len(), req.device_address
                );
                if inbound_tx.send(value).await.is_err() {
                    eprintln!("{} peripheral: inbound drop: engine gone", ts());
                }
                Ok(())
            })
        },
    );
    let application = Application {
        services: vec![Service {
            uuid: service_uuid,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: CHARACTERISTIC_UUID,
                write: Some(CharacteristicWrite {
                    write: true,
                    write_without_response: true,
                    method: CharacteristicWriteMethod::Fun(write_fun),
                    ..Default::default()
                }),
                notify: Some(CharacteristicNotify {
                    notify: true,
                    method: CharacteristicNotifyMethod::Io,
                    ..Default::default()
                }),
                control_handle: char_control_handle,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let app_handle = adapter.serve_gatt_application(application).await?;

    // Peripheral control loop: accept writes (inbound packets) and notify
    // subscriptions (outbound sinks).
    {
        let links = links.clone();
        let inbound_tx = inbound_tx.clone();
        tokio::spawn(async move {
            pin_mut!(char_control);
            while let Some(event) = char_control.next().await {
                match event {
                    CharacteristicControlEvent::Write(req) => {
                        eprintln!("{} peripheral: central write channel opened (mtu {})", ts(), req.mtu());
                        let mtu = req.mtu();
                        if let Ok(mut reader) = req.accept() {
                            let inbound_tx = inbound_tx.clone();
                            tokio::spawn(async move {
                                let mut buf = vec![0u8; mtu.max(64)];
                                loop {
                                    match reader.read(&mut buf).await {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            if inbound_tx.send(buf[..n].to_vec()).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    }
                    CharacteristicControlEvent::Notify(notifier) => {
                        eprintln!("{} peripheral: central subscribed to notifications (mtu {})", ts(), notifier.mtu());
                        links.lock().await.writers.push(notifier);
                    }
                }
            }
        });
    }

    // ---- Central: scan for peers advertising the service -------------------
    let blocked = std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::<bluer::Address, u64>::new()));
    // Fresh peer sightings waiting for the serialized connect slot: address
    // → (last rssi, last seen ms). The scheduler always picks the strongest
    // signal, so a desk-side phone outranks neighbors across the wall.
    let pending = std::sync::Arc::new(tokio::sync::Mutex::new(
        HashMap::<bluer::Address, (Option<i16>, u64)>::new(),
    ));
    let discovery = {
        let links = links.clone();
        let blocked = blocked.clone();
        let adapter = adapter.clone();
        let pending = pending.clone();
        let connect_lock = connect_lock.clone();
        let own_address = adapter.address().await.unwrap_or_default();
        tokio::spawn(async move {
            let filter = DiscoveryFilter {
                uuids: [service_uuid].into_iter().collect(),
                transport: DiscoveryTransport::Le,
                ..Default::default()
            };
            if adapter.set_discovery_filter(filter).await.is_err() {
                return;
            }
            loop {
                // bluetoothd finds a connect target with its own triggered
                // scan, then issues the LE connection command. A discovery
                // restart in the middle (the MGMT stop + start cycle) tears
                // that down, so the pending connect never completes and no
                // LE Create Connection ever reaches the controller. Don't
                // churn discovery while any connect procedure is in flight.
                while connect_lock.try_lock().is_err() {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                let events = match adapter.discover_devices().await {
                    Ok(events) => events,
                    Err(_) => return,
                };
                pin_mut!(events);
                while let Some(event) = events.next().await {
                    match event {
                        AdapterEvent::DeviceAdded(addr) => {
                            if own_address == addr {
                                continue;
                            }
                            // Connection scheduler (ported from upstream's
                            // BluetoothGattClientManager + PowerManager):
                            // cap concurrent links, gate weak signals, and
                            // back off addresses that failed permanently so
                            // one dying device can't hold the serialized
                            // connect slot forever.
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            {
                                let mut blocked = blocked.lock().await;
                                if let Some(&until) = blocked.get(&addr) {
                                    if now_ms < until {
                                        continue;
                                    }
                                    blocked.remove(&addr);
                                }
                            }
                            if !central_enabled {
                                // Peripheral-only: just note the sighting,
                                // throttled by a 15s cooldown in the same
                                // table used for connect backoff.
                                let mut blocked = blocked.lock().await;
                                if blocked.get(&addr).copied().unwrap_or(0) <= now_ms {
                                    blocked.insert(addr, now_ms + 15_000);
                                    let device = adapter.device(addr);
                                    let name = match &device {
                                        Ok(d) => d.name().await.ok().flatten().unwrap_or_default(),
                                        Err(_) => String::new(),
                                    };
                                    let rssi = match &device {
                                        Ok(d) => d.rssi().await.ok().flatten(),
                                        Err(_) => None,
                                    };
                                    let rssi = rssi
                                        .map(|r| format!("{r} dBm"))
                                        .unwrap_or_else(|| "rssi n/a".into());
                                    eprintln!(
                                        "{} discovery: bitchat peer at {} (name {name:?}, {rssi}) advertising",
                                        ts(), addr
                                    );
                                }
                                continue;
                            }
                            let link_table = links.lock().await;
                            if link_table.remotes.contains_key(&addr) {
                                continue;
                            }
                            if link_table.remotes.len() >= MAX_CENTRAL_LINKS {
                                continue;
                            }
                            drop(link_table);
                            let device = match adapter.device(addr) {
                                Ok(device) => device,
                                Err(_) => continue,
                            };
                            let uuids = device.uuids().await.unwrap_or_default().unwrap_or_default();
                            if !uuids.contains(&service_uuid) {
                                continue;
                            }
                            // Upstream's BALANCED power profile threshold.
                            if let Ok(Some(rssi)) = device.rssi().await {
                                if rssi < MIN_RSSI as i16 {
                                    crate::radio_log!(
                                        "{} discovery: peer at {} too weak ({rssi} dBm < {MIN_RSSI}): skipping",
                                        ts(), addr
                                    );
                                    continue;
                                }
                            }
                            // Queue the sighting; the connect scheduler
                            // picks the strongest fresh peer each tick.
                            let rssi = device.rssi().await.ok().flatten();
                            let mut pending = pending.lock().await;
                            pending.insert(addr, (rssi, now_ms));
                        }
                        AdapterEvent::DeviceRemoved(_) => {}
                        _ => {}
                    }
                }
                // A stream that ends immediately (adapter state churn)
                // must not become a hot loop.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    };

    // ---- Connect scheduler: strongest fresh sighting gets the slot ---------
    {
        let adapter = adapter.clone();
        let links = links.clone();
        let inbound_tx = inbound_tx.clone();
        let blocked = blocked.clone();
        let pending = pending.clone();
        let wedge_failures = wedge_failures.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let best = {
                    let mut pending = pending.lock().await;
                    pending.retain(|_, (_, seen)| now_ms.saturating_sub(*seen) < 30_000);
                    // No rssi property yet ranks weakest so a measured peer wins.
                    let best = pending
                        .iter()
                        .max_by_key(|(_, (rssi, _))| rssi.unwrap_or(-120))
                        .map(|(addr, (rssi, _))| (*addr, *rssi));
                    if let Some((addr, _)) = best {
                        pending.remove(&addr);
                    }
                    best
                };
                let Some((addr, best_rssi)) = best else { continue };
                // A connect procedure owns the slot; leave the sighting
                // decision to a later tick rather than queue another task
                // behind the lock.
                if connect_lock.try_lock().is_err() {
                    pending.lock().await.insert(addr, (best_rssi, now_ms));
                    continue;
                }
                let device = match adapter.device(addr) {
                    Ok(device) => device,
                    Err(_) => continue,
                };
                let link_table = links.lock().await;
                if link_table.remotes.contains_key(&addr)
                    || link_table.remotes.len() >= MAX_CENTRAL_LINKS
                {
                    continue;
                }
                drop(link_table);
                let uuids = device.uuids().await.unwrap_or_default().unwrap_or_default();
                if !uuids.contains(&service_uuid) {
                    continue;
                }
                crate::radio_log!(
                    "{} discovery: bitchat peer at {} (name {:?}): connecting as central",
                    ts(), addr, device.name().await.ok().flatten().unwrap_or_default()
                );
                let (remote_tx, remote_rx) = mpsc::channel::<Vec<u8>>(64);
                links.lock().await.remotes.insert(addr, remote_tx.clone());
                let inbound = inbound_tx.clone();
                let connect_lock = connect_lock.clone();
                let wedge_failures = wedge_failures.clone();
                let blocked = blocked.clone();
                let links = links.clone();
                tokio::spawn(async move {
                    let mut remote_rx = remote_rx;
                    let result = run_remote(
                        &device,
                        service_uuid,
                        &mut remote_rx,
                        &inbound,
                        &links,
                        &connect_lock,
                        &wedge_failures,
                    )
                    .await;
                    if let Err(ref err) = result {
                        // Classify the failure: a busy BlueZ
                        // ("In Progress", "Busy") is transient; a
                        // connect timeout to an absent phone is a
                        // normal mesh miss (NOT a wedge: it was
                        // over-counting and power-cycling the
                        // adapter on ordinary misses); "NotReady"
                        // means the adapter itself is sick.
                        let msg = err.to_string();
                        let (wedge, cooldown) = if msg.contains("InProgress")
                            || msg.contains("Busy")
                        {
                            (false, TRANSIENT_BACKOFF_MS)
                        } else if msg.contains("NotReady") {
                            (true, CONNECT_BACKOFF_MS)
                        } else {
                            (false, CONNECT_BACKOFF_MS)
                        };
                        if wedge {
                            // The node watches this counter with
                            // inbound silence and power-cycles.
                            wedge_failures
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let mut blocked = blocked.lock().await;
                        if !blocked.contains_key(&addr)
                            && blocked.len() >= MAX_BLOCKED_ADDRESSES
                        {
                            if let Some(oldest) = blocked
                                .iter()
                                .min_by_key(|(_, t)| **t)
                                .map(|(a, _)| *a)
                            {
                                blocked.remove(&oldest);
                            }
                        }
                        blocked.insert(addr, now_ms + cooldown);
                    }
                    if let Err(ref err) = result {
                        eprintln!("{} remote {}: link ended: {err}", ts(), device.address());
                    } else {
                        eprintln!("{} remote {}: link ended: connected session finished", ts(), device.address());
                    }
                    let mut links = links.lock().await;
                    links.remotes.remove(&addr);
                    // The peer routing entry dies with its link;
                    // the next announce re-learns it.
                    if let Some(peer) = links.addr_peer.remove(&addr) {
                        links.peer_addr.remove(&peer);
                    }
                });
            }
        });
    }

    // ---- Outbound pump: write every frame to every live link ---------------
    // Peripheral writes carry a timeout: a stalled subscriber must not hold
    // the links mutex (and with it discovery and every other transmit)
    // forever. Writers that fail or stall are removed; BlueZ re-subscribes
    // as a fresh writer if the peer actually wants notifications.
    let task = tokio::spawn(async move {
        let mut outbound_rx = outbound_rx;
        while let Some(bytes) = outbound_rx.recv().await {
            let mut links = links.lock().await;
            crate::radio_log!(
                "{} transmit: {} bytes → {} peripheral subscriber(s), {} central link(s)",
                ts(), bytes.len(), links.writers.len(), links.remotes.len()
            );
            // Unicast frames (handshakes, DMs, sync responses) go straight
            // to the peer's known central link when we have one, instead of
            // flooding every radio path. Unknown or peripheral-side peers
            // still flood (their writers can't be correlated today).
            if let Some(addr) = route_for(
                Packet::decode(&bytes).ok().and_then(|p| p.recipient_id),
                &links.peer_addr,
                &links.remotes,
            ) {
                if let Some(remote) = links.remotes.get(&addr) {
                    match remote.try_send(bytes.clone()) {
                        Ok(()) => {
                            crate::radio_log!("{} transmit: routed unicast to {addr}", ts());
                            continue;
                        }
                        Err(e) => {
                            crate::radio_log!("{} transmit: unicast route {addr} full ({e}): flooding", ts());
                        }
                    }
                }
            }
            let mut dead = Vec::new();
            for (i, writer) in links.writers.iter_mut().enumerate() {
                match tokio::time::timeout(Duration::from_secs(5), writer.write_all(&bytes)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        crate::radio_log!("{} transmit: peripheral write failed: {e}", ts());
                        dead.push(i);
                    }
                    Err(_) => {
                        crate::radio_log!("{} transmit: peripheral write stalled >5s: dropping subscriber", ts());
                        dead.push(i);
                    }
                }
            }
            for i in dead.into_iter().rev() {
                links.writers.remove(i);
            }
            for remote in links.remotes.values() {
                if let Err(e) = remote.try_send(bytes.clone()) {
                    crate::radio_log!("{} transmit: central write failed: {e}", ts());
                }
            }
        }
    });

    Ok(LinkHandle {
        inbound_rx,
        outbound_tx,
        wedge_failures,
        task,
        discovery,
        _adv: adv_handle,
        _gatt: app_handle,
    })
}

/// Record which mesh peer a remote link speaks for, from announce packets
/// only (relayed packets carry a third party's sender id and would map the
/// wrong peer). Never blocks: if the transmit pump holds the table lock,
/// the next announce (15 s cadence) re-learns the mapping.
fn learn_peer(
    bytes: &[u8],
    addr: bluer::Address,
    links: &std::sync::Arc<tokio::sync::Mutex<Links>>,
) {
    let Ok(packet) = Packet::decode(bytes) else { return };
    if packet.kind != MessageType::Announce as u8 || packet.recipient_id.is_some() {
        return;
    }
    let Ok(mut links) = links.try_lock() else { return };
    let peer = packet.sender_id;
    if let Some(old_peer) = links.addr_peer.insert(addr, peer) {
        if old_peer != peer {
            links.peer_addr.remove(&old_peer);
        }
    }
    if let Some(old_addr) = links.peer_addr.insert(peer, addr) {
        if old_addr != addr && links.addr_peer.get(&old_addr) == Some(&peer) {
            links.addr_peer.remove(&old_addr);
        }
    }
}

/// Which central link (if any) carries a unicast frame for this recipient?
/// Some only when the peer announced over a still-live central link.
fn route_for(
    recipient: Option<[u8; 8]>,
    peer_addr: &HashMap<[u8; 8], bluer::Address>,
    remotes: &HashMap<bluer::Address, mpsc::Sender<Vec<u8>>>,
) -> Option<bluer::Address> {
    let addr = peer_addr.get(&recipient?)?;
    remotes.contains_key(addr).then_some(*addr)
}

/// Central-role connection to one remote device: acquire write-without-
/// response IO + notifications, then pump both directions until it goes away.
#[allow(clippy::too_many_arguments)]
async fn run_remote(
    device: &bluer::Device,
    service_uuid: uuid::Uuid,
    outbound_rx: &mut mpsc::Receiver<Vec<u8>>,
    inbound_tx: &InboundTx,
    links: &std::sync::Arc<tokio::sync::Mutex<Links>>,
    connect_lock: &tokio::sync::Mutex<()>,
    wedge_failures: &std::sync::atomic::AtomicU32,
) -> bluer::Result<()> {
    use std::sync::atomic::Ordering;
    {
        // Serialize connection procedures across all remotes.
        let _guard = connect_lock.lock().await;
        eprintln!("{} remote {}: connecting", ts(), device.address());
        if !device.is_connected().await? {
            // bluer's connect() is a bare D-Bus call with no timeout: when
            // the sighting's RPA rotates away (phones do, every ~15 min)
            // bluetoothd waits for the device to advertise again forever
            // and never replies, wedging the serialized connect slot for
            // everyone. Bound each attempt; bluetoothd's Disconnect cancels
            // a pending Connect, so clean up the zombie attempt too.
            let mut retries = 2;
            let mut backoff = 700u64;
            loop {
                let attempt = tokio::time::timeout(
                    Duration::from_secs(CONNECT_TIMEOUT_SECS),
                    device.connect(),
                )
                .await;
                match attempt {
                    Ok(Ok(())) => break,
                    Ok(Err(err)) if retries > 0 => {
                        eprintln!("{} remote {}: connect failed ({err}), retrying", ts(), device.address());
                        retries -= 1;
                        tokio::time::sleep(Duration::from_millis(backoff)).await;
                        backoff *= 2;
                    }
                    Ok(Err(err)) => {
                        eprintln!("{} remote {}: connect FAILED permanently: {err}", ts(), device.address());
                        return Err(err);
                    }
                    Err(_) => {
                        eprintln!(
                            "{} remote {}: connect timed out after {CONNECT_TIMEOUT_SECS}s (address stale or peer not advertising), cancelling",
                            ts(), device.address()
                        );
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            device.disconnect(),
                        ).await;
                        if retries > 0 {
                            retries -= 1;
                            tokio::time::sleep(Duration::from_millis(backoff)).await;
                            backoff *= 2;
                            continue;
                        }
                        return Err(bluer::Error::from(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "connect timed out",
                        )));
                    }
                }
            }
        }
        wedge_failures.store(0, Ordering::Relaxed);
    }
    eprintln!("{} remote {}: connected", ts(), device.address());

    // Locate the characteristic. Right after connect() the remote GATT db
    // is sometimes not yet resolved: retry briefly before giving up. When
    // the connect came through a coalesced bluetoothd attempt the
    // resolution can lag by several seconds.
    let mut characteristic = None;
    let mut resolved_misses = 0u32;
    for attempt in 0..30 {
        let services = device.services().await?;
        let mut found = None;
        for service in &services {
            if service.uuid().await? != service_uuid {
                continue;
            }
            for char in service.characteristics().await? {
                if char.uuid().await? == CHARACTERISTIC_UUID {
                    found = Some(char);
                    break;
                }
            }
            if found.is_some() { break; }
        }
        if let Some(char) = found {
            characteristic = Some(char);
            break;
        }
        if !services.is_empty() {
            // The db resolved without our service: the peer accepts
            // connections but its mesh app is not serving GATT right now
            // (phone with the app closed or backgrounded). Waiting longer
            // cannot help, and holding the connection would keep our own
            // advertisement down.
            resolved_misses += 1;
            if resolved_misses >= 4 {
                eprintln!(
                    "{} remote {}: peer resolved {} services but none serves the mesh: releasing the link",
                    ts(), device.address(), services.len()
                );
                let _ = tokio::time::timeout(Duration::from_secs(5), device.disconnect()).await;
                return Ok(());
            }
        }
        if attempt % 4 == 3 {
            let mut resolved: Vec<String> = Vec::new();
            for s in device.services().await.unwrap_or_default() {
                resolved.push(s.uuid().await.unwrap_or_default().to_string());
            }
            eprintln!(
                "{} remote {}: service not resolved yet (attempt {}): resolved services: {resolved:?}",
                ts(), device.address(), attempt
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let Some(characteristic) = characteristic else {
        eprintln!("{} remote {}: characteristic never appeared: giving up", ts(), device.address());
        let _ = tokio::time::timeout(Duration::from_secs(5), device.disconnect()).await;
        return Ok(());
    };
    eprintln!("{} remote {}: characteristic found", ts(), device.address());

    let flags = characteristic.flags().await?;
    eprintln!(
        "{} remote {}: char flags: write={} write_without_response={} notify={} indicate={}",
        ts(), device.address(), flags.write, flags.write_without_response, flags.notify, flags.indicate
    );

    let notify = match characteristic.notify().await {
        Ok(n) => {
            eprintln!("{} remote {}: notification stream acquired", ts(), device.address());
            n
        }
        Err(e) => {
            eprintln!("{} remote {}: notify FAILED: {e}", ts(), device.address());
            return Err(e);
        }
    };
    pin_mut!(notify);

    // Outbound path: prefer the acquired write-without-response IO (one
    // syscall per packet, matches upstream); fall back to plain D-Bus
    // writes when acquisition is refused (some peripherals only accept
    // write-with-response, or the acquire races).
    enum Outbound {
        /// Acquired write-without-response IO.
        Io(bluer::gatt::CharacteristicWriter),
        /// Plain D-Bus writes (with response).
        Direct,
    }
    let mut outbound = if flags.write_without_response {
        match characteristic.write_io().await {
            Ok(io) => {
                eprintln!("{} remote {}: write-without-response IO acquired", ts(), device.address());
                Outbound::Io(io)
            }
            Err(e) => {
                eprintln!(
                    "{} remote {}: write IO refused ({e}): falling back to write-with-response",
                    ts(), device.address()
                );
                Outbound::Direct
            }
        }
    } else {
        eprintln!("{} remote {}: char has no write-without-response: using write-with-response", ts(), device.address());
        Outbound::Direct
    };

    loop {
        tokio::select! {
            value = notify.next() => {
                match value {
                    Some(bytes) => {
                        crate::radio_log!("{} remote {}: notification ({} bytes)", ts(), device.address(), bytes.len());
                        learn_peer(&bytes, device.address(), links);
                        if inbound_tx.send(bytes).await.is_err() {
                            return Ok(());
                        }
                    }
                    None => {
                        eprintln!("{} remote {}: notification stream ended (disconnected)", ts(), device.address());
                        return Ok(()); // unsubscribed / disconnected
                    }
                }
            }
            bytes = outbound_rx.recv() => {
                match bytes {
                    Some(bytes) => {
                        let result = match &mut outbound {
                            Outbound::Io(io) => io.write(&bytes).await.map(|n| n == bytes.len()),
                            Outbound::Direct => characteristic.write(&bytes).await.map(|_| true).map_err(std::io::Error::other),
                        };
                        match result {
                            Ok(true) => {}
                            Ok(false) => crate::radio_log!("{} remote {}: short write", ts(), device.address()),
                            Err(e) => crate::radio_log!("{} remote {}: write FAILED: {e}", ts(), device.address()),
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> bluer::Address {
        "AA:BB:CC:DD:EE:FF".parse().unwrap()
    }

    fn live_remotes() -> HashMap<bluer::Address, mpsc::Sender<Vec<u8>>> {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(1);
        HashMap::from([(addr(), tx)])
    }

    #[test]
    fn unicast_frames_route_to_a_known_live_link() {
        let peer = [7u8; 8];
        let peer_addr = HashMap::from([(peer, addr())]);
        let remotes = live_remotes();

        // Known peer with a live link: routed.
        assert_eq!(route_for(Some(peer), &peer_addr, &remotes), Some(addr()));
        // Unknown peer, broadcasts, and handoff frames: flood.
        assert_eq!(route_for(Some([9u8; 8]), &peer_addr, &remotes), None);
        assert_eq!(route_for(None, &peer_addr, &remotes), None);

        // Link died since the announce: fall back to flooding.
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(1);
        let remotes = HashMap::from([(bluer::Address::new([1, 2, 3, 4, 5, 6]), tx)]);
        assert_eq!(route_for(Some(peer), &peer_addr, &remotes), None);
    }
}
