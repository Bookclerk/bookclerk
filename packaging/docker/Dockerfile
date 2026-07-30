# Multi-stage Dockerfile for bookclerk CLI + daemon.
#
# Build: docker build -f packaging/docker/Dockerfile -t bookclerkd .
#
# Decrypt (acquire): Adrm aaxc, Widevine DASH/CENC, MP3 encode, metadata
# fix-up, and chapter split are all native Rust — no ffmpeg or aaxclean-cli.
# Widevine L3 CDMs auto-provision via AudibleCdm (Android auth). Optional BYO
# .wvd under /config still works. Spatial/Atmos (L1) is not available.
#
# Typical volumes:
#   /config  → BOOKCLERK_FILES_DIR (config.toml, library.db, Accounts/*.auth + *.wvd)
#   /data    → acquired audiobooks (set output.local.root = "/data/Audiobooks")
# Prefer: BOOKCLERK_AUTH_PASSWORD (env-only). For local-only unprotected tokens:
# auth.allow_plaintext=true / BOOKCLERK_AUTH_ALLOW_PLAINTEXT=1.
#
# The HTTP control plane is unauthenticated. Default listen is loopback inside
# the container. To publish the port, override explicitly, e.g.:
#   -e BOOKCLERK_DAEMON_LISTEN=0.0.0.0:8787 -p 8787:8787

FROM rust:1.85-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p bookclerk-cli -p bookclerkd \
    && strip target/release/bookclerk target/release/bookclerkd

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/bookclerk --create-home --shell /usr/sbin/nologin bookclerk \
    && mkdir -p /data/Audiobooks \
    && chown -R bookclerk:bookclerk /data
COPY --from=builder /src/target/release/bookclerk /usr/local/bin/bookclerk
COPY --from=builder /src/target/release/bookclerkd /usr/local/bin/bookclerkd
COPY config/config.example.toml /etc/bookclerk/config.example.toml
ENV BOOKCLERK_FILES_DIR=/config \
    BOOKCLERK_CONFIG=/config/config.toml \
    BOOKCLERK_DAEMON_LISTEN=127.0.0.1:8787 \
    BOOKCLERK_OUTPUT_LOCAL_ROOT=/data/Audiobooks
VOLUME ["/config", "/data"]
EXPOSE 8787
USER bookclerk
WORKDIR /config
ENTRYPOINT ["/usr/local/bin/bookclerkd"]
