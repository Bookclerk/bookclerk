#!/usr/bin/env bash
# Build stays on the host. The seed, daemon, and load run in this container,
# whose cgroup is the 1 GiB / one-core envelope.
#
#   cargo install-platform --release
#   cargo test --release -p bookclerk-library --test envelope_seed --no-run
#   scripts/bench/envelope-container.sh
#
# small-vps.sh then refuses to record numbers unless memory.max and cpu.max
# are that envelope.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FILES="${BOOKCLERK_FILES_DIR:-$ROOT/BookclerkFiles/envelope}"
DAEMON="${BOOKCLERK_DAEMON_BIN:-$ROOT/target/release/bookclerkd}"
CLI="${BOOKCLERK_CLI_BIN:-$ROOT/target/release/bookclerk}"
LABEL="${ENVELOPE_LABEL:-measured}"

[[ -x "${DAEMON}" ]] || {
  echo "envelope: missing ${DAEMON}; cargo install-platform --release on the host" >&2
  exit 1
}
[[ -x "${CLI}" ]] || {
  echo "envelope: missing ${CLI}; cargo install-platform --release on the host" >&2
  exit 1
}

seed="$(
  find "${ROOT}/target/release/deps" -maxdepth 1 -type f -name 'envelope_seed-*' ! -name '*.*' -printf '%T@\t%p\n' 2>/dev/null \
    | sort -n | tail -1 | cut -f2- || true
)"
[[ -n "${seed}" && -x "${seed}" ]] || {
  echo "envelope: missing release envelope_seed binary; cargo test --release -p bookclerk-library --test envelope_seed --no-run" >&2
  exit 1
}

# The hosts are built on this machine and link its glibc and libssl, so the
# container runs the same Ubuntu release: an older one cannot load them
# (ubuntu-latest moving to a newer release made bookclerkd exit at once).
# Hosts that are not Ubuntu fall back to the LTS the envelope was sized on.
image="ubuntu:24.04"
if [[ -r /etc/os-release ]]; then
  host_id="$(. /etc/os-release && printf '%s' "${ID:-}")"
  host_version="$(. /etc/os-release && printf '%s' "${VERSION_ID:-}")"
  if [[ "${host_id}" == "ubuntu" && "${host_version}" =~ ^[0-9]{2}\.[0-9]{2}$ ]]; then
    image="ubuntu:${host_version}"
  fi
fi
echo "envelope: container image ${image}"

parent="$(dirname "${FILES}")"
mkdir -p "${FILES}"
mounts=(-v "${ROOT}:${ROOT}")
case "${parent}" in
  "${ROOT}"|"${ROOT}"/*) ;;
  *) mounts+=(-v "${parent}:${parent}") ;;
esac

exec docker run --rm \
  --memory=1g \
  --memory-swap=1g \
  --cpus=1 \
  "${mounts[@]}" \
  -w "${ROOT}" \
  -e BOOKCLERK_FILES_DIR="${FILES}" \
  -e BOOKCLERK_DAEMON_BIN="${DAEMON}" \
  -e BOOKCLERK_CLI_BIN="${CLI}" \
  -e ENVELOPE_LABEL="${LABEL}" \
  -e ENVELOPE_SEED_BIN="${seed}" \
  "${image}" \
  bash "${ROOT}/scripts/bench/envelope-inside.sh"
