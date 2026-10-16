//! The bluer radio layer: peripheral (advertise + local GATT) *and* central
//! (scan + connect) on one adapter, mirroring upstream's dual-role
//! `BLEService`.
//!
//! v1 simplification: every outbound frame is written to *all* live links
//! (peripheral subscribers + connected remote devices). Unicast packets
//! still carry their recipient in the wire header, so phones route them
//! correctly; dedicated per-peer link tables are a later port of
//! upstream's connection scheduler.

use std::collections::HashMap;
use std::time::Duration;

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

use crate::{MeshConfig, CHARACTERISTIC_UUID};

/// Raw packet bytes destined for the radio (broadcast in v1).
pub type OutboundTx = mpsc::Sender<Vec<u8>>;
/// Raw packet bytes received from any radio path.
pub type InboundTx = mpsc::Sender<Vec<u8>>;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("bluer: {0}")]
    Bluer(#[from] bluer::Error),
    #[error("no bluetooth adapter")]
    NoAdapter,
}

/// Runs the link layer until the returned task is aborted. Returns the
/// outbound sender the node pushes packets into.
pub async fn spawn(
    config: &MeshConfig,
    inbound_tx: InboundTx,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
) -> Result<tokio::task::JoinHandle<()>, LinkError> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let service_uuid = config.network.service_uuid();

    // Shared link state: peripheral notify writers (one per subscribed
    // central) and connected remote devices (central role).
    struct Links {
        writers: Vec<bluer::gatt::CharacteristicWriter>,
        remotes: HashMap<bluer::Address, mpsc::Sender<Vec<u8>>>,
    }
    let links = std::sync::Arc::new(tokio::sync::Mutex::new(Links {
        writers: Vec::new(),
        remotes: HashMap::new(),
    }));
    // BlueZ runs one connection procedure at a time — concurrent Connect
    // calls fail with "In Progress".
    let connect_lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));

    // ---- Peripheral: advertisement + local GATT ----------------------------
    let advertisement = Advertisement {
        advertisement_type: AdvType::Peripheral,
        service_uuids: [service_uuid].into_iter().collect(),
        discoverable: Some(true),
        local_name: Some(config.nickname.clone()),
        ..Default::default()
    };
    let _adv_handle = adapter.advertise(advertisement).await?;

    let (char_control, char_control_handle) = characteristic_control();
    let application = Application {
        services: vec![Service {
            uuid: service_uuid,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: CHARACTERISTIC_UUID,
                write: Some(CharacteristicWrite {
                    write_without_response: true,
                    method: CharacteristicWriteMethod::Io,
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
    let _app_handle = adapter.serve_gatt_application(application).await?;

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
    let discovery = {
        let links = links.clone();
        let inbound_tx = inbound_tx.clone();
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
                let events = match adapter.discover_devices().await {
                    Ok(events) => events,
                    Err(_) => return,
                };
                pin_mut!(events);
                while let Some(event) = events.next().await {
                    match event {
                        AdapterEvent::DeviceAdded(addr) => {
                            if own_address == addr || links.lock().await.remotes.contains_key(&addr) {
                                continue;
                            }
                            let links = links.clone();
                            let device = match adapter.device(addr) {
                                Ok(device) => device,
                                Err(_) => continue,
                            };
                            let uuids = device.uuids().await.unwrap_or_default().unwrap_or_default();
                            if !uuids.contains(&service_uuid) {
                                continue;
                            }
                            eprintln!(
                                "{} discovery: bitchat peer at {} (name {:?}) — connecting as central",
                                ts(), addr, device.name().await.ok().flatten().unwrap_or_default()
                            );
                            let inbound = inbound_tx.clone();
                            let connect_lock = connect_lock.clone();
                            let (remote_tx, remote_rx) = mpsc::channel::<Vec<u8>>(64);
                            links.lock().await.remotes.insert(addr, remote_tx.clone());
                            tokio::spawn(async move {
                                let mut remote_rx = remote_rx;
                                let result =
                                    run_remote(&device, service_uuid, &mut remote_rx, &inbound, &connect_lock).await;
                                eprintln!("{} remote {}: link ended: {result:?}", ts(), device.address());
                                links.lock().await.remotes.remove(&addr);
                            });
                        }
                        AdapterEvent::DeviceRemoved(_) => {}
                        _ => {}
                    }
                }
            }
        })
    };

    // ---- Outbound pump: write every frame to every live link ---------------
    let outbound_task = tokio::spawn(async move {
        while let Some(bytes) = outbound_rx.recv().await {
            let mut links = links.lock().await;
            eprintln!(
                "{} transmit: {} bytes → {} peripheral subscriber(s), {} central link(s)",
                ts(), bytes.len(), links.writers.len(), links.remotes.len()
            );
            for writer in &mut links.writers {
                if let Err(e) = writer.write_all(&bytes).await {
                    eprintln!("{} transmit: peripheral write failed: {e}", ts());
                }
            }
            for remote in links.remotes.values() {
                if let Err(e) = remote.try_send(bytes.clone()) {
                    eprintln!("{} transmit: central write failed: {e}", ts());
                }
            }
        }
    });

    drop(discovery); // task keeps running via tokio::spawn; handle unused
    Ok(outbound_task)
}

/// Central-role connection to one remote device: acquire write-without-
/// response IO + notifications, then pump both directions until it goes away.
async fn run_remote(
    device: &bluer::Device,
    service_uuid: uuid::Uuid,
    outbound_rx: &mut mpsc::Receiver<Vec<u8>>,
    inbound_tx: &InboundTx,
    connect_lock: &tokio::sync::Mutex<()>,
) -> bluer::Result<()> {
    {
        // Serialize connection procedures across all remotes.
        let _guard = connect_lock.lock().await;
        eprintln!("{} remote {}: connecting", ts(), device.address());
        if !device.is_connected().await? {
            let mut retries = 3;
            loop {
                match device.connect().await {
                    Ok(()) => break,
                    Err(err) if retries > 0 => {
                        eprintln!("{} remote {}: connect failed ({err}), retrying", ts(), device.address());
                        retries -= 1;
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    }
                    Err(err) => {
                        eprintln!("{} remote {}: connect FAILED permanently: {err}", ts(), device.address());
                        // The LE controller accumulates zombie connections when
                        // clients die mid-connection; scans then return nothing
                        // and every connect D-Bus-times out. Point the user at
                        // the host-side reset instead of failing silently.
                        let msg = err.to_string();
                        if msg.contains("Timeout") || msg.contains("NotReady") {
                            eprintln!(
                                "{} !! adapter looks wedged (D-Bus timeouts / NotReady) —\n     run scripts/ble-reset.sh on the HOST, then restart the app",
                                ts()
                            );
                        }
                        return Err(err);
                    }
                }
            }
        }
    }
    eprintln!("{} remote {}: connected", ts(), device.address());

    // Locate the characteristic. Right after connect() the remote GATT db
    // is sometimes not yet resolved — retry briefly before giving up.
    let mut characteristic = None;
    for attempt in 0..10 {
        for service in device.services().await? {
            if service.uuid().await? != service_uuid {
                continue;
            }
            for char in service.characteristics().await? {
                if char.uuid().await? == CHARACTERISTIC_UUID {
                    characteristic = Some(char);
                    break;
                }
            }
            if characteristic.is_some() { break; }
        }
        if characteristic.is_some() {
            break;
        }
        eprintln!("{} remote {}: service not resolved yet (attempt {})", ts(), device.address(), attempt);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let Some(characteristic) = characteristic else {
        eprintln!("{} remote {}: characteristic never appeared — giving up", ts(), device.address());
        return Ok(());
    };
    eprintln!("{} remote {}: characteristic found", ts(), device.address());

    let flags = characteristic.flags().await?;
    eprintln!(
        "{} remote {}: char flags — write={} write_without_response={} notify={} indicate={}",
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
                    "{} remote {}: write IO refused ({e}) — falling back to write-with-response",
                    ts(), device.address()
                );
                Outbound::Direct
            }
        }
    } else {
        eprintln!("{} remote {}: char has no write-without-response — using write-with-response", ts(), device.address());
        Outbound::Direct
    };

    loop {
        tokio::select! {
            value = notify.next() => {
                match value {
                    Some(bytes) => {
                        eprintln!("{} remote {}: notification ({} bytes)", ts(), device.address(), bytes.len());
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
                            Ok(false) => eprintln!("{} remote {}: short write", ts(), device.address()),
                            Err(e) => eprintln!("{} remote {}: write FAILED: {e}", ts(), device.address()),
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}
