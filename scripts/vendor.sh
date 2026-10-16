#!/usr/bin/env bash
# Regenerate the vendored crate sources (vendor/) used by all builds.
# vendor/ is not committed (about 1 GB); a fresh clone must run this once
# before cargo will build - .cargo/config.toml points cargo at vendor/.
set -euo pipefail
cd "$(dirname "$0")/.."
exec ./scripts/build.sh vendor vendor
