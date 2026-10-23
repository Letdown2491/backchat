#!/usr/bin/env bash
# Regenerate the vendored crate sources (vendor/) used by all builds.
# vendor/ is not committed (about 1 GB); a fresh clone must run this once
# before cargo will build - .cargo/config.toml points cargo at vendor/.
#
# Prefers the pinned container (like every other build path); falls back to
# any system cargo (CI runners: rustup with a 1.85+ toolchain for edition
# 2024). Only downloads sources, so the toolchain version does not matter.
set -euo pipefail
cd "$(dirname "$0")/.."

if podman image exists localhost/bitchat-dev-rust 2>/dev/null; then
  ./scripts/build.sh vendor vendor
else
  if command -v cargo >/dev/null; then
    cargo vendor vendor
  else
    echo "no podman image and no system cargo: install rustup (1.85+) or build" >&2
    echo "the container first (scripts/build.sh builds it on first use)" >&2
    exit 1
  fi
fi

# Reapply the gpui list patches on the fresh vendor copy. cargo vendor
# writes original checksums, so rehash whatever the patch touched.
./scripts/apply-gpui-patches.sh
