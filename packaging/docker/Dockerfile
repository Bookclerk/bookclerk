# Multi-stage Dockerfile for libation CLI + daemon.
#
# Build: docker build -f packaging/docker/Dockerfile -t libationd .
#
# Decrypt (liberate): Adrm aaxc, Widevine DASH/CENC, MP3 encode, metadata
# fix-up, and chapter split are all native Rust — no ffmpeg or aaxclean-cli.
# Widevine L3 CDMs auto-provision via AudibleCdm (Android auth). Optional BYO
# .wvd under /config still works. Spatial/Atmos (L1) is not available.
#
# Typical volumes:
#   /config  → LIBATION_FILES_DIR (config.toml, library.db, Accounts/*.auth + *.wvd)
#   /data    → liberated audiobooks (set output.local.root = "/data/Audiobooks")
# Prefer: LIBATION_AUTH_PASSWORD, or mount a secrets volume and set
# LIBATION_AUTH_PASSWORD_FILE=/secrets/auth_password (auto-created with a strong
# random secret if missing — do not put this under Accounts/). For local-only
# unprotected tokens: auth.allow_plaintext=true / LIBATION_AUTH_ALLOW_PLAINTEXT=1.
#
# The HTTP control plane is unauthenticated. Default listen is loopback inside
# the container. To publish the port, override explicitly, e.g.:
#   -e LIBATION_DAEMON_LISTEN=0.0.0.0:8787 -p 8787:8787

FROM rust:1.85-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p libation-cli -p libationd \
    && strip target/release/libation target/release/libationd

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/libation --create-home --shell /usr/sbin/nologin libation \
    && mkdir -p /data/Audiobooks \
    && chown -R libation:libation /data
COPY --from=builder /src/target/release/libation /usr/local/bin/libation
COPY --from=builder /src/target/release/libationd /usr/local/bin/libationd
COPY config/config.example.toml /etc/libation/config.example.toml
ENV LIBATION_FILES_DIR=/config \
    LIBATION_CONFIG=/config/config.toml \
    LIBATION_DAEMON_LISTEN=127.0.0.1:8787 \
    LIBATION_OUTPUT_LOCAL_ROOT=/data/Audiobooks
VOLUME ["/config", "/data"]
EXPOSE 8787
USER libation
WORKDIR /config
ENTRYPOINT ["/usr/local/bin/libationd"]
