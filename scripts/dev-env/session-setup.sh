#!/usr/bin/env bash
# Gives a disposable Debian or Ubuntu development session (a hosted cloud
# workspace, a CI-like VM) the Dev Container's toolchain natively, so commands
# run in the session's own shell with the versions the Dev Container and CI
# use.
#
# - Node.js and uv are copied out of the digest-pinned images named in
#   .devcontainer/Dockerfile (needs Docker; see docker-up.sh).
# - Everything else, including the current stable Rust that
#   rust-toolchain.toml names, comes from provision.sh, the Dev Container's
#   own list. It needs rustup already installed (https://rustup.rs), as hosted
#   workspaces and the Dev Container base image have.
# - /etc/profile.d puts the script-test virtualenv and cargo first on PATH.
#
# For the exact Dev Container image instead, run a command through
# scripts/dev-env/in-devcontainer.sh.
#
# Usage (as root, from the checkout):
#   scripts/dev-env/session-setup.sh
#
# Environment (both are also written to the profile, so later shells and
# in-devcontainer.sh see them):
#   BOOKCLERK_DEV_REGISTRY_MIRROR  see docker-up.sh.
#   BOOKCLERK_DEV_CA_BUNDLE        complete PEM trust bundle (system roots plus
#                                  a TLS-intercepting proxy's CA); it replaces
#                                  the default trust store for cargo, rustup
#                                  and pip. See provision.sh and
#                                  in-devcontainer.sh.
#
# Each step reports its own failure and the rest still run, so a session
# starts with whatever succeeded; the summary at the end names what failed.
set -uo pipefail

log() { printf 'session-setup: %s\n' "$*"; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "${script_dir}/../.." && pwd)"
dockerfile="${repo}/.devcontainer/Dockerfile"
failed=()

if [[ "$(id -u)" -ne 0 ]]; then
  log "must run as root (installs system packages)"
  exit 1
fi

# A setup hook may run with a minimal PATH; find rustup where it is usually
# installed before provision.sh looks for it.
export PATH="/usr/local/cargo/bin:${HOME}/.cargo/bin:${PATH}"
if [[ -n "${BOOKCLERK_DEV_CA_BUNDLE:-}" && -f "${BOOKCLERK_DEV_CA_BUNDLE}" ]]; then
  BOOKCLERK_DEV_CA_BUNDLE="$(realpath "${BOOKCLERK_DEV_CA_BUNDLE}")"
  export BOOKCLERK_DEV_CA_BUNDLE
fi

# Prints the `name:tag@digest` the Dev Container Dockerfile pulls for an image.
# Flags such as `--platform=...` after FROM are skipped.
#
# Arguments: $1 image name as written after FROM (for example `node`).
image_ref() {
  awk -v name="$1" '
    $1 == "FROM" {
      for (i = 2; i <= NF; i++) {
        if ($i ~ /^--/) continue
        if (index($i, name ":") == 1) { print $i; exit }
        break
      }
    }' "${dockerfile}"
}

