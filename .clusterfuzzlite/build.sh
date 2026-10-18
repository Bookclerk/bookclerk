#!/bin/bash -eu
# Builds every cargo-fuzz target and packages its checked-in seed corpus.
# Invoked by ClusterFuzzLite inside the image from .clusterfuzzlite/Dockerfile;
# $SRC, $OUT and the sanitizer flags come from the base image.

cd "$SRC/bookclerk/fuzz"
# The repo's .cargo/config.toml points target-dir at the workspace root, so pin
# it here to know where the binaries land.
export CARGO_TARGET_DIR="$PWD/target"
# Keep overflow checks and debug assertions on: -O alone turns them off, and
# integer overflow in the parsers is a bug class the targets must report.
cargo fuzz build -O --debug-assertions

release_dir="$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release"
for target in $(cargo fuzz list); do
  cp "$release_dir/$target" "$OUT/"
  if [ -d "corpus/$target" ]; then
    (cd "corpus/$target" && zip -q -r "$OUT/${target}_seed_corpus.zip" .)
  fi
done
