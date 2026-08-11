#!/usr/bin/env bash
# Cursor Cloud Agent / Builds install — durable workspace warm-up.
# Idempotent: safe to re-run against a partially prepared checkout.
set -euo pipefail

# Cloud shells may omit /usr/local/cargo/bin; the image also symlinks into
# /usr/local/bin, but keep an explicit prepend for robustness.
export PATH="/usr/local/cargo/bin:${PATH}"

# Resolve the workspace root from this script (.cursor/cloud-agent-install.sh),
# then cd there so manual runs and non-root CWDs still work.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

# Workspace-local caches (must not be baked into the image under /workspace).
export CARGO_HOME="${CARGO_HOME:-${ROOT}/.cargo-home}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"
export TMPDIR="${TMPDIR:-${ROOT}/.tmp}"
export BOOKCLERK_FILES_DIR="${BOOKCLERK_FILES_DIR:-${ROOT}/BookclerkFiles}"

mkdir -p "${CARGO_HOME}" "${CARGO_TARGET_DIR}" "${TMPDIR}" "${BOOKCLERK_FILES_DIR}"

cargo fetch

if [[ -f ui/package-lock.json ]]; then
  (cd ui && npm ci && npm run build)
fi

# Full app graph (matches CI): platform hosts/helpers/workerd/sqlite/local,
# optional storefronts, and reference examples — so tests and
# `cargo dev --skip-build` start from a warm target/.
cargo build-app --platform --optional --examples
