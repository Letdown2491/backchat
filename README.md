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

**M1 working: Nostr transport.** You can join geohash channels and chat
drop-in with real bitchat iOS/Android clients over the internet.

Verified so far:
- private envelopes (kind 13/14/1059) decrypt real Android- and iOS-emitted
  fixtures (`crates/bitchat-nostr/tests/envelope_fixtures.rs`)
- live publish/subscribe round-trip against `bitchat.nostr1.com`
  (`crates/bitchat-nostr/tests/probe_live.rs`, network test, `--ignored`)
- BLE mesh (M2) is stubbed; see [PORTING.md](PORTING.md).

## Running

No compiler on the host — everything runs in the pinned Fedora 43 container:

```sh
podman build -t localhost/bitchat-dev-rust -f containers/bitchat-dev-rust.containerfile .
./scripts/run.sh
```

A window opens. `/join <geohash>` picks the 5 relays nearest that cell (from
upstream's GPS relay directory) and announces you; `/nick <name>` sets your
display name; `/help` lists commands. State lives in
`~/.local/state/bitchat-rs/` — the device seed rotates every geohash
identity, same unlinkability model as upstream.

Tests: `./scripts/build.sh test` (42 tests, all offline; one network probe
behind `--ignored`).

## License

Public domain (Unlicense), matching upstream.
