#!/usr/bin/env bash
# Builds upstream's `spm_to_fb`, which turns a SentencePiece model into the
# FlatBuffers file the Lite runtime reads. The model pack builder
# (scripts/model_packs.py) and the tokenizer fixtures
# (crates/bookclerk-sentencepiece/tests/fixtures/make_fixture.py) use it.
#
#   scripts/model-packs/build-spm-to-fb.sh <output directory>
#
# The tool lands at <output directory>/spm_to_fb. It is built from the same
# google/sentencepiece commit the vendored runtime comes from (the pins at
# the top of scripts/vendor-sentencepiece-lite.sh). Upstream's build fetches
# its other sources by tag; this script fails unless each tag is at the
# commit pinned below. Needs git, cmake and a C++20 compiler.
set -euo pipefail

# Commits of the tags upstream's lite/CMakeLists.txt fetches at the pinned
# sentencepiece commit, by FetchContent directory name.
declare -A DEPENDENCY_COMMITS=(
  [abseil-cpp-src]="2065f4ded0558c6f89fee67c8e5228feb4eb960e"   # 20260817.0
  [protobuf-src]="2c74169b34066ceb8ddb6b882fcb3fb32d737a55"     # v36.2
  [googletest-src]="063de7e9578f82b369302001269680b4b1553359"   # v1.18.0
  [flatbuffers-src]="03fffb25e2d777462b719cb4964249c30b19d58f"  # v25.12.19-2026-02-06-03fffb2
)

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <output directory>" >&2
  exit 2
fi
mkdir -p "$1"
out="$(cd "$1" && pwd)"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

pin() {
  sed -n "s/^$1=\"\\(.*\\)\"\$/\\1/p" "$root/scripts/vendor-sentencepiece-lite.sh"
}
sentencepiece_commit="$(pin SENTENCEPIECE_COMMIT)"
flatbuffers_commit="$(pin FLATBUFFERS_COMMIT)"
if [[ -z "$sentencepiece_commit" || "$flatbuffers_commit" != "${DEPENDENCY_COMMITS[flatbuffers-src]}" ]]; then
  echo "the pins in scripts/vendor-sentencepiece-lite.sh and this script disagree" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git init -q "$work/sentencepiece"
git -C "$work/sentencepiece" fetch -q --depth 1 \
  https://github.com/google/sentencepiece "$sentencepiece_commit"
git -C "$work/sentencepiece" checkout -q FETCH_HEAD

cmake -S "$work/sentencepiece/lite" -B "$work/build" -DCMAKE_BUILD_TYPE=Release >/dev/null

checked=0
for source in "$work"/build/_deps/*-src "$work"/build/third_party/*-src; do
  [[ -d "$source" ]] || continue
  name="$(basename "$source")"
  expected="${DEPENDENCY_COMMITS[$name]:-}"
  actual="$(git -C "$source" rev-parse HEAD)"
  if [[ -z "$expected" || "$actual" != "$expected" ]]; then
    echo "$name is at $actual, expected ${expected:-no such dependency}" >&2
    exit 1
  fi
  checked=$((checked + 1))
done
if [[ "$checked" -ne "${#DEPENDENCY_COMMITS[@]}" ]]; then
  echo "expected ${#DEPENDENCY_COMMITS[@]} fetched dependencies, found $checked" >&2
  exit 1
fi

cmake --build "$work/build" --target spm_to_fb \
  --parallel "$(getconf _NPROCESSORS_ONLN)" >/dev/null
cp "$work/build/spm_to_fb" "$out/spm_to_fb"
echo "built $out/spm_to_fb from sentencepiece $sentencepiece_commit"
