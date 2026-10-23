# Changelog

## Unreleased

## v0.2.0 (2026-10-22)

- **The app pings you when it is not looking.** Mentions and private
  messages arriving while the window is unfocused raise desktop
  notifications through the xdg-desktop-portal (the path that works
  inside the flatpak sandbox), and the people rail tracks unread
  badges per conversation, cleared the moment you open the chat. A
  missing portal service means notifications off, not a crash.
- **Mesh peers can be verified.** `/fingerprint` in a private chat
  shows both sides' fingerprint blocks (uppercase hex, groups of
  four, one line per 16 chars) so two humans can compare them out of
  band or over a call; `/verify` records the match in a persisted
  verified set and the verification output lands in that
  conversation's log instead of the mesh log.
- **Channel discovery shows what is alive.** The `#` browser counts
  participants the same way the phone does (a 300-second window
  including you), remembers your recently visited cells for the
  "your channels" section, and the discovery scan subscription is
  toggled off the moment you leave the browser.
- **The chat log stopped overlapping and became actually scrollable.**
  Rows now reserve their true wrapped height (gpui sized them from
  the unwrapped text and never propagated the wrapped measure, so
  multi-line messages drew over each other), and a long log scrolls
  to the end: appended rows land in one splice, the pin-to-bottom
  follow releases the moment you wheel upward, and the 1000-line cap
  no longer resets your scroll position.
- **Relay connections heal without thrashing.** A relay that fails
  five connects in a row drops to one quiet probe every ten minutes
  instead of a retry storm, the status banner counts only relays
  that actually connected, and swapping the relay set (moving to a
  new geohash, a geo fix mid-session) rebuilds sessions once and
  replays the subscription set so no traffic is lost.
- **Considerably leaner under load.** Bursts of relay or mesh events
  land in one store update and one render instead of one per event
  (duplicates from multiple relays included), the active log is
  snapshotted once per change instead of cloned every frame, and all
  in-memory tables are bounded (presence, DM conversations, pending
  handshakes, seen events). An idle window sits at zero CPU.
- **A security pass over every untrusted surface.** The device seed
  and settings now live in owner-only files (0600, directory 0700;
  existing installs are repaired on load), oversized pastes cannot
  hang the composer, over-limit mesh DMs are refused with an honest
  log line instead of panicking the radio, inbound nicknames are
  stripped of control characters and the RTL override, and relay
  websocket frames are capped at 1 MiB against a hostile relay.

## v0.1.1 (2026-10-22)

- **First aarch64 release.** Linux phones and ARM laptops install the
  same listing as x86_64. Mesh vocabulary unified across the client,
  radio logging calmed.

## v0.1.0 (2026-10-16)

- **First packaging.** BLE mesh dual-role radio, geohash channels,
  private messages, and the people-rail client.
