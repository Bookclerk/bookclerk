#!/usr/bin/env bash
# Audiobookshelf integration test: a real Audiobookshelf server and the
# production bookclerkd image with the Audiobookshelf plugin, wired together
# the way an operator would and exercised end to end.
#
#   tests/abs-integration/smoke.sh
#
# Expects the images built (see Dockerfile; the ABS Integration workflow
# builds them) and Docker Compose. It:
#   1. starts Audiobookshelf, creates its root user and a book library;
#   2. starts bookclerkd on a fresh volume (it installs its platform plugins);
#   3. approves the audiobookshelf plugin with a TCP grant to Audiobookshelf
#      (the consent API), configures it (the API key through stdin, sealed)
#      and deploys it with the CLI, then restarts bookclerkd;
#   4. checks the plugin reaches Audiobookshelf, mints a claim ticket and
#      redeems it on the portal, signs in to the portal with Audiobookshelf
#      credentials, and asks Audiobookshelf to scan its library.
# The stack and its volumes are removed on exit unless KEEP_STACK=1.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
compose=(docker compose -f "$root/tests/abs-integration/docker-compose.yml")
abs_url=http://127.0.0.1:13378
bookclerk_url=http://127.0.0.1:8787
admin_user=absadmin
# Throwaway credentials for this run only; they reach curl through stdin and
# the containers by name, never on argv.
admin_pass="abs-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
export BOOKCLERK_AUTH_PASSWORD="smoke-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
work=$(mktemp -d)

cleanup() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    "${compose[@]}" logs --no-color 2>&1 | tail -n 300 || true
  fi
  if [ "${KEEP_STACK:-0}" != 1 ]; then
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

# wait_for <label> <url>: poll until the URL answers 2xx (90 tries, 5 s each
# at most, 2 s apart: about 3 to 10 minutes).
wait_for() {
  for _ in $(seq 1 90); do
    if curl -fs --max-time 5 -o /dev/null "$2"; then
      return 0
    fi
    sleep 2
  done
  echo "::error::$1 did not answer $2" >&2
  return 1
}

# cli <args…>: the bookclerk CLI inside the bookclerkd container.
cli() {
  "${compose[@]}" exec -T bookclerkd bookclerk "$@"
}

# json <python expression over d>: evaluate against JSON on stdin.
json() {
  python3 -c "import json, sys; d = json.load(sys.stdin); print($1)"
}

echo "== Audiobookshelf"
"${compose[@]}" up -d audiobookshelf
wait_for Audiobookshelf "$abs_url/healthcheck"
printf '{"newRoot":{"username":"%s","password":"%s"}}' "$admin_user" "$admin_pass" \
  | curl -fsS -o /dev/null -X POST "$abs_url/init" -H 'Content-Type: application/json' --data-binary @-
printf '{"username":"%s","password":"%s"}' "$admin_user" "$admin_pass" \
  | curl -fsS -X POST "$abs_url/login" -H 'Content-Type: application/json' --data-binary @- >"$work/login.json"
token=$(json 'd["user"]["token"]' <"$work/login.json")
abs_post() {
  curl -fsS -X POST -H @- -H 'Content-Type: application/json' -d "$2" "$abs_url$1" \
    <<<"Authorization: Bearer $token"
}
library_id=$(abs_post /api/libraries \
  '{"name":"Audiobooks","folders":[{"fullPath":"/audiobooks"}],"mediaType":"book"}' \
  | json 'd["id"]')
echo "library $library_id"

echo "== bookclerkd, first boot"
"${compose[@]}" up -d bookclerkd
wait_for bookclerkd "$bookclerk_url/health"

echo "== approve, configure and deploy the audiobookshelf plugin"
# A plugin built from this checkout is local-development provenance, so it
# does not get the host-implied grant to its base_url that the first-party
# package from crates.io gets. The operator adds the destination instead,
# through the same consent API the Settings page uses.
operator_token=$(cli daemon token)
printf 'Authorization: Bearer %s\n' "$operator_token" \
  | curl -fsS -o /dev/null -H @- -H 'Content-Type: application/json' \
    -X POST "$bookclerk_url/api/plugins/audiobookshelf/consent" \
    -d '{"approve":true,"grant":{"tcp":[{"host":"127.0.0.1","ports":[80]}],"addressCidrs":["127.0.0.1/32"]}}'
cli plugins config set audiobookshelf base_url http://127.0.0.1:80
cli plugins config set audiobookshelf library_id "$library_id"
printf '%s' "$token" | cli plugins config set audiobookshelf api_key --secret
cli plugins deploy audiobookshelf
"${compose[@]}" restart bookclerkd
wait_for bookclerkd "$bookclerk_url/health"

echo "== the plugin reaches Audiobookshelf"
cli integrations test --integration audiobookshelf | tee "$work/test.txt"
cli integrations status | tee "$work/status.txt"
grep -Eq '^audiobookshelf enabled=true ok=true' "$work/status.txt"

echo "== claim ticket, redeemed on the portal"
cli integrations tickets create --provider audiobookshelf --external-user-id ci-user --label ci \
  >"$work/ticket.txt"
ticket=$(sed -n 's/^ticket=//p' "$work/ticket.txt" | head -n1)
test -n "$ticket"
# The nonce binds the portal session to this client, as the web UI does.
nonce=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
printf '{"ticket":"%s","nonce":"%s"}' "$ticket" "$nonce" \
  | curl -fsS -c "$work/portal.jar" -o /dev/null -X POST "$bookclerk_url/api/portal/redeem" \
    -H 'Content-Type: application/json' --data-binary @-
curl -fsS -b "$work/portal.jar" "$bookclerk_url/api/portal/me" >"$work/me.json"
test "$(json 'd["external_user_id"]' <"$work/me.json")" = ci-user

echo "== portal sign-in with Audiobookshelf credentials"
printf '{"provider":"audiobookshelf","username":"%s","password":"%s"}' "$admin_user" "$admin_pass" \
  | curl -fsS -c "$work/portal2.jar" -o /dev/null -X POST "$bookclerk_url/api/portal/login/integration" \
    -H 'Content-Type: application/json' --data-binary @-
curl -fsS -b "$work/portal2.jar" "$bookclerk_url/api/portal/me" >"$work/me2.json"
json 'd["external_user_id"] if isinstance(d.get("external_user_id"), str) and d["external_user_id"] else sys.exit("portal sign-in has no external user id")' <"$work/me2.json" >/dev/null

echo "== remote library scan"
cli integrations scan --integration audiobookshelf

echo "Audiobookshelf integration test passed"
