#!/usr/bin/env bash
# Cursor Cloud Agent / Builds install — durable workspace warm-up.
# Idempotent: safe to re-run against a partially prepared checkout.
set -euo pipefail

export PATH="/usr/local/cargo/bin:${PATH}"

mkdir -p .cargo-home .tmp BookclerkFiles

cargo fetch

if [[ -f ui/package-lock.json ]]; then
  (cd ui && npm ci && npm run build)
fi

# Full app graph (matches CI): platform hosts/helpers/workerd/sqlite/local,
# optional storefronts, and reference examples — so tests and
# `cargo dev --skip-build` start from a warm target/.
cargo build-app --platform --optional --examples
