#!/usr/bin/env bash
#
# Generate language API references into docs/api/.
#
# Builds rustdoc (workspace libraries), TypeDoc for @bookclerk/plugin-sdk and
# the operator UI, and pdoc for the Python guest SDK. Output lands in
# docs/api/{rust,typescript,ui,python}/ for local browsing and publish prep
# (crates.io / docs.rs still build from source rustdoc independently).
#
# Usage:
#   ./scripts/generate-api-docs.sh           # write HTML under docs/api/
#   ./scripts/generate-api-docs.sh --check   # generate into a temp dir; do not
#                                           # leave docs/api/ dirty (CI)
#
# Environment:
#   CARGO_TARGET_DIR - Cargo target directory (default from .cargo/config.toml).
#   BOOKCLERK_DOC_PYTHON - Python interpreter (default: python3).
#   SKIP_RUST / SKIP_TS / SKIP_PYTHON - Set to 1 to skip a language backend.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CHECK=0
if [[ "${1:-}" == "--check" ]]; then
  CHECK=1
elif [[ "${1:-}" != "" ]]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

PYTHON="${BOOKCLERK_DOC_PYTHON:-python3}"
OUT_ROOT="$ROOT/docs/api"
if [[ "$CHECK" -eq 1 ]]; then
  OUT_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/bookclerk-api-docs.XXXXXX")"
  trap 'rm -rf "$OUT_ROOT"' EXIT
fi

RUST_OUT="$OUT_ROOT/rust"
TS_OUT="$OUT_ROOT/typescript"
UI_OUT="$OUT_ROOT/ui"
PY_OUT="$OUT_ROOT/python"

echo "==> API docs output: $OUT_ROOT"

if [[ "${SKIP_RUST:-0}" != "1" ]]; then
  echo "==> rustdoc (workspace libs, no deps)"
  DOC_TARGET="${ROOT}/.tmp/api-docs-cargo-target"
  rm -rf "$DOC_TARGET/doc"
  mkdir -p "$DOC_TARGET"
  # missing_docs is enforced at compile time via workspace lints.
  # Workspace-wide broken intra-doc links warn (many cross-crate refs are
  # intentional prose); publish crates below must document cleanly.
  RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -W rustdoc::broken_intra_doc_links" \
    cargo doc --workspace --no-deps --all-features --target-dir "$DOC_TARGET"
  echo "==> rustdoc (publish crates: deny broken links)"
  RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D rustdoc::broken_intra_doc_links" \
    cargo doc -p bookclerk-plugin-abi -p bookclerk-plugin-manifest \
      -p bookclerk-plugin-sdk --no-deps --all-features --target-dir "$DOC_TARGET"
  rm -rf "$RUST_OUT"
  mkdir -p "$RUST_OUT"
  cp -a "$DOC_TARGET/doc/." "$RUST_OUT/"
fi

if [[ "${SKIP_TS:-0}" != "1" ]]; then
  echo "==> TypeDoc (@bookclerk/plugin-sdk)"
  if [[ ! -d packages/plugin-sdk/node_modules/typedoc ]]; then
    (cd packages/plugin-sdk && npm ci)
  fi
  rm -rf "$TS_OUT"
  (cd packages/plugin-sdk && npx typedoc \
    --options typedoc.json \
    --out "$TS_OUT")

  echo "==> TypeDoc (ui)"
  if [[ ! -d ui/node_modules ]]; then
    (cd ui && npm ci)
  fi
  # typedoc is a ui devDependency (npm ci above).
  rm -rf "$UI_OUT"
  (cd ui && npx typedoc \
    --options typedoc.json \
    --out "$UI_OUT")
fi

if [[ "${SKIP_PYTHON:-0}" != "1" ]]; then
  echo "==> pdoc (bookclerk-plugin-sdk)"
  VENV="${ROOT}/.tmp/api-docs-venv"
  if [[ ! -x "$VENV/bin/pdoc" ]]; then
    "$PYTHON" -m venv "$VENV"
    "$VENV/bin/pip" -q install -U pip
    # pdoc comes from the package's optional `docs` extra (see pyproject.toml).
    "$VENV/bin/pip" -q install -e "packages/plugin-sdk-python[docs]"
  fi
  rm -rf "$PY_OUT"
  mkdir -p "$PY_OUT"
  "$VENV/bin/pdoc" \
    -o "$PY_OUT" \
    --docformat google \
    bookclerk_plugin_sdk
fi

if [[ "$CHECK" -eq 1 ]]; then
  echo "==> check ok (generated under temp $OUT_ROOT)"
else
  # Ensure the index README remains the entry point beside generated trees.
  if [[ ! -f "$ROOT/docs/api/README.md" ]]; then
    echo "missing docs/api/README.md" >&2
    exit 1
  fi
  echo "==> wrote:"
  echo "    $RUST_OUT"
  echo "    $TS_OUT"
  echo "    $UI_OUT"
  echo "    $PY_OUT"
fi
