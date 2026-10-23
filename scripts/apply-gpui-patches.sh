#!/usr/bin/env bash
# Reapply the gpui list patches after vendoring (scripts/vendor.sh runs
# this). Each file under patches/gpui-<version>/ replaces the matching
# path inside vendor/gpui/, then .cargo-checksum.json is rehashed for
# everything touched, since cargo vendor wrote original hashes.
set -euo pipefail
cd "$(dirname "$0")/.."

GPUI_VERSION=0.2.2
patch_dir="patches/gpui-$GPUI_VERSION"
vendor_dir="vendor/gpui"

if [ ! -d "$vendor_dir" ]; then
  echo "no $vendor_dir: run scripts/vendor.sh first" >&2
  exit 1
fi

changed=()
for file in "$patch_dir"/*.rs; do
  [ -e "$file" ] || continue
  rel="src/elements/$(basename "$file")"
  cp "$file" "$vendor_dir/$rel"
  changed+=("$rel")
done

if [ ${#changed[@]} -eq 0 ]; then
  echo "no gpui patches found in $patch_dir" >&2
  exit 1
fi

python3 - "$vendor_dir" "${changed[@]}" <<'EOF'
import hashlib, json, sys
vendor_dir, rels = sys.argv[1], sys.argv[2:]
path = f"{vendor_dir}/.cargo-checksum.json"
with open(path) as f:
    manifest = json.load(f)
for rel in rels:
    with open(f"{vendor_dir}/{rel}", "rb") as f:
        manifest["files"][rel] = hashlib.sha256(f.read()).hexdigest()
with open(path, "w") as f:
    json.dump(manifest, f)
EOF

echo "applied gpui $GPUI_VERSION patches: ${changed[*]}"
