#!/bin/bash -eu
# Builds every cargo-fuzz target and packages its checked-in seed corpus.
# Invoked by ClusterFuzzLite inside the image from .clusterfuzzlite/Dockerfile;
# $SRC, $OUT and the sanitizer flags come from the base image.

cd "$SRC/bookclerk/fuzz"
cargo fuzz build -O

release_dir="target/x86_64-unknown-linux-gnu/release"
for target in $(cargo fuzz list); do
  cp "$release_dir/$target" "$OUT/"
  if [ -d "corpus/$target" ]; then
    (cd "corpus/$target" && zip -q -r "$OUT/${target}_seed_corpus.zip" .)
  fi
done
