#!/usr/bin/env bash
# Container-based build for bitchat-rs. The atomic host ships no compiler,
# same as orchard's setup: everything builds inside a pinned Fedora 43
# container (glibc 2.42, what freedesktop 25.08 needs), with the gpui native
# stack preinstalled (fontconfig, freetype, xkbcommon, clang for bindgen).
#
#   ./scripts/build.sh check
#   ./scripts/build.sh test
#   ./scripts/build.sh build --release
#
# First run builds the image:
#   podman build -t localhost/bitchat-dev-rust \
#     -f containers/bitchat-dev-rust.containerfile containers/

set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=localhost/bitchat-dev-rust
# Cache volumes are ours, not orchard's, so toolchain upgrades stay independent.
CARGO_VOL=bitchat-cargo-home
RUSTUP_VOL=bitchat-rustup-home

if ! podman image exists "$IMAGE"; then
  podman build -t "$IMAGE" -f containers/bitchat-dev-rust.containerfile containers/
fi

exec podman run --rm \
  -v "$PWD":/work:Z -w /work \
  -v "$CARGO_VOL":/root/.cargo \
  -v "$RUSTUP_VOL":/root/.rustup \
  "$IMAGE" \
  bash -c '
    if [ ! -x /root/.cargo/bin/cargo ]; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.98.1
    fi
    export PATH=/root/.cargo/bin:$PATH
    cargo "$@"
  ' _ "$@"
