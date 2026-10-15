# bitchat-rs

A Rust/GPUI reimplementation of [bitchat](https://github.com/permissionlesstech/bitchat)
for Linux: bluetooth mesh chat, IRC vibes. Drop-in wire compatible with the
upstream iOS/Android clients.

Not a code fork — see [PORTING.md](PORTING.md) for how upstream is tracked
and which Swift files define wire behavior.

## Layout

- `crates/bitchat-core` — pure wire format and mesh semantics: packet codec,
  TLVs, Noise_XX sessions, flood control. No I/O. Checked against upstream
  fixtures in `fixtures/`.
- `crates/bitchat-nostr` — Nostr transport: geohash channels, `bitchat1:`
  embedded packets, the private-envelope scheme.
- `crates/bitchat-ble` — Linux BLE mesh via BlueZ (`bluer`), central +
  peripheral roles.
- `crates/bitchat-app` — the GPUI client.
- `fixtures/` — conformance vectors ported from upstream's test suite.
- `self/`, `scripts/build.sh` (planned) — flatpak tree + Orchard publish,
  modeled on orchard's own self-publishing setup.

## Status

M0 scaffold. See [PORTING.md](PORTING.md) for the milestone list and the
upstream tracking procedure.

## License

Public domain (Unlicense), matching upstream.
