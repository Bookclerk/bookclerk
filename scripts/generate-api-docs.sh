#!/usr/bin/env bash
#
# Generate language API references into docs/api/.
#
# Builds rustdoc (workspace libraries or selected packages), TypeDoc for
# @bookclerk/plugin-sdk and the operator UI, and pdoc for the Python guest SDK.
# Output lands in docs/api/{rust,typescript,ui,python}/ for local browsing and
# publish prep (crates.io / docs.rs still build from source rustdoc independently).
#
# Usage:
#   ./scripts/generate-api-docs.sh              # generate everything under docs/api/
#   ./scripts/generate-api-docs.sh --check      # generate into a temp dir (CI)
#   ./scripts/generate-api-docs.sh --all        # explicit full generation
#   ./scripts/generate-api-docs.sh --rust-package bookclerk-config --ui
#
# Selectors (repeatable / combinable; default with no selectors = --all):
#   --rust-package <name>   Include this Cargo package in rustdoc
#   --typescript-sdk        TypeDoc for packages/plugin-sdk
#   --ui                    TypeDoc for ui/
#   --python                pdoc for packages/plugin-sdk-python
#   --all                   All backends (workspace rustdoc)
#   --check                 Write to a temp dir; do not dirty docs/api/
#
# Environment:
#   CARGO_TARGET_DIR - Cargo target directory (default from .cargo/config.toml /
#                      $CARGO_TARGET_DIR). Rustdoc uses ${CARGO_TARGET_DIR:-target}/doc-ci
#                      so artifacts participate in the normal CI cargo cache.
#   BOOKCLERK_DOC_PYTHON - Python interpreter (default: python3).
#   SKIP_RUST / SKIP_TS / SKIP_PYTHON - Set to 1 to skip a language backend.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CHECK=0
DO_ALL=0
DO_TS=0
DO_UI=0
DO_PYTHON=0
RUST_PACKAGES=()
HAVE_SELECTOR=0

