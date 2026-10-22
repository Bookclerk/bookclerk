#!/usr/bin/env bash
# Seal or open the OSV remediation bundle that jobs of one run pass along.
#
# The OSV remediation workflow tests a lockfile bump with the full CI suite
# before anything about it is public. Artifacts of a public repository can be
# downloaded by anyone, so the bump (a git bundle of one commit) and the pull
# request body travel between jobs encrypted with the
# OSV_REMEDIATION_BUNDLE_KEY Actions secret.
#
# Usage:
#   osv-remediation-bundle.sh seal <dir> <file>   tar <dir>, encrypt to <file>
#   osv-remediation-bundle.sh open <file> <dir>   decrypt <file>, untar into <dir>
#
# The passphrase is read from the BUNDLE_KEY environment variable, never from
# argv. Paths go through `cd` and pipes rather than tar arguments, because GNU
# tar on Windows reads `D:\...` as a remote host. Integrity of the code is
# checked by the caller, which compares the unbundled commit with the commit
# id the sealing job reported.
#
# Exit codes: 0 on success, 1 on any error (including a wrong key).

set -euo pipefail

# PBKDF2 rounds for the AES-256-CBC key; both ends must agree.
ITERATIONS=200000

# Prints the path of an OpenSSL 3 binary. macOS ships LibreSSL as `openssl`,
# so Homebrew's OpenSSL 3 is preferred where it exists.
pick_openssl() {
  local candidate
  for candidate in \
    /opt/homebrew/opt/openssl@3/bin/openssl \
    /usr/local/opt/openssl@3/bin/openssl \
    openssl; do
    if command -v "$candidate" >/dev/null 2>&1 &&
      "$candidate" version 2>/dev/null | grep -q '^OpenSSL 3'; then
      echo "$candidate"
      return 0
    fi
  done
  echo "osv-remediation-bundle: no OpenSSL 3 binary found" >&2
  return 1
}

if [[ $# -ne 3 ]] || [[ "$1" != seal && "$1" != open ]]; then
  echo "usage: $0 seal <dir> <file> | open <file> <dir>" >&2
  exit 1
fi
if [[ -z "${BUNDLE_KEY:-}" ]]; then
  echo "osv-remediation-bundle: BUNDLE_KEY is empty (is OSV_REMEDIATION_BUNDLE_KEY set?)" >&2
  exit 1
fi

ossl="$(pick_openssl)"
case "$1" in
  seal)
    dir="$2"
    file="$3"
    (cd "$dir" && tar -cf - .) |
      "$ossl" enc -aes-256-cbc -salt -pbkdf2 -iter "$ITERATIONS" -md sha256 \
        -pass env:BUNDLE_KEY >"$file"
    ;;
  open)
    file="$2"
    dir="$3"
    mkdir -p "$dir"
    "$ossl" enc -d -aes-256-cbc -pbkdf2 -iter "$ITERATIONS" -md sha256 \
      -pass env:BUNDLE_KEY <"$file" |
      (cd "$dir" && tar -xf -)
    ;;
esac
