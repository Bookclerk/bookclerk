#!/usr/bin/env bash
# Boots a built image twice on one fresh /config volume and checks that the
# bundled platform plugins are installed, trusted and running jailed.
#
#   packaging/docker/smoke.sh bookclerkd:ci
#
# First boot: bookclerkd must install sqlite and local from the image's bundle
# and load the sqlite database guest under the default ("required")
# isolation. Second boot: nothing is reinstalled and the database still loads.
# The image must not override the isolation defaults.
set -euo pipefail

image=${1:?usage: smoke.sh <image>}
port=${SMOKE_PORT:-18787}
name="bookclerk-smoke-$$"
volume="bookclerk-smoke-config-$$"
# A throwaway passphrase for this volume only; passed by name, never on argv.
export BOOKCLERK_AUTH_PASSWORD="smoke-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"

cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm "$volume" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if docker image inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$image" \
  | grep -E '^BOOKCLERK_(PLUGIN|MEDIA)_ISOLATION='; then
  echo "::error::the image overrides the isolation defaults" >&2
  exit 1
fi

# boot <label>: start the daemon, wait for /health, keep its log in $logs.
boot() {
  docker run -d --name "$name" \
    -v "$volume:/config" \
    -e BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787 \
    -e BOOKCLERK_AUTH_PASSWORD \
    -p "127.0.0.1:$port:8787" \
    "$image" >/dev/null
  local healthy=no
  for _ in $(seq 1 90); do
    if curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
      healthy=yes
      break
    fi
    if [ "$(docker inspect -f '{{.State.Running}}' "$name")" != true ]; then
      break
    fi
    sleep 2
  done
  logs=$(docker logs "$name" 2>&1)
  echo "----- $1 boot log -----"
  printf '%s\n' "$logs"
  if [ "$healthy" != yes ]; then
    echo "::error::$1 boot: bookclerkd did not answer /health" >&2
    exit 1
  fi
  docker rm -f "$name" >/dev/null
}

# expect <label> <pattern>: the last boot log contains the pattern.
expect() {
  if ! grep -qE "$2" <<<"$logs"; then
    echo "::error::$1: log has no match for: $2" >&2
    exit 1
  fi
}

# refuse <label> <pattern>: the last boot log does not contain the pattern.
refuse() {
  if grep -qE "$2" <<<"$logs"; then
    echo "::error::$1: log unexpectedly matches: $2" >&2
    exit 1
  fi
}

boot first
expect first 'installed bundled platform plugin.*sqlite|sqlite.*installed bundled platform plugin'
expect first 'installed bundled platform plugin.*local|local.*installed bundled platform plugin'
expect first 'loaded external database plugin'
refuse first 'could not install the (platform plugins bundled|bundled platform plugin)|did not install the bundled platform plugin'

boot second
refuse second 'installed bundled platform plugin|updated platform plugin from the bundled copy'
expect second 'loaded external database plugin'

echo "smoke test passed"