# Copies paths out of a pinned image, without running it, into a staging
# directory. Nothing on the host is replaced until every copy has succeeded.
#
# Arguments: $1 image reference, $2 staging directory, then the paths in the
# image; each lands at <staging>/<basename>.
copy_from_image() {
  local ref="$1" stage="$2" cid
  shift 2
  docker pull --quiet "${ref}" >/dev/null || return 1
  cid="$(docker create "${ref}" /bin/true)" || return 1
  while [[ $# -gt 0 ]]; do
    if ! docker cp --quiet "${cid}:$1" "${stage}/$(basename "$1")"; then
      docker rm "${cid}" >/dev/null
      return 1
    fi
    shift
  done
  docker rm "${cid}" >/dev/null
}

# Moves a staged file or directory over its destination. The stage is on
# the destination's filesystem, so the move is a rename; the old copy is
# kept aside until it succeeds and put back if it fails.
#
# Arguments: $1 staged path, $2 destination.
# Returns: non-zero when the destination could not be replaced.
replace_with() {
  local old="$2.bookclerk-dev-old"
  rm -rf "${old}"
  if [[ -e "$2" || -L "$2" ]]; then
    mv "$2" "${old}" || return 1
  fi
  if ! mv "$1" "$2"; then
    [[ -e "${old}" || -L "${old}" ]] && mv "${old}" "$2"
    return 1
  fi
  rm -rf "${old}"
}

step_node_uv() {
  local node_ref uv_ref
  node_ref="$(image_ref node)"
  uv_ref="$(image_ref ghcr.io/astral-sh/uv)"
  if [[ -z "${node_ref}" || -z "${uv_ref}" ]]; then
    log "node or uv image not found in ${dockerfile}"
    return 1
  fi
  "${script_dir}/docker-up.sh" || return 1
  local stage entry
  # Under /usr/local, the destinations' filesystem, so replace_with renames.
  stage="$(mktemp -d /usr/local/.bookclerk-dev-stage.XXXXXX)" || return 1
  log "Node.js from ${node_ref%%@*}, uv from ${uv_ref%%@*}"
  if ! copy_from_image "${node_ref}" "${stage}" /usr/local/bin/node /usr/local/lib/node_modules \
    || ! copy_from_image "${uv_ref}" "${stage}" /uv /uvx; then
    rm -rf "${stage}"
    return 1
  fi
  mkdir -p /usr/local/bin /usr/local/lib/node_modules
  if ! replace_with "${stage}/node" /usr/local/bin/node \
    || ! replace_with "${stage}/uv" /usr/local/bin/uv \
    || ! replace_with "${stage}/uvx" /usr/local/bin/uvx; then
    rm -rf "${stage}"
    return 1
  fi
  # Only the packages the image bundles (npm, corepack) are replaced; other
  # globally installed packages on the host stay.
  for entry in "${stage}/node_modules"/*; do
    if ! replace_with "${entry}" "/usr/local/lib/node_modules/$(basename "${entry}")"; then
      rm -rf "${stage}"
      return 1
    fi
  done
  rm -rf "${stage}"
  ln -sf ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm
  ln -sf ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
}

step_provision() {
  "${script_dir}/provision.sh" --repo "${repo}"
}

step_path() {
  # Same PATH order as the Dev Container image. /usr/local/bin comes before
  # any other Node.js the host image ships, so the copied one wins; the zz-
  # prefix makes login shells source it after the host's own profile.d
  # entries, and ~/.bashrc covers interactive non-login shells.
  printf '%s\n' \
    '# Written by scripts/dev-env/session-setup.sh' \
    'export PATH="/opt/bookclerk-dev/venv/bin:/usr/local/cargo/bin:${HOME}/.cargo/bin:/usr/local/bin:$PATH"' \
    >/etc/profile.d/zz-bookclerk-dev.sh
  local var
  for var in BOOKCLERK_DEV_REGISTRY_MIRROR BOOKCLERK_DEV_CA_BUNDLE; do
    if [[ -n "${!var:-}" ]]; then
      printf 'export %s=%q\n' "${var}" "${!var}" >>/etc/profile.d/zz-bookclerk-dev.sh
    fi
  done
  # The same trust the provisioning used, for later cargo, rustup, pip and
  # Node.js downloads (as in-devcontainer.sh sets inside the container).
  if [[ -n "${BOOKCLERK_DEV_CA_BUNDLE:-}" ]]; then
    for var in SSL_CERT_FILE CARGO_HTTP_CAINFO PIP_CERT NODE_EXTRA_CA_CERTS; do
      printf 'export %s=%q\n' "${var}" "${BOOKCLERK_DEV_CA_BUNDLE}" >>/etc/profile.d/zz-bookclerk-dev.sh
    done
  fi
  local rc="${HOME}/.bashrc" line='[ -f /etc/profile.d/zz-bookclerk-dev.sh ] && . /etc/profile.d/zz-bookclerk-dev.sh'
  touch "${rc}"
  grep -qxF "${line}" "${rc}" || printf '%s\n' "${line}" >>"${rc}"
}

for step in step_node_uv step_provision step_path; do
  if ! "${step}"; then
    log "FAILED: ${step#step_}"
    failed+=("${step#step_}")
  fi
done

# shellcheck source=/dev/null
. /etc/profile.d/zz-bookclerk-dev.sh 2>/dev/null || true
for tool in "cargo --version" "cargo nextest --version" "capnp --version" "node --version" "uv --version"; do
  version="$(${tool} 2>/dev/null | head -n1)"
  printf '  %-14s %s\n' "${tool% --version}" "${version:-missing}"
done

if [[ ${#failed[@]} -gt 0 ]]; then
  log "finished with failures: ${failed[*]} (the session still starts)"
else
  log "done"
fi
exit 0
