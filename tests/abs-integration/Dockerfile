# Build release binaries for ABS integration CI.
FROM rust:1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc AS build
WORKDIR /src
COPY . .
RUN apt-get update && apt-get install -y --no-install-recommends g++ \
    && cargo build --release -p bookclerk-cli -p bookclerkd -p bookclerk-media-worker

FROM debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/bookclerk /usr/local/bin/bookclerk
COPY --from=build /src/target/release/bookclerkd /usr/local/bin/bookclerkd
COPY --from=build /src/target/release/bookclerk-media-worker /usr/local/bin/bookclerk-media-worker
COPY tests/abs-integration/config.toml /etc/bookclerk/config.toml
ENV BOOKCLERK_CONFIG=/etc/bookclerk/config.toml
EXPOSE 8787
CMD ["bookclerkd"]
