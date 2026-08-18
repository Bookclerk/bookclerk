#!/usr/bin/env bash
# Cursor Cloud Agent / Builds install — durable workspace warm-up.
# Idempotent: safe to re-run against a partially prepared checkout.
set -euo pipefail

# Resolve the workspace root from this script (.cursor/cloud-agent-install.sh),
# then cd there so manual runs and non-root CWDs still work.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

# Workspace-local caches (must not be baked into the image under /workspace).
# Keep /usr/local/cargo/bin first so Cloud uses the image rustc, not a host wrapper.
# shellcheck source=../scripts/workspace-env.sh
source "${ROOT}/scripts/workspace-env.sh"
export PATH="/usr/local/cargo/bin:${PATH}"

cargo fetch

if [[ -f ui/package-lock.json ]]; then
  (cd ui && npm ci && npm run build)
fi

# Full app graph (matches CI): platform hosts/helpers/workerd/sqlite/local,
# optional storefronts, and reference examples — so tests and
# `cargo dev --skip-build` start from a warm target/.
cargo build-app --platform --optional --examples
