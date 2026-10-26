# Production image: bookclerkd + CLI with the platform bundle.
#
# Build from the repository root:
#   docker build -f packaging/docker/Dockerfile -t bookclerkd .
#
# The image carries the `cargo package-platform` bundle under /opt/bookclerk:
# the hosts, bookclerk-media-worker, bookclerk-jail, bookclerk-workerd, the
# pinned workerd, and the sqlite and local platform plugins under plugins/.
# On first start (and after an image upgrade) bookclerkd installs those
# plugins into BOOKCLERK_FILES_DIR (/config) and records them in its install
# ledger, so a fresh volume boots with a working database and local output.
#
# Isolation stays at the shipped default ("required" for plugins and media):
# a host that cannot confine a guest refuses to run it. Docker's default
# seccomp profile allows Landlock since Docker 20.10.14; see
# docs/server/operations.md#docker.
#
# Volumes:
#   /config  BOOKCLERK_FILES_DIR (config.toml, library.db, master.key, plugins/)
#   /data    acquired audiobooks: `bookclerk plugins config set local root /data/Audiobooks`
#
# Operator auth is on by default (token sealed in library.db, or
# BOOKCLERK_OPERATOR_TOKEN). Set BOOKCLERK_AUTH_PASSWORD to wrap master.key at
# rest. bookclerkd listens on loopback inside the container; to publish the
# port, override explicitly, e.g.:
#   -e BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787 -p 8787:8787
#
# Base images are pinned by digest; Dependabot (docker ecosystem) bumps them.

FROM node:25-bookworm-slim@sha256:81db02c4b671288a03915da9534dbd54f96d0e7c24d80ccc54f5b36b2e684370 AS ui
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY ui/ ./
RUN npm run build

FROM rust:1-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends capnproto \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# Built in the ui stage, so package-platform finds a current ui/dist and does
# not need Node here.
COPY --from=ui /src/ui/dist ui/dist
# Release hosts + helpers + pinned workerd + sqlite/local, stamped and laid
# out exactly as the release archive. The bundle-local ledger is dropped: the
# host stamps its own ledger in BOOKCLERK_FILES_DIR when it installs the
# plugins.
RUN cargo package-platform --out /dist \
    && bundle=$(find /dist -mindepth 1 -maxdepth 1 -type d -name 'bookclerk-*') \
    && test -n "$bundle" \
    && mv "$bundle" /opt/bookclerk \
    && rm -f /opt/bookclerk/install-ledger.json /opt/bookclerk/.plugin-mutation.lock \
    && test -x /opt/bookclerk/bookclerkd \
    && test -x /opt/bookclerk/bookclerk-jail \
    && test -x /opt/bookclerk/bookclerk-workerd \
    && test -x /opt/bookclerk/bookclerk-media-worker \
    && test -x /opt/bookclerk/workerd \
    && test "$(find /opt/bookclerk/plugins -mindepth 2 -maxdepth 2 -name plugin.toml | wc -l)" -ge 2

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8 AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/bookclerk --create-home --shell /usr/sbin/nologin bookclerk \
    && mkdir -p /config /data/Audiobooks \
    && chown -R bookclerk:bookclerk /config /data
# Root-owned and read-only to the service user. The helpers and plugins/ must
# stay beside the host binaries: that is where the hosts look for them.
COPY --from=builder /opt/bookclerk /opt/bookclerk
COPY --from=ui /src/ui/dist /opt/bookclerk/ui/dist
COPY config/config.example.toml /etc/bookclerk/config.example.toml
RUN ln -s /opt/bookclerk/bookclerk /usr/local/bin/bookclerk \
    && ln -s /opt/bookclerk/bookclerkd /usr/local/bin/bookclerkd
ENV BOOKCLERK_FILES_DIR=/config \
    BOOKCLERK_CONFIG=/config/config.toml \
    BOOKCLERK_DAEMON_LISTEN=127.0.0.1:8787
VOLUME ["/config", "/data"]
EXPOSE 8787
USER bookclerk
WORKDIR /config
ENTRYPOINT ["/opt/bookclerk/bookclerkd"]
