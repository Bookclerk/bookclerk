#!/usr/bin/env bash
# Makes sure a Docker daemon is reachable, starting one when this is a
# disposable root environment that has dockerd installed but not running.
#
# Used by session-setup.sh and in-devcontainer.sh. On a workstation with
# Docker Desktop or a system service it only checks `docker info`.
#
# Environment:
#   BOOKCLERK_DEV_REGISTRY_MIRROR  optional registry mirror URL (for example
#                                  https://mirror.gcr.io) written to
#                                  /etc/docker/daemon.json before starting a
#                                  daemon, for networks whose shared egress is
#                                  rate-limited by Docker Hub. Images stay
#                                  pinned by digest, so a mirror cannot swap
#                                  their content.
#
# Exit status: 0 when `docker info` succeeds, 1 otherwise.
set -euo pipefail

log() { printf 'docker-up: %s\n' "$*" >&2; }

if ! command -v docker >/dev/null 2>&1; then
  log "docker CLI not installed"
  exit 1
fi
if docker info >/dev/null 2>&1; then
  exit 0
fi
if [[ "$(id -u)" -ne 0 ]] || ! command -v dockerd >/dev/null 2>&1; then
  log "no reachable Docker daemon; start Docker and retry"
  exit 1
fi

mirror="${BOOKCLERK_DEV_REGISTRY_MIRROR:-}"
if [[ -n "${mirror}" && -e /etc/docker/daemon.json ]]; then
  log "/etc/docker/daemon.json exists; not adding BOOKCLERK_DEV_REGISTRY_MIRROR to it"
elif [[ -n "${mirror}" ]]; then
  if [[ ! "${mirror}" =~ ^https://[A-Za-z0-9.-]+(:[0-9]+)?/?$ ]]; then
    log "BOOKCLERK_DEV_REGISTRY_MIRROR must be an https:// origin"
    exit 1
  fi
  mkdir -p /etc/docker
  printf '{"registry-mirrors":["%s"]}\n' "${mirror}" >/etc/docker/daemon.json
fi

log_file="${TMPDIR:-/tmp}/bookclerk-dockerd.log"
log "starting dockerd (log: ${log_file})"
nohup dockerd >"${log_file}" 2>&1 &
for _ in $(seq 1 30); do
  if docker info >/dev/null 2>&1; then
    exit 0
  fi
  sleep 1
done
log "dockerd did not come up; see ${log_file}"
exit 1
