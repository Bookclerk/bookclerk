#!/usr/bin/env bash
# Runs inside `docker run --memory=1g --memory-swap=1g --cpus=1`.
# The release binaries are already built on the host.
# GitHub's job-container shell is dash; this file is started with bash.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends python3 curl ca-certificates libssl3
rm -rf /var/lib/apt/lists/*

[[ -n "${ENVELOPE_SEED_BIN:-}" && -x "${ENVELOPE_SEED_BIN}" ]] || {
  echo "envelope: seed binary is not available inside the container" >&2
  exit 1
}
"${ENVELOPE_SEED_BIN}" --ignored --nocapture

set +e
bash "${ROOT}/scripts/bench/small-vps.sh"
status=$?
set -e
if [[ -n "${BOOKCLERK_FILES_DIR:-}" ]]; then
  chmod -R a+rX "$(dirname "${BOOKCLERK_FILES_DIR}")" || true
fi
exit "${status}"
