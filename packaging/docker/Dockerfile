# Multi-stage Dockerfile for libation CLI + daemon.
#
# Build: docker build -f packaging/docker/Dockerfile -t libationd .
#
# Decrypt (liberate) requires aaxclean-cli on PATH inside the container
# (Adrm + preferred CENC). ffmpeg is also needed for format=mp3 and as a
# CENC decrypt fallback. This image does not bundle either — mount binaries, e.g.:
#   -v /usr/local/bin/aaxclean-cli:/usr/local/bin/aaxclean-cli:ro
#   -v /usr/bin/ffmpeg:/usr/bin/ffmpeg:ro
# Widevine titles need a .wvd CDM under /config (widevine.wvd or download.widevine_cdm).
#
# Typical volumes:
#   /config  → LIBATION_FILES_DIR (config.toml, library.db, auth/)
#   /data    → liberated audiobooks (set storage.local.root = "/data/Audiobooks")
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
    LIBATION_STORAGE_LOCAL_ROOT=/data/Audiobooks
VOLUME ["/config", "/data"]
EXPOSE 8787
USER libation
WORKDIR /config
ENTRYPOINT ["/usr/local/bin/libationd"]
