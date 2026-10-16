//! Minimal relay client: websocket sessions to a set of relays, REQ/EVENT
//!/NIP-01 message handling. The seam the GPUI app talks to.

// TODO(M1): RelayClient (tokio-tungstenite) — connect, subscribe by tag,
// publish, reconnect with backoff, surface events to the app. Default
// relay set lives in lib.rs; the GPS-tagged directory from upstream
// (relays/online_relays_gps.csv → nearest-N selection) lands with location
// support.
