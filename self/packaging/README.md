# self: flatpak packaging + Orchard publishing

Everything flatpak lives here. The app id is `io.github.Letdown2491.backchat`
(reverse-DNS from the GitHub repo).

## Layout

- `packaging/io.github.Letdown2491.backchat.yml`: the flatpak manifest
  (freedesktop 25.08 runtime, rust-stable sdk extension).
- `packaging/*.desktop`, `*.metainfo.xml`, `*.svg`: appstream + launcher
  metadata, installed by the manifest.
- `tree/`: flatpak-builder's work area (gitignored).

## Dependencies of the build

One-time, on the host (the host has no compiler; the SDK does the build):

```
flatpak install -y --system flathub \
  org.freedesktop.Platform//25.08 \
  org.freedesktop.Sdk//25.08 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08 \
  org.flatpak.Builder
```

Crate sources are vendored at the repo root (`vendor/`, about 1 GB,
gitignored): regenerate with `scripts/vendor.sh` after any Cargo.toml or
Cargo.lock change. The manifest builds with `--offline`, so flatpak's
no-network sandbox is honored.

## Build, install, run (local test)

```
flatpak run --command=flatpak-builder org.flatpak.Builder \
  --user --install --force-clean self/tree \
  self/packaging/io.github.Letdown2491.backchat.yml

flatpak run io.github.Letdown2491.backchat
```

The app keeps its state under `~/.var/app/io.github.Letdown2491.backchat/`
(flatpak's sandboxed HOME): the same `state/backchat` tree as a bare run,
one level deeper.

## Permissions (finish-args, why each exists)

- `--system-talk-name=org.bluez`: BLE mesh. bluer speaks D-Bus to BlueZ
  for advertising, scanning, and GATT in both roles. Without this the
  radio cannot start.
- `--system-talk-name=org.freedesktop.DBus`: the system bus itself, so
  discovery and adapter property calls route through dbus-broker.
- `--socket=wayland` / `--socket=x11`: windowing (wayland primary, X11
  fallback), `--device=dri` for GPU rendering.
- `--share=network`: nostr relays for geohash channels and sync.

## Publishing to Orchard

Build an export tree and hand it to Orchard's publisher flow:

```
flatpak run --command=flatpak-builder org.flatpak.Builder \
  --force-clean --repo=self/tree/export self/tree/stage \
  self/packaging/io.github.Letdown2491.backchat.yml

# single-file bundle alternative:
flatpak build-bundle self/tree/export \
  backchat-0.1.0.flatpak io.github.Letdown2491.backchat stable
```

For a source build on Orchard's side, the manifest needs two edits:
swap the local `dir` source for the git URL + tag, and keep the vendor
directory reachable (commit it to the publish branch, or generate it in
a prep module). The `*.key` files for repo signing never enter this
repository (gitignored).
