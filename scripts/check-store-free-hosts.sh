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
# same ciphers inside aws-lc-rs for TLS. Widevine crates are never allowed.
#
# The exception is checked edge by edge, not by name, so a second route to a
# cipher still fails:
# - webauthn-rs is crypto-glue's only user;
# - every crate that depends on a cipher (or on a crate carrying one: aes-gcm,
#   aes-kw, pkcs5) is itself one of those crates, crypto-glue, or pkcs8;
# - pkcs8 reaches pkcs5 only through its `encryption` / `pkcs5` features, and
#   only crypto-glue turns those on.
GLUE='crypto-glue'
GLUE_USERS='^(webauthn-rs-core|webauthn-attestation-ca)$'
GLUE_CRATES='aes aes-gcm aes-kw cbc ctr pkcs5'
GLUE_PARENTS='^(aes|aes-gcm|aes-kw|cbc|ctr|pkcs5|pkcs8|crypto-glue)$'

# Set when any `cargo tree` query below fails. The helpers run in command
# substitutions, whose exit status a `for` list drops, so a failure is
# recorded here and the check fails on it rather than reading a query error
# as "no dependents".
QUERY_FAILED="$(mktemp)"
rm -f "$QUERY_FAILED"
trap 'rm -f "$QUERY_FAILED"' EXIT

# Runs `cargo tree` with the given arguments and prints its output; on
# failure prints Cargo's error, records it in QUERY_FAILED and returns 2.
tree_or_die() {
  local out
  if ! out="$(cargo tree "$@" 2>&1)"; then
    echo "FAIL: cargo tree $* failed:" >&2
    sed 's/^/  /' <<<"$out" >&2
    : >"$QUERY_FAILED"
    return 2
  fi
  printf '%s\n' "$out"
}

# Every resolved `name@version` of package `$2` in host `$1`'s normal graph
# (one line per version; nothing when the package is absent).
versions_of() {
  local listing
  listing="$(tree_or_die -p "$1" --edges normal --prefix none --format '{p}')" || return 2
  awk -v name="$2" '$1 == name { sub(/^v/, "", $2); print name "@" $2 }' <<<"$listing" | sort -u
}

# Crates that depend directly on package spec `$2` (`name@version`) in host
# `$1`'s normal graph.
parents() {
  local listing
  listing="$(tree_or_die -p "$1" --edges normal -i "$2" --depth 1 --prefix none --format '{p}')" || return 2
  awk 'NR > 1 {print $1}' <<<"$listing" | sort -u
}

# Packages that turn on a pkcs8 feature that brings pkcs5 (`encryption`,
# `pkcs5`) for package spec `$2` (a pkcs5 version) in host `$1`.
pkcs5_enablers() {
  local listing
  listing="$(tree_or_die -p "$1" --edges normal,features -i "$2" --prefix depth --format '{p}')" || return 2
  awk '
    match($0, /^[0-9]+/) {
      d = substr($0, 1, RLENGTH) + 0; node = substr($0, RLENGTH + 1)
      if (in_feat && d <= feat_depth) in_feat = 0
      if (in_feat && d == feat_depth + 1) { split(node, w, " "); print w[1] }
      if (node ~ /^pkcs8 feature "(encryption|pkcs5)"$/) { in_feat = 1; feat_depth = d }
    }' <<<"$listing" | grep -vx pkcs8 | sort -u || true
}

# Prints why host `$1`'s ciphers do not all come from crypto-glue, or
# nothing when they do. Each resolved version is checked on its own.
glue_exception_violations() {
  local host="$1" crate spec parent enabler
  for spec in $(versions_of "$host" "$GLUE"); do
    for parent in $(parents "$host" "$spec"); do
      grep -Eq "$GLUE_USERS" <<<"$parent" || echo "$spec is used by $parent"
    done
  done
  for crate in $GLUE_CRATES; do
    for spec in $(versions_of "$host" "$crate"); do
      for parent in $(parents "$host" "$spec"); do
        grep -Eq "$GLUE_PARENTS" <<<"$parent" || echo "$spec is reached from $parent"
      done
    done
  done
  for spec in $(versions_of "$host" pkcs5); do
    for enabler in $(pkcs5_enablers "$host" "$spec"); do
      [[ "$enabler" == "$GLUE" ]] || echo "pkcs8 encryption ($spec) is enabled by $enabler"
    done
  done
}

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
    violations="$(glue_exception_violations "$host")"
    if [[ -e "$QUERY_FAILED" ]]; then
      echo "FAIL: $host (default): could not check the passkey cipher exception." >&2
      exit 2
    fi
    if [[ -n "$violations" ]]; then
      echo "FAIL: $host (default) reaches ciphers outside the passkey exception:" >&2
      sed 's/^/  /' <<<"$violations" >&2
      status=1
    else
      ciphers="$(grep '^widevine' <<<"$ciphers" || true)"
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
