#!/usr/bin/env bash
# Echo workerd guest conformance (describe + role health + cliInvoke) against a
# real Cloudflare workerd binary (no shim). Used by workerd-pin-bump CI.
#
# Drives the canonical staged-plugin harness (`cargo test-staged`), which
# installs platform guests, stages the Echo examples, and exercises every
# staged guest over the product Cap'n Proto ABI — including the workerd
# guests via the bookclerk-workerd launcher.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PROFILE="${1:-debug}"
ARGS=(run -p bookclerk-dev --)
if [[ "$PROFILE" == "release" ]]; then
  ARGS+=(--release)
fi
ARGS+=(test-staged)

cargo "${ARGS[@]}"

echo "all workerd echo guests passed"
