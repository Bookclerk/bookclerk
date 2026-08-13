#!/usr/bin/env bash
# Handshake + health + cliInvoke against staged Echo workerd guests using a real
# Cloudflare workerd binary (no shim). Used by workerd-pin-bump CI.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PROFILE="${1:-debug}"
BIN_DIR="$ROOT/target/$PROFILE"
WORKERD_BIN="${BOOKCLERK_WORKERD_BIN:-$BIN_DIR/workerd}"
LAUNCHER="$BIN_DIR/bookclerk-workerd"

if [[ ! -x "$LAUNCHER" ]]; then
  echo "missing $LAUNCHER — build bookclerk-workerd first" >&2
  exit 1
fi
if [[ ! -x "$WORKERD_BIN" ]]; then
  echo "missing $WORKERD_BIN — run cargo ensure-workerd first" >&2
  exit 1
fi

stage_echo() {
  local id="$1"
  local src="$ROOT/examples/$id"
  local dest="$ROOT/target/plugin-artifacts/$id"
  rm -rf "$dest"
  mkdir -p "$dest"
  cp "$src/plugin.toml" "$dest/"
  # Copy full modules tree (.js / .py / .wasm / pkg/).
  if [[ -d "$src/modules" ]]; then
    cp -a "$src/modules" "$dest/"
  fi
  # Embedded logo (and any other assets/) for guests that ship one.
  if [[ -d "$src/assets" ]]; then
    cp -a "$src/assets" "$dest/"
  fi
  echo "$dest"
}

run_rpc() {
  local root="$1"
  shift
  BOOKCLERK_PLUGIN_ROOT="$root" BOOKCLERK_WORKERD_BIN="$WORKERD_BIN" \
    "$LAUNCHER" "$@"
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  if [[ "$haystack" != *"$needle"* ]]; then
    echo "expected output to contain: $needle" >&2
    echo "got: $haystack" >&2
    exit 1
  fi
}

for id in plugins-echo-workerd-ts plugins-echo-workerd-python plugins-echo-workerd-rust; do
  dest="$(stage_echo "$id")"
  echo "==> $id"
  out="$(
    printf '%s\n' \
      '{"id":1,"method":"handshake","params":{"apiVersion":1}}' \
      '{"id":2,"method":"health","params":{}}' \
      '{"id":3,"method":"cliInvoke","params":{"command":"ping","args":{"message":"ci"}}}' \
      '{"id":4,"method":"shutdown","params":{}}' \
      | run_rpc "$dest" 2>/dev/null
  )"
  case "$id" in
    plugins-echo-workerd-ts)
      assert_contains "$out" 'echo_workerd_ts'
      assert_contains "$out" 'echo workerd plugin ready'
      ;;
    plugins-echo-workerd-python)
      assert_contains "$out" 'echo_workerd_python'
      assert_contains "$out" 'echo workerd python plugin ready'
      ;;
    plugins-echo-workerd-rust)
      assert_contains "$out" 'echo_workerd_rust'
      assert_contains "$out" 'echo workerd rust wasm plugin ready'
      ;;
  esac
  if [[ "$out" != *'"ok":true'* && "$out" != *'"ok": true'* ]]; then
    echo "expected health ok in: $out" >&2
    exit 1
  fi
  if [[ "$out" != *'"exitCode":0'* && "$out" != *'"exitCode": 0'* ]]; then
    echo "expected exitCode 0 in: $out" >&2
    exit 1
  fi
  assert_contains "$out" 'pong: ci'
  echo "ok $id"
done

# Outbound allowlist guest: probe www.example.com under *.example.com.
# Success = egress returned a Response (HTTP status is best-effort / ignored).
id=plugins-echo-workerd-fetch
dest="$(stage_echo "$id")"
echo "==> $id"
out="$(
  printf '%s\n' \
    '{"id":1,"method":"handshake","params":{"apiVersion":1}}' \
    '{"id":2,"method":"health","params":{}}' \
    '{"id":3,"method":"cliInvoke","params":{"command":"ping","args":{"message":"ci"}}}' \
    '{"id":4,"method":"cliInvoke","params":{"command":"fetch-example","args":{}}}' \
    '{"id":5,"method":"shutdown","params":{}}' \
    | run_rpc "$dest" 2>/dev/null
)"
assert_contains "$out" "echo_workerd_fetch"
assert_contains "$out" 'echo workerd fetch plugin ready'
assert_contains "$out" 'pong: ci'
if [[ "$out" == *'"allowed":true'* || "$out" == *'"allowed": true'* ]]; then
  # Allowed Response must surface exitCode 0; do not require HTTP 2xx.
  if [[ "$out" != *'"exitCode":0'* && "$out" != *'"exitCode": 0'* ]]; then
    echo "expected exitCode 0 when fetch was allowed in: $out" >&2
    exit 1
  fi
  assert_contains "$out" 'example.com fetch allowed (HTTP'
  echo "ok $id (allowlist Response received; HTTP status ignored)"
else
  echo "warn: $id fetch-example best-effort skip (no Response / network unavailable)" >&2
  echo "got: $out" >&2
  echo "ok $id (handshake/health/ping only)"
fi

echo "all workerd echo guests passed"
