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
# `--format '{lib}'` prints library names, so match `_` as well as `-`.
CIPHERS='^(aes|aes[-_]gcm|cbc|ctr|aes[-_]kw|widevine.*)( \(\*\))?$'

# The one allowed route to generic block ciphers: crypto-glue, which
# webauthn-rs (passkeys in bookclerkd) uses for its RustCrypto primitives and
# which links AES, CBC and CTR unconditionally. They verify nothing about
# media and come with no content keys or CDM; the binary already carries the
# same ciphers inside aws-lc-rs for TLS. The exception holds only while
# webauthn-rs is crypto-glue's sole user and the host reaches no cipher that
# crypto-glue does not itself bring. Widevine crates are never allowed.
GLUE='crypto-glue'
GLUE_USERS='^(webauthn-rs-core|webauthn-attestation-ca)$'

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

  ciphers="$(grep -E "$CIPHERS" <<<"$tree" | sed 's/ (\*)$//' | sort -u || true)"
  if [[ -n "$ciphers" ]] && grep -qx "${GLUE//-/_}" <<<"$tree"; then
    users="$(cargo tree -p "$host" --edges normal -i "$GLUE" --depth 1 --prefix none --format '{p}' |
      awk '{print $1}' | grep -vx "$GLUE" | sort -u)"
    strangers="$(grep -Ev "$GLUE_USERS" <<<"$users" || true)"
    if [[ -n "$strangers" ]]; then
      echo "FAIL: $host (default) has $GLUE users other than webauthn-rs:" >&2
      sed 's/^/  /' <<<"$strangers" >&2
      status=1
    else
      glue_ciphers="$(cargo tree -p "$GLUE" --edges normal --prefix none --format '{lib}' |
        grep -E "$CIPHERS" | sed 's/ (\*)$//' | grep -v '^widevine' | sort -u || true)"
      ciphers="$(comm -23 <(printf '%s\n' "$ciphers") <(printf '%s\n' "$glue_ciphers") | sed '/^$/d')"
    fi
  fi
  if [[ -n "$ciphers" ]]; then
    echo "FAIL: $host (default) reaches cipher crates:" >&2
    sed 's/^/  /' <<<"$ciphers" >&2
    echo "  A store-free host must not be able to decrypt anything." >&2
    status=1
  fi

  [[ $status -eq 0 ]] && echo "ok: $host (default) links no store and no cipher outside passkeys"
done

exit "$status"