usage() {
  echo "usage: $0 [--check] [--all] [--rust-package NAME]... [--typescript-sdk] [--ui] [--python]" >&2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check) CHECK=1; shift ;;
    --all) DO_ALL=1; HAVE_SELECTOR=1; shift ;;
    --typescript-sdk) DO_TS=1; HAVE_SELECTOR=1; shift ;;
    --ui) DO_UI=1; HAVE_SELECTOR=1; shift ;;
    --python) DO_PYTHON=1; HAVE_SELECTOR=1; shift ;;
    --rust-package)
      if [[ $# -lt 2 ]]; then
        echo "--rust-package requires a package name" >&2
        exit 2
      fi
      RUST_PACKAGES+=("$2")
      HAVE_SELECTOR=1
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

# No selectors → full generation (local default / CI full suite).
if [[ "$HAVE_SELECTOR" -eq 0 ]]; then
  DO_ALL=1
fi

if [[ "$DO_ALL" -eq 1 ]]; then
  DO_TS=1
  DO_UI=1
  DO_PYTHON=1
  RUST_PACKAGES=()
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

# Prefer the workspace cargo target so Swatinem rust-cache covers rustdoc.
CARGO_TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
DOC_TARGET="${CARGO_TARGET}/doc-ci"
mkdir -p "$DOC_TARGET"

PUBLISH_CRATES=(bookclerk-plugin-abi bookclerk-plugin-manifest bookclerk-plugin-sdk)

contains_publish() {
  local p
  for p in "${RUST_PACKAGES[@]+"${RUST_PACKAGES[@]}"}"; do
    case "$p" in
      bookclerk-plugin-abi|bookclerk-plugin-manifest|bookclerk-plugin-sdk) return 0 ;;
    esac
  done
  return 1
}

echo "==> API docs output: $OUT_ROOT"

RUN_RUST=0
if [[ "${SKIP_RUST:-0}" != "1" ]]; then
  if [[ "$DO_ALL" -eq 1 ]] || [[ ${#RUST_PACKAGES[@]} -gt 0 ]]; then
    RUN_RUST=1
  fi
fi

if [[ "$RUN_RUST" -eq 1 ]]; then
  echo "==> rustdoc"
  rm -rf "$DOC_TARGET/doc"
  # `--no-deps` cannot resolve rustdoc paths into other workspace crates, so
  # those must be prose (backticks), not intra-doc links. Deny broken, private,
  # and redundant links rather than warning.
  WORKSPACE_RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links -D rustdoc::redundant_explicit_links -D rustdoc::invalid_html_tags -D rustdoc::bare_urls -D rustdoc::invalid_codeblock_attributes"
  if [[ "$DO_ALL" -eq 1 ]]; then
    # Exclude the publish trio here — they are documented once below under
    # stricter rustdoc flags (crate-level docs).
    RUSTDOCFLAGS="$WORKSPACE_RUSTDOCFLAGS" \
      cargo doc --workspace --exclude bookclerk-plugin-abi \
        --exclude bookclerk-plugin-manifest --exclude bookclerk-plugin-sdk \
        --no-deps --all-features --target-dir "$DOC_TARGET"
  else
    ARGS=()
    for p in "${RUST_PACKAGES[@]}"; do
      case "$p" in
        bookclerk-plugin-abi|bookclerk-plugin-manifest|bookclerk-plugin-sdk) continue ;;
      esac
      ARGS+=(-p "$p")
    done
    if [[ ${#ARGS[@]} -gt 0 ]]; then
      RUSTDOCFLAGS="$WORKSPACE_RUSTDOCFLAGS" \
        cargo doc "${ARGS[@]}" --no-deps --all-features --target-dir "$DOC_TARGET"
    fi
  fi

  if [[ "$DO_ALL" -eq 1 ]] || contains_publish; then
    echo "==> rustdoc (publish crates: deny broken links + crate docs)"
    RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links -D rustdoc::redundant_explicit_links -D rustdoc::invalid_html_tags -D rustdoc::bare_urls -D rustdoc::invalid_codeblock_attributes -D rustdoc::missing_crate_level_docs" \
      cargo doc -p bookclerk-plugin-abi -p bookclerk-plugin-manifest \
        -p bookclerk-plugin-sdk --no-deps --all-features --target-dir "$DOC_TARGET"
  fi

  rm -rf "$RUST_OUT"
  mkdir -p "$RUST_OUT"
  if [[ -d "$DOC_TARGET/doc" ]]; then
    cp -a "$DOC_TARGET/doc/." "$RUST_OUT/"
  fi
fi

RUN_TS_BLOCK=0
if [[ "${SKIP_TS:-0}" != "1" ]] && { [[ "$DO_TS" -eq 1 ]] || [[ "$DO_UI" -eq 1 ]]; }; then
  RUN_TS_BLOCK=1
fi

if [[ "$RUN_TS_BLOCK" -eq 1 ]]; then
  if [[ "$DO_TS" -eq 1 ]]; then
    echo "==> TypeDoc (@bookclerk/plugin-sdk)"
    if [[ ! -d packages/plugin-sdk/node_modules/typedoc ]]; then
      (cd packages/plugin-sdk && npm ci)
    fi
    rm -rf "$TS_OUT"
    (cd packages/plugin-sdk && npx typedoc \
      --options typedoc.json \
      --out "$TS_OUT")
  fi

  if [[ "$DO_UI" -eq 1 ]]; then
    echo "==> TypeDoc (ui)"
    if [[ ! -d ui/node_modules ]]; then
      (cd ui && npm ci)
    fi
    rm -rf "$UI_OUT"
    (cd ui && npx typedoc \
      --options typedoc.json \
      --out "$UI_OUT")
  fi
fi

if [[ "${SKIP_PYTHON:-0}" != "1" ]] && [[ "$DO_PYTHON" -eq 1 ]]; then
  echo "==> pdoc (bookclerk-plugin-sdk)"
  VENV="${ROOT}/.tmp/api-docs-venv"
  if [[ ! -x "$VENV/bin/pdoc" ]]; then
    "$PYTHON" -m venv "$VENV"
    "$VENV/bin/pip" -q install -U pip
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
  if [[ ! -f "$ROOT/docs/api/README.md" ]]; then
    echo "missing docs/api/README.md" >&2
    exit 1
  fi
  echo "==> wrote selected backends under $OUT_ROOT"
fi
