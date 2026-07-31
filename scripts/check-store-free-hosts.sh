#!/usr/bin/env bash
# Assert that `--no-default-features` hosts link no store plugin and no cipher.
#
# Some regions restrict distributing a binary that can circumvent DRM, so a
# packager must be able to build `bookclerk` / `bookclerkd` with the Audible
# plugin left out. That only holds while nothing in the host graph reaches a
# cipher by another route: a shared crate that grew an `aes` dependency, or a
# plugin promoted from optional to required, would take the option away without
# breaking any test. Hence this check.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Crates that only a decrypt path needs. Anything sealing Bookclerk's own data
# uses XChaCha20-Poly1305, which is not on this list and must not be.
CIPHERS='^(aes|aes-gcm|cbc|ctr|aes-kw|widevine.*)$'

status=0

for host in bookclerk-cli bookclerkd; do
  tree="$(cargo tree -p "$host" --no-default-features --edges normal --prefix none --format '{lib}')"

  stores="$(grep -E '^bookclerk_plugin_(source|integration)_' <<<"$tree" | sort -u || true)"
  if [[ -n "$stores" ]]; then
    echo "FAIL: $host --no-default-features still links store plugins:" >&2
    sed 's/^/  /' <<<"$stores" >&2
    status=1
  fi

  ciphers="$(grep -E "$CIPHERS" <<<"$tree" | sort -u || true)"
  if [[ -n "$ciphers" ]]; then
    echo "FAIL: $host --no-default-features reaches cipher crates:" >&2
    sed 's/^/  /' <<<"$ciphers" >&2
    echo "  A store-free host must not be able to decrypt anything." >&2
    status=1
  fi

  [[ $status -eq 0 ]] && echo "ok: $host --no-default-features links no store and no cipher"
done

# A default build must still link them, or the check above passes for the boring
# reason that the feature stopped doing anything.
if ! cargo tree -p bookclerkd --edges normal --prefix none --format '{lib}' \
  | grep -q '^bookclerk_plugin_source_audible$'; then
  echo "FAIL: a default bookclerkd build no longer links the Audible plugin," >&2
  echo "  so the store-free check above proves nothing." >&2
  status=1
fi

exit "$status"
