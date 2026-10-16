#!/usr/bin/env bash
# Container-based build for backchat. The atomic host ships no compiler,
# same as orchard's setup: everything builds inside a pinned Fedora 43
# container (glibc 2.42, what freedesktop 25.08 needs), with the gpui native
# stack preinstalled (fontconfig, freetype, xkbcommon, clang for bindgen)
# and the rust toolchain baked into the image (self-contained, no volumes).
#
#   ./scripts/build.sh check
#   ./scripts/build.sh test
#   ./scripts/build.sh build --release
#
# First run builds the image (a few minutes):
#   podman build -t localhost/bitchat-dev-rust \
#     -f containers/bitchat-dev-rust.containerfile containers/

set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=localhost/bitchat-dev-rust

if ! podman image exists "$IMAGE"; then
  podman build -t "$IMAGE" -f containers/bitchat-dev-rust.containerfile containers/
fi

# Named volume so crates downloaded once survive `--rm` runs; the podman
# network is flaky (HTTP2 framing errors) and full re-fetches fail often.
exec podman run --rm \
  -v "$PWD":/work:Z \
  -v bitchat-cargo-registry:/usr/local/cargo/registry:Z \
  -w /work \
  -e PATH=/usr/local/cargo/bin:/usr/bin:/bin \
  "$IMAGE" \
  cargo "$@"
