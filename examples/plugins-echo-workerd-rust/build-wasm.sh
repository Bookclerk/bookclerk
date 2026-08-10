#!/usr/bin/env bash
# Build the Echo Wasm guest into modules/pkg/ for bookclerk-workerd.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EXAMPLE="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

rustup target add wasm32-unknown-unknown >/dev/null
cargo build -p bookclerk-plugin-echo-workerd-rust \
  --target wasm32-unknown-unknown --release

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
# Respect workspace `.cargo/config.toml` relative target-dir when unset in env
# but Cargo still wrote elsewhere — prefer the fresh artifact cargo reports.
WASM_OUT="$(
  cargo metadata --format-version 1 --no-deps \
    | python3 -c "
import json, os, sys
root = os.environ.get('ROOT', '')
meta = json.load(sys.stdin)
td = os.environ.get('CARGO_TARGET_DIR') or meta.get('target_directory') or 'target'
print(os.path.join(td, 'wasm32-unknown-unknown', 'release', 'bookclerk_plugin_echo_workerd_rust.wasm'))
" 
)"
if [[ ! -f "$WASM_OUT" ]]; then
  WASM_OUT="$TARGET_DIR/wasm32-unknown-unknown/release/bookclerk_plugin_echo_workerd_rust.wasm"
fi
test -f "$WASM_OUT"

if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "installing wasm-bindgen-cli…" >&2
  cargo install wasm-bindgen-cli --locked
fi

rm -rf "$EXAMPLE/modules/pkg"
mkdir -p "$EXAMPLE/modules/pkg"
wasm-bindgen "$WASM_OUT" \
  --target web \
  --out-dir "$EXAMPLE/modules/pkg" \
  --out-name bookclerk_plugin_echo_workerd_rust

rm -f "$EXAMPLE/modules/pkg"/*.d.ts 2>/dev/null || true
ls -la "$EXAMPLE/modules/pkg"
echo "wrote $EXAMPLE/modules/pkg"
