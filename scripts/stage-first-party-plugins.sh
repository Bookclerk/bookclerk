#!/usr/bin/env bash
# Stage first-party plugin binaries + plugin.toml for local/CI integration tests.
# Does NOT publish artifacts — only copies build outputs into a plugins tree.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${1:-debug}"
DEST="${2:-${BOOKCLERK_PLUGIN_ARTIFACTS:-$ROOT/target/plugin-artifacts}}"
BIN_DIR="$ROOT/target/$PROFILE"

PLUGINS=(
  "bookclerk-plugin-echo-integration:integration:echo:crates/bookclerk-plugin-examples/echo-integration"
  "bookclerk-plugin-source-audible:source:audible:crates/bookclerk-plugins/source-audible"
  "bookclerk-plugin-source-libro:source:libro:crates/bookclerk-plugins/source-libro"
  "bookclerk-plugin-source-chirp:source:chirp:crates/bookclerk-plugins/source-chirp"
  "bookclerk-plugin-source-graphicaudio:source:graphicaudio:crates/bookclerk-plugins/source-graphicaudio"
  "bookclerk-plugin-integration-audiobookshelf:integration:audiobookshelf:crates/bookclerk-plugins/integration-audiobookshelf"
)

mkdir -p "$DEST"

for entry in "${PLUGINS[@]}"; do
  IFS=':' read -r bin _kind id srcdir <<<"$entry"
  src_bin="$BIN_DIR/$bin"
  if [[ ! -x "$src_bin" && ! -f "$src_bin" ]]; then
    # Windows MSVC
    if [[ -f "${src_bin}.exe" ]]; then
      src_bin="${src_bin}.exe"
    else
      echo "missing binary: $BIN_DIR/$bin (build with cargo build -p … --$PROFILE)" >&2
      exit 1
    fi
  fi
  out="$DEST/$id"
  mkdir -p "$out"
  cp -f "$src_bin" "$out/"
  chmod +x "$out/$(basename "$src_bin")" 2>/dev/null || true
  if [[ -f "$ROOT/$srcdir/plugin.toml" ]]; then
    cp -f "$ROOT/$srcdir/plugin.toml" "$out/plugin.toml"
  else
    echo "missing plugin.toml for $id" >&2
    exit 1
  fi
  # Normalize command to the staged binary name.
  bin_name="$(basename "$src_bin")"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$out/plugin.toml" "$bin_name" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
bin_name = sys.argv[2]
text = path.read_text()
lines = []
for line in text.splitlines():
    if line.strip().startswith("command"):
        lines.append(f'command = "./{bin_name}"')
    else:
        lines.append(line)
path.write_text("\n".join(lines) + "\n")
PY
  fi
  echo "staged $id -> $out"
done

echo "BOOKCLERK_PLUGIN_ARTIFACTS=$DEST"
