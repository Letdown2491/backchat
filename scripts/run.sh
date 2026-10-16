#!/usr/bin/env bash
# Run the bitchat desktop app from inside the build container against the
# host's Wayland session (no compiler on the atomic host). Usage:
#   ./scripts/run.sh            # build + run
#   ./scripts/run.sh            # forwarded args go to cargo build
set -euo pipefail
cd "$(dirname "$0")/.."

# Build first (cached; cheap after the first run).
./scripts/build.sh build "$@"

# The runtime needs: the Wayland socket, fonts (the container has none),
# and SELinux labeling off so the socket is connectable.
# BLE mesh: BlueZ talks over the system D-Bus socket; the mesh identity
# (Noise + signing keys) persists in .data so restarts keep the same peer ID.
# --userns=keep-id is REQUIRED for the D-Bus auth: dbus-broker checks that
# the EXTERNAL auth uid matches the socket credentials (SO_PEERCRED); as
# container root we present uid 0 while the host sees uid 1000 → the bus
# rejects every call ("NoReply"). keep-id makes the uids agree.
exec podman run --rm -i --userns=keep-id \
  -v "$PWD":/work -w /work \
  -e PATH=/usr/local/cargo/bin:/usr/bin:/bin \
  --security-opt label=disable \
  -v /run/user/"$(id -u)":"/run/user/$(id -u)" \
  -e XDG_RUNTIME_DIR="/run/user/$(id -u)" \
  -e WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}" \
  -v /usr/share/fonts:/usr/share/fonts:ro \
  -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket \
  -e XDG_DATA_HOME=/work/.data \
  -e BACKCHAT_DEBUG="${BACKCHAT_DEBUG:-}" \
  -e BACKCHAT_NO_CENTRAL="${BACKCHAT_NO_CENTRAL:-}" \
  -e DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/dbus/system_bus_socket \
  localhost/bitchat-dev-rust \
  ./target/debug/backchat
