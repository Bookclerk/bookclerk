# ClusterFuzzLite build image for the cargo-fuzz targets under fuzz/.
# base-builder-rust provides nightly Rust, cargo-fuzz and the sanitizers.
FROM gcr.io/oss-fuzz-base/base-builder-rust:v1@sha256:d345218878c78feed410f0ad57268617a368912b224f844bbdb71c54c0b9955a
# bookclerk-plugin-abi compiles plugin.capnp in its build script;
# bookclerk-libav's build script downloads (curl) and unpacks (xz) the pinned
# FFmpeg release and builds it with make.
RUN apt-get update && apt-get install -y --no-install-recommends capnproto zip curl xz-utils make
# The image's pinned nightly predates the workspace's rust-version (1.94).
# Bump this date together with that MSRV.
ENV RUSTUP_TOOLCHAIN=nightly-2026-10-17
RUN rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal --component rust-src
COPY . $SRC/bookclerk
WORKDIR $SRC/bookclerk
COPY .clusterfuzzlite/build.sh $SRC/
