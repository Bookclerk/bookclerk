#!/usr/bin/env bash
# Installs the Bookclerk development toolchain on a Debian-family system.
#
# The single list of what a development environment needs beyond the Rust
# and Node.js base images: system packages, Rust components, cargo-nextest at
# the version CI pins, and a virtualenv with the hashed test requirements of
# scripts/tests/. .devcontainer/Dockerfile runs it at image build time, so
# anything added here lands in the Dev Container; add new tools here rather
# than in the Dockerfile.
#
# Meant for containers and disposable machines (the Dev Container image, a
# cloud development session), not a personal workstation: it installs as root
# into root's Rust toolchain. It deletes nothing; image builds clean their own
# caches afterwards.
#
# Usage (as root, from anywhere):
#   scripts/dev-env/provision.sh [--repo <checkout>]
#
# --repo defaults to the checkout this script lives in; the Dockerfile copies
# the script, scripts/tests/requirements.txt and .tool-versions into the same
# layout.
#
# Environment:
#   BOOKCLERK_DEV_VENV       virtualenv for the script tests (default
#                            /opt/bookclerk-dev/venv). Put its bin/ on PATH to
#                            use it as the default python3.
#   BOOKCLERK_DEV_CA_BUNDLE  optional complete PEM trust bundle (system roots
#                            plus a TLS-intercepting proxy's CA) for the
#                            rustup, cargo and pip downloads; it replaces
#                            their default trust store.
#
# Idempotent: re-running skips what is already installed at the right
# version. Never touches anything under the checkout.
set -euo pipefail

# System packages, one per line with the reason. Rust and Node.js come from
# the base images (or, outside a container, rustup and the Node.js image).
APT_PACKAGES=(
  build-essential
  ca-certificates
  capnproto      # compiles schema/plugin.capnp (bookclerk-plugin-abi build script)
  clang
  cmake
  curl
  git
  jq
  openssh-client # SSH commit signing through the forwarded agent
  pkg-config
  libssl-dev     # openssl-sys
  libdbus-1-dev  # Linux tray (ksni / zbus)
  xdg-utils      # tray "Open Bookclerk" uses xdg-open
  python3
  python3-venv   # virtualenv for scripts/tests (BOOKCLERK_DEV_VENV)
  iproute2       # debugging listen addresses from inside the container
  procps
)

log() { printf 'provision: %s\n' "$*"; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "${script_dir}/../.." && pwd)"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      if [[ -z "${2:-}" ]]; then
        printf 'usage: %s [--repo <checkout>]\n' "$0" >&2
        exit 2
      fi
      repo="$(cd "$2" && pwd)"
      shift 2
      ;;
    *)
      printf 'usage: %s [--repo <checkout>]\n' "$0" >&2
      exit 2
      ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  log "must run as root (installs system packages)"
  exit 1
fi
if ! command -v apt-get >/dev/null 2>&1; then
  log "needs a Debian-family system with apt-get"
  exit 1
fi

if ! command -v rustup >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1; then
  log "rustup and cargo not found on PATH; install Rust first (https://rustup.rs)"
  exit 1
fi

requirements="${repo}/scripts/tests/requirements.txt"
if [[ ! -f "${requirements}" ]]; then
  log "missing ${requirements}; pass --repo <checkout>"
  exit 1
fi

# cargo-nextest version: the pin in the repository's .tool-versions, which CI
# installs too (docs/contributing/ci.md#tool-versions).
NEXTEST_VERSION="$(awk '$1 == "cargo-nextest" { print $2 }' "${repo}/.tool-versions" 2>/dev/null || true)"
if [[ ! "${NEXTEST_VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  log "no cargo-nextest pin in ${repo}/.tool-versions; pass --repo <checkout>"
  exit 1
fi

ca="${BOOKCLERK_DEV_CA_BUNDLE:-}"
if [[ -n "${ca}" ]]; then
  if [[ ! -f "${ca}" ]]; then
    log "BOOKCLERK_DEV_CA_BUNDLE ${ca} is not a file"
    exit 1
  fi
  export SSL_CERT_FILE="${ca}" CARGO_HTTP_CAINFO="${ca}" PIP_CERT="${ca}"
fi

log "system packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends "${APT_PACKAGES[@]}"

# rust-toolchain.toml asks for the `stable` channel by name. Base images
# install a numbered toolchain (1.x.y), which rustup does not treat as
# `stable`, so without this the first cargo command in the checkout would
# download stable at run time. Installing it here, with the components the
# toolchain file lists, keeps that out of every new container.
log "rust stable toolchain"
rustup toolchain install stable --profile minimal --component rustfmt,clippy --no-self-update
rustup default stable

if [[ "$(cargo nextest --version 2>/dev/null | head -n1)" == "cargo-nextest ${NEXTEST_VERSION} "* ]]; then
  log "cargo-nextest ${NEXTEST_VERSION} already installed"
else
  # Built from crates.io with its lockfile: checksums come from the registry
  # index, and no prebuilt binary is downloaded.
  log "cargo-nextest ${NEXTEST_VERSION}"
  cargo install cargo-nextest --locked --version "${NEXTEST_VERSION}"
fi

venv="${BOOKCLERK_DEV_VENV:-/opt/bookclerk-dev/venv}"
log "script-test virtualenv at ${venv}"
if [[ ! -x "${venv}/bin/python" ]]; then
  python3 -m venv "${venv}"
fi
"${venv}/bin/pip" install --quiet --no-cache-dir --require-hashes -r "${requirements}"

log "done"
