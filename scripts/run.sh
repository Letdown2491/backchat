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
exec podman run --rm -i \
  -v "$PWD":/work -w /work \
  -e PATH=/usr/local/cargo/bin:/usr/bin:/bin \
  --security-opt label=disable \
  -v /run/user/"$(id -u)":"/run/user/$(id -u)" \
  -e XDG_RUNTIME_DIR="/run/user/$(id -u)" \
  -e WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}" \
  -v /usr/share/fonts:/usr/share/fonts:ro \
  localhost/bitchat-dev-rust \
  ./target/debug/bitchat
