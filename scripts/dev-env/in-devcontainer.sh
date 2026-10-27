#!/usr/bin/env bash
# Runs one command inside the Dev Container image, with this checkout
# bind-mounted, for when exact parity with the Dev Container matters (a
# toolchain-sensitive failure, a check before reporting a bug).
#
# Usage, from anywhere in the checkout:
#   scripts/dev-env/in-devcontainer.sh cargo test -p bookclerk-db-exec
#   scripts/dev-env/in-devcontainer.sh bash        # interactive shell
#
# The image is built from .devcontainer/Dockerfile on first use and tagged
# with a hash of the files it is built from, so it is rebuilt only when one
# of them changes. The host's target/devcontainer/ is mounted over the
# container's target/, so host and container builds of different toolchains
# do not invalidate each other while tools that expect target/ (cargo dev,
# cargo stage-plugins) still find their artifacts there. The command runs
# as the caller's UID/GID with HOME under /tmp.
#
# Environment:
#   BOOKCLERK_DEV_CA_BUNDLE        complete PEM trust bundle (system roots plus
#                                  a TLS-intercepting proxy's CA) for the build
#                                  and the container; it replaces the default
#                                  trust store for cargo, rustup and pip.
#   BOOKCLERK_DEV_REGISTRY_MIRROR  see docker-up.sh.
#   HTTP(S)_PROXY / NO_PROXY       passed through; the container shares the
#                                  host network so a loopback proxy works.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "${script_dir}/../.." && pwd)"

if [[ $# -eq 0 ]]; then
  printf 'usage: %s <command> [args...]\n' "$0" >&2
  exit 2
fi

"${script_dir}/docker-up.sh"

inputs=(
  "${repo}/.devcontainer/Dockerfile"
  "${repo}/scripts/dev-env/provision.sh"
  "${repo}/scripts/tests/requirements.txt"
)
tag="bookclerk-dev:local-$(cat "${inputs[@]}" | sha256sum | cut -c1-12)"

ca="${BOOKCLERK_DEV_CA_BUNDLE:-}"
if [[ -n "${ca}" && ! -f "${ca}" ]]; then
  printf 'in-devcontainer: BOOKCLERK_DEV_CA_BUNDLE %s is not a file\n' "${ca}" >&2
  exit 1
fi
# docker run treats a relative -v source as a volume name.
[[ -n "${ca}" ]] && ca="$(realpath "${ca}")"

if ! docker image inspect "${tag}" >/dev/null 2>&1; then
  printf 'in-devcontainer: building %s\n' "${tag}" >&2
  # Host network and the caller's proxy settings, as for docker run below,
  # so a loopback proxy also works for the build's downloads. Docker keeps
  # these predefined proxy arguments out of the image history.
  build=(docker build --network host -f "${repo}/.devcontainer/Dockerfile" -t "${tag}")
  [[ -n "${ca}" ]] && build+=(--secret "id=extra_ca,src=${ca}")
  for var in HTTP_PROXY HTTPS_PROXY NO_PROXY http_proxy https_proxy no_proxy; do
    [[ -n "${!var:-}" ]] && build+=(--build-arg "${var}")
  done
  "${build[@]}" "${repo}" >&2
fi

name="$(basename "${repo}")"
workspace="/workspaces/${name}"
rel="$(realpath --relative-to="${repo}" "${PWD}" 2>/dev/null || echo .)"
[[ "${rel}" == ..* ]] && rel="."

# Created here so Docker does not create it as root.
mkdir -p "${repo}/target/devcontainer"

run=(docker run --rm -i --network host
  --user "$(id -u):$(id -g)"
  -v "${repo}:${workspace}"
  -v "${repo}/target/devcontainer:${workspace}/target"
  -w "${workspace}/${rel}"
  -e HOME=/tmp/home
  -e "CARGO_HOME=${workspace}/.cargo-home"
  -e "BOOKCLERK_FILES_DIR=${workspace}/BookclerkFiles"
  -e CARGO_TERM_COLOR)
[[ -t 0 && -t 1 ]] && run+=(-t)
for var in HTTP_PROXY HTTPS_PROXY NO_PROXY http_proxy https_proxy no_proxy; do
  [[ -n "${!var:-}" ]] && run+=(-e "${var}")
done
if [[ -n "${ca}" ]]; then
  run+=(-v "${ca}:/run/bookclerk-dev/ca.pem:ro"
    -e SSL_CERT_FILE=/run/bookclerk-dev/ca.pem
    -e CARGO_HTTP_CAINFO=/run/bookclerk-dev/ca.pem
    -e NODE_EXTRA_CA_CERTS=/run/bookclerk-dev/ca.pem
    -e PIP_CERT=/run/bookclerk-dev/ca.pem)
fi

exec "${run[@]}" "${tag}" "$@"
