# Build release binaries for ABS integration CI.
FROM rust:1-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS build
WORKDIR /src
COPY . .
RUN apt-get update && apt-get install -y --no-install-recommends g++ \
    && cargo build --release -p bookclerk-cli -p bookclerkd -p bookclerk-media-worker

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/bookclerk /usr/local/bin/bookclerk
COPY --from=build /src/target/release/bookclerkd /usr/local/bin/bookclerkd
COPY --from=build /src/target/release/bookclerk-media-worker /usr/local/bin/bookclerk-media-worker
COPY tests/abs-integration/config.toml /etc/bookclerk/config.toml
ENV BOOKCLERK_CONFIG=/etc/bookclerk/config.toml
EXPOSE 8787
CMD ["bookclerkd"]
