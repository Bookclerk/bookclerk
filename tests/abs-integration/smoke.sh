#!/usr/bin/env bash
# Smoke-test Audiobookshelf + libationd sidecar.
# Expects compose stack running and ABS reachable at ABS_URL (default localhost:13378).
set -euo pipefail

ABS_URL="${ABS_URL:-http://127.0.0.1:13378}"
LIBATION_URL="${LIBATION_URL:-http://127.0.0.1:8787}"
ADMIN_USER="${ABS_ADMIN_USER:-absadmin}"
ADMIN_PASS="${ABS_ADMIN_PASS:-AbsAdminPass1!}"

echo "== waiting for ABS at $ABS_URL"
for i in $(seq 1 60); do
  if curl -fsS "$ABS_URL/healthcheck" >/dev/null 2>&1 || curl -fsS "$ABS_URL/ping" >/dev/null 2>&1; then
    break
  fi
  # ABS may use /status or root before fully up
  if curl -fsS "$ABS_URL/" >/dev/null 2>&1; then
    break
  fi
  sleep 2
  if [[ $i -eq 60 ]]; then
    echo "ABS did not become ready" >&2
    exit 1
  fi
done

echo "== init / login ABS admin"
# First-time init (idempotent if already initialized).
INIT_CODE=$(curl -sS -o /tmp/abs-init.json -w "%{http_code}" -X POST "$ABS_URL/init" \
  -H 'Content-Type: application/json' \
  -d "{\"newRoot\":{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}}" || true)
echo "init http=$INIT_CODE"

LOGIN=$(curl -fsS -X POST "$ABS_URL/login" \
  -H 'Content-Type: application/json' \
  -d "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}")
TOKEN=$(echo "$LOGIN" | python3 -c 'import json,sys; print(json.load(sys.stdin)["user"]["token"])')
echo "got ABS token"

echo "== ensure book library"
LIBS=$(curl -fsS "$ABS_URL/api/libraries" -H "Authorization: Bearer $TOKEN")
LIB_ID=$(echo "$LIBS" | python3 -c '
import json,sys
libs=json.load(sys.stdin).get("libraries") or []
for lib in libs:
    if lib.get("mediaType")=="book" or lib.get("media_type")=="book":
        print(lib["id"]); break
else:
    if libs: print(libs[0]["id"])
')
if [[ -z "$LIB_ID" ]]; then
  CREATE=$(curl -fsS -X POST "$ABS_URL/api/libraries" \
    -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"name":"Audiobooks","folders":[{"fullPath":"/audiobooks"}],"mediaType":"book"}')
  LIB_ID=$(echo "$CREATE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
fi
echo "library_id=$LIB_ID"

echo "== authorize via libation CLI in container"
docker compose -f tests/abs-integration/docker-compose.yml exec -T \
  -e LIBATION_ABS_API_KEY="$TOKEN" \
  -e LIBATION_ABS_LIBRARY_ID="$LIB_ID" \
  libationd libation integrations test

echo "== mint claim ticket"
TICKET_OUT=$(docker compose -f tests/abs-integration/docker-compose.yml exec -T \
  -e LIBATION_ABS_API_KEY="$TOKEN" \
  libationd libation integrations tickets create \
  --provider audiobookshelf --external-user-id ci-user --label ci)
TICKET=$(echo "$TICKET_OUT" | sed -n 's/^ticket=//p' | head -n1)
echo "ticket=${TICKET:0:8}…"

echo "== redeem ticket on portal"
curl -fsS -c /tmp/libation-portal.jar -X POST "$LIBATION_URL/connect/api/redeem" \
  -H 'Content-Type: application/json' \
  -d "{\"ticket\":\"$TICKET\"}" >/tmp/redeem.json
curl -fsS -b /tmp/libation-portal.jar "$LIBATION_URL/connect/api/me" | tee /tmp/me.json
python3 -c 'import json; d=json.load(open("/tmp/me.json")); assert d["external_user_id"]=="ci-user"'

echo "== portal landing"
curl -fsS "$LIBATION_URL/connect" | grep -q 'Libation Connect'
# Trailing-slash is optional; prefer `/connect` (ticket URLs omit the slash).
curl -fsS "$LIBATION_URL/connect/" >/dev/null 2>&1 || true

echo "== credential login path"
curl -fsS -c /tmp/libation-portal2.jar -X POST "$LIBATION_URL/connect/api/login/integration" \
  -H 'Content-Type: application/json' \
  -d "{\"provider\":\"audiobookshelf\",\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}" >/tmp/login.json
curl -fsS -b /tmp/libation-portal2.jar "$LIBATION_URL/connect/api/me" | tee /tmp/me2.json

echo "== trigger integration library scan via CLI"
docker compose -f tests/abs-integration/docker-compose.yml exec -T \
  -e LIBATION_ABS_API_KEY="$TOKEN" \
  -e LIBATION_ABS_LIBRARY_ID="$LIB_ID" \
  libationd libation integrations scan --integration audiobookshelf || true

echo "ABS integration smoke OK"
