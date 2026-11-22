#!/usr/bin/env bash
# Refreshes third_party/sentencepiece-lite from the pinned upstream commits.
#
# Clones google/sentencepiece and google/flatbuffers at the pins below,
# builds that FlatBuffers' own `flatc`, regenerates the schema header and
# copies the files the Lite runtime compiles. Needs git, cmake and a C++
# compiler. See third_party/sentencepiece-lite/README.md.
set -euo pipefail

SENTENCEPIECE_COMMIT="b85b750589487f545a752e0dd51df497ade6f4fc"
FLATBUFFERS_TAG="v25.12.19-2026-02-06-03fffb2"
FLATBUFFERS_COMMIT="03fffb25e2d777462b719cb4964249c30b19d58f"

# The FlatBuffers headers sentencepiece_lite.cc includes, directly or not.
FLATBUFFERS_HEADERS=(
  allocator array base buffer buffer_ref default_allocator detached_buffer
  flatbuffer_builder flatbuffers stl_emulation string struct table vector
  vector_downward verifier
)

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/third_party/sentencepiece-lite"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git init -q "$work/sentencepiece"
git -C "$work/sentencepiece" fetch -q --depth 1 \
  https://github.com/google/sentencepiece "$SENTENCEPIECE_COMMIT"
git -C "$work/sentencepiece" checkout -q FETCH_HEAD

git clone -q --depth 1 --branch "$FLATBUFFERS_TAG" \
  https://github.com/google/flatbuffers "$work/flatbuffers"
actual="$(git -C "$work/flatbuffers" rev-parse HEAD)"
if [[ "$actual" != "$FLATBUFFERS_COMMIT" ]]; then
  echo "flatbuffers tag $FLATBUFFERS_TAG is at $actual, expected $FLATBUFFERS_COMMIT" >&2
  exit 1
fi

cmake -S "$work/flatbuffers" -B "$work/flatc" \
  -DFLATBUFFERS_BUILD_TESTS=OFF -DFLATBUFFERS_BUILD_FLATLIB=OFF \
  -DCMAKE_BUILD_TYPE=Release >/dev/null
cmake --build "$work/flatc" --target flatc --parallel >/dev/null

mkdir -p "$dest/lite" "$dest/flatbuffers/include/flatbuffers"
find "$dest/lite" "$dest/flatbuffers" -type f -delete
for file in sentencepiece_lite.cc sentencepiece_lite.h sentencepiece_lite.fbs; do
  cp "$work/sentencepiece/lite/$file" "$dest/lite/"
done
"$work/flatc/flatc" -c --gen-object-api -o "$dest/lite" \
  "$dest/lite/sentencepiece_lite.fbs"
cp "$work/sentencepiece/LICENSE" "$dest/lite/LICENSE"
for header in "${FLATBUFFERS_HEADERS[@]}"; do
  cp "$work/flatbuffers/include/flatbuffers/$header.h" \
    "$dest/flatbuffers/include/flatbuffers/"
done
cp "$work/flatbuffers/LICENSE" "$dest/flatbuffers/LICENSE"

echo "vendored sentencepiece $SENTENCEPIECE_COMMIT and flatbuffers $FLATBUFFERS_TAG into $dest"
