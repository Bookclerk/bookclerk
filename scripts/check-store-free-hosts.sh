#!/usr/bin/env bash
# Assert that default hosts link no store plugin and no cipher.
#
# Some regions restrict distributing a binary that can circumvent DRM, so a
# packager must be able to build `bookclerk` / `bookclerkd` without linking a
# store. That only holds while nothing in the host graph reaches a cipher by
# another route: a shared crate that grew an `aes` dependency, or a plugin
# promoted from optional to required, would take the option away without
# breaking any test. Hence this check.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Crates that only a decrypt path needs. Anything sealing Bookclerk's own data
# uses XChaCha20-Poly1305, which is not on this list and must not be.
CIPHERS='^(aes|aes-gcm|cbc|ctr|aes-kw|widevine.*)$'

status=0

# Check the actual default feature set (empty: external guests only).
# Do not pass --no-default-features — that would stop matching the packaged
# default build if defaults ever gain features. Storefronts are staged guests;
# SQLite, local output, and optional database adapters ship as staged guests.
# Production hosts must not link adapter implementation crates for lowering.
for host in bookclerk-cli bookclerkd; do
  tree="$(cargo tree -p "$host" --edges normal --prefix none --format '{lib}')"

  stores="$(grep -E '^bookclerk_plugin_(source|integration|destination|database)_' <<<"$tree" | sort -u || true)"
  if [[ -n "$stores" ]]; then
    echo "FAIL: $host (default) still links store/destination plugin crates:" >&2
    sed 's/^/  /' <<<"$stores" >&2
    status=1
  fi

  ciphers="$(grep -E "$CIPHERS" <<<"$tree" | sort -u || true)"
  if [[ -n "$ciphers" ]]; then
    echo "FAIL: $host (default) reaches cipher crates:" >&2
    sed 's/^/  /' <<<"$ciphers" >&2
    echo "  A store-free host must not be able to decrypt anything." >&2
    status=1
  fi

  [[ $status -eq 0 ]] && echo "ok: $host (default) links no store and no cipher"
done

exit "$status"
