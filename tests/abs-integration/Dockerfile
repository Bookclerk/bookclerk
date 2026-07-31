# Build release binaries for ABS integration CI.
FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN apt-get update && apt-get install -y --no-install-recommends g++ \
    && cargo build --release -p bookclerk-cli -p bookclerkd -p bookclerk-media-worker

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/bookclerk /usr/local/bin/bookclerk
COPY --from=build /src/target/release/bookclerkd /usr/local/bin/bookclerkd
COPY --from=build /src/target/release/bookclerk-media-worker /usr/local/bin/bookclerk-media-worker
COPY tests/abs-integration/config.toml /etc/bookclerk/config.toml
ENV BOOKCLERK_CONFIG=/etc/bookclerk/config.toml
EXPOSE 8787
CMD ["bookclerkd"]
