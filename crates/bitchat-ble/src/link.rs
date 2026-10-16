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
                            let inbound = inbound_tx.clone();
                            let (remote_tx, remote_rx) = mpsc::channel::<Vec<u8>>(64);
                            links.lock().await.remotes.insert(addr, remote_tx.clone());
                            tokio::spawn(async move {
                                let _ = run_remote(
                                    device, service_uuid, remote_rx, inbound,
                                )
                                .await;
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
            for writer in &mut links.writers {
                let _ = writer.write_all(&bytes).await;
            }
            for remote in links.remotes.values() {
                let _ = remote.try_send(bytes.clone());
            }
        }
    });

    drop(discovery); // task keeps running via tokio::spawn; handle unused
    Ok(outbound_task)
}

/// Central-role connection to one remote device: acquire write-without-
/// response IO + notifications, then pump both directions until it goes away.
async fn run_remote(
    device: bluer::Device,
    service_uuid: uuid::Uuid,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    inbound_tx: InboundTx,
) -> bluer::Result<()> {
    // Connect with retries (upstream also retries).
    if !device.is_connected().await? {
        let mut retries = 3;
        loop {
            match device.connect().await {
                Ok(()) => break,
                Err(err) if retries > 0 => {
                    retries -= 1;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = err;
                }
                Err(err) => return Err(err),
            }
        }
    }

    // Locate the characteristic.
    let mut characteristic = None;
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
    }
    let Some(characteristic) = characteristic else {
        return Ok(());
    };

    let mut write_io = characteristic.write_io().await?;
    let notify = characteristic.notify().await?;
    pin_mut!(notify);

    loop {
        tokio::select! {
            value = notify.next() => {
                match value {
                    Some(bytes) => {
                        if inbound_tx.send(bytes).await.is_err() {
                            return Ok(());
                        }
                    }
                    None => return Ok(()), // unsubscribed / disconnected
                }
            }
            bytes = outbound_rx.recv() => {
                match bytes {
                    Some(bytes) => {
                        // write_io is unbuffered; single write per packet.
                        if write_io.write(&bytes).await.is_err() {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}
