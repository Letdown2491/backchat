// One-off network probe (not a CI test): subscribe via the manager, publish
// from a second connection, confirm the event comes back. Run manually:
//   cargo test -p bitchat-nostr --test probe_live -- --nocapture

use bitchat_nostr::event::NostrEvent;
use bitchat_nostr::relay::{self, RelayCommand, RelayEvent, Subscription};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "network test: run manually with --ignored"]
async fn live_subscribe_publish_roundtrip() {
    let relay = "wss://bitchat.nostr1.com".to_string();
    let (cmd_tx, mut evt_rx) = relay::spawn_relay_manager(vec![relay.clone()]);

    let geohash = "dr5rs";
    let filter = relay::geohash_filter(geohash, 600);
    cmd_tx.send(RelayCommand::Subscribe(vec![Subscription::new(filter)])).unwrap();

    // Build and publish an event from a second connection.
    let sender = bitchat_nostr::Identity::generate();
    let event = bitchat_nostr::geohash::create_ephemeral_geohash_event(
        "bitchat-rs live probe",
        geohash,
        &sender,
        Some("probe"),
        false,
    );
    let expected_id = event.id.clone();
    // Give the manager a moment to connect + subscribe first.
    tokio::time::sleep(Duration::from_secs(3)).await;
    relay::publish_once(&relay, &event).await.expect("publish");

    // Expect our own event back through the manager subscription.
    let deadline = Duration::from_secs(15);
    let got = tokio::time::timeout(deadline, async {
        while let Some(evt) = evt_rx.recv().await {
            if let RelayEvent::Received { event, .. } = evt {
                if event.id == expected_id {
                    return true;
                }
            }
        }
        false
    })
    .await;
    assert!(got.unwrap_or(false), "did not receive our own event back");
}
