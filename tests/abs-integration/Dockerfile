# Build release binaries for ABS integration CI.
FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN apt-get update && apt-get install -y --no-install-recommends g++ \
    && cargo build --release -p libation-cli -p libationd

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/libation /usr/local/bin/libation
COPY --from=build /src/target/release/libationd /usr/local/bin/libationd
COPY tests/abs-integration/config.toml /etc/libation/config.toml
ENV LIBATION_CONFIG=/etc/libation/config.toml
EXPOSE 8787
CMD ["libationd"]
