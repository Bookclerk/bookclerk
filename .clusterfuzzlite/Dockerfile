# ClusterFuzzLite build image for the cargo-fuzz targets under fuzz/.
# base-builder-rust provides nightly Rust, cargo-fuzz and the sanitizers.
FROM gcr.io/oss-fuzz-base/base-builder-rust:v1@sha256:3428d0a90b1e7efbe2ba947bd1d0c9a70594986b4b6553804547f1fb24cfc92f
# bookclerk-plugin-abi compiles plugin.capnp in its build script.
RUN apt-get update && apt-get install -y --no-install-recommends capnproto zip
# The image's pinned nightly predates the workspace's rust-version (1.94).
# Bump this date together with that MSRV.
ENV RUSTUP_TOOLCHAIN=nightly-2026-10-17
RUN rustup toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal --component rust-src
COPY . $SRC/bookclerk
WORKDIR $SRC/bookclerk
COPY .clusterfuzzlite/build.sh $SRC/
