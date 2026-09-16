#!/usr/bin/env bash
# Compile-time author-surface boundary check.
#
# 1. `tools/author-surface-check/positive` (sdk + documented `db` feature)
#    must `cargo check` cleanly — the intended third-party author API.
# 2. `tools/author-surface-check/negative` placeholder must `cargo check`
#    cleanly (harness control), then every `cases/*.rs` swapped into
#    `src/case.rs` must FAIL — host-private symbols are unreachable on
#    default features.
#
# Both fixture packages are excluded from the workspace so feature
# unification cannot leak the abi `host` feature into their graphs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POSITIVE="$ROOT/tools/author-surface-check/positive"
NEGATIVE="$ROOT/tools/author-surface-check/negative"
CASE_RS="$NEGATIVE/src/case.rs"

# Share the workspace target dir for caching; ignore any inherited RUSTFLAGS
# (e.g. -D warnings) so only hard resolution errors decide negative cases.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export RUSTFLAGS=""

echo "== author-surface: positive fixture must compile =="
cargo check --quiet --manifest-path "$POSITIVE/Cargo.toml"

PLACEHOLDER="$(cat "$CASE_RS")"
restore_placeholder() {
  printf '%s' "$PLACEHOLDER" > "$CASE_RS"
}
trap restore_placeholder EXIT

echo "== author-surface: negative harness control must compile =="
cargo check --quiet --manifest-path "$NEGATIVE/Cargo.toml"

fail=0
for case_file in "$NEGATIVE"/cases/*.rs; do
  name="$(basename "$case_file")"
  cp "$case_file" "$CASE_RS"
  if cargo check --quiet --manifest-path "$NEGATIVE/Cargo.toml" 2>/dev/null; then
    echo "FAIL: negative case compiled (host-private symbol leaked): $name" >&2
    fail=1
  else
    echo "ok (rejected): $name"
  fi
done

restore_placeholder
if [ "$fail" -ne 0 ]; then
  echo "author-surface check FAILED" >&2
  exit 1
fi
echo "author-surface check passed"
