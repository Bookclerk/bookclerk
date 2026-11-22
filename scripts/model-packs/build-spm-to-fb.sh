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
# its other sources by tag while CMake configures, which would run a moved
# tag's build files before anything could check them. So this script fetches
# each one itself by the commit pinned below, checks it, and hands CMake the
# checked directories with fetching turned off. Needs git, cmake and a C++20
# compiler.
set -euo pipefail

# The sources upstream's lite/CMakeLists.txt fetches at the pinned
# sentencepiece commit: FetchContent name, repository, and the commit of the
# tag it names.
DEPENDENCIES=(
  "abseil-cpp https://github.com/abseil/abseil-cpp 2065f4ded0558c6f89fee67c8e5228feb4eb960e"         # 20260817.0
  "protobuf https://github.com/protocolbuffers/protobuf 2c74169b34066ceb8ddb6b882fcb3fb32d737a55"   # v36.2
  "googletest https://github.com/google/googletest 063de7e9578f82b369302001269680b4b1553359"        # v1.18.0
  "flatbuffers https://github.com/google/flatbuffers 03fffb25e2d777462b719cb4964249c30b19d58f"      # v25.12.19-2026-02-06-03fffb2
)
flatbuffers_pinned="03fffb25e2d777462b719cb4964249c30b19d58f"
# Declared upstream only behind an option this build turns off, so never
# fetched.
NOT_FETCHED=(google_benchmark)

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
if [[ -z "$sentencepiece_commit" || "$flatbuffers_commit" != "$flatbuffers_pinned" ]]; then
  echo "the pins in scripts/vendor-sentencepiece-lite.sh and this script disagree" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Fetches one commit of a repository into a directory and fails unless the
# checkout is that commit.
fetch_commit() {
  local directory="$1" repository="$2" commit="$3"
  git init -q "$directory"
  git -C "$directory" fetch -q --depth 1 "$repository" "$commit"
  git -C "$directory" checkout -q FETCH_HEAD
  if [[ "$(git -C "$directory" rev-parse HEAD)" != "$commit" ]]; then
    echo "$repository is not at $commit" >&2
    exit 1
  fi
}

fetch_commit "$work/sentencepiece" https://github.com/google/sentencepiece "$sentencepiece_commit"

# Every name upstream declares must be one pinned here, and the other way
# round, so a new dependency cannot be fetched unchecked.
declared="$(sed -n '/FetchContent_Declare(/{n;s/^ *//p}' "$work/sentencepiece/lite/CMakeLists.txt" | sort | tr '\n' ' ')"
pinned="$({ printf '%s\n' "${DEPENDENCIES[@]}" | cut -d' ' -f1; printf '%s\n' "${NOT_FETCHED[@]}"; } | sort | tr '\n' ' ')"
if [[ "$declared" != "$pinned" ]]; then
  echo "upstream fetches [$declared], this script pins [$pinned]" >&2
  exit 1
fi

source_dirs=()
for dependency in "${DEPENDENCIES[@]}"; do
  read -r name repository commit <<<"$dependency"
  fetch_commit "$work/deps/$name" "$repository" "$commit"
  source_dirs+=("-DFETCHCONTENT_SOURCE_DIR_${name^^}=$work/deps/$name")
done

cmake -S "$work/sentencepiece/lite" -B "$work/build" -DCMAKE_BUILD_TYPE=Release \
  -DSPM_ENABLE_BENCHMARK=OFF -DFETCHCONTENT_FULLY_DISCONNECTED=ON "${source_dirs[@]}" >/dev/null

cmake --build "$work/build" --target spm_to_fb \
  --parallel "$(getconf _NPROCESSORS_ONLN)" >/dev/null
cp "$work/build/spm_to_fb" "$out/spm_to_fb"
echo "built $out/spm_to_fb from sentencepiece $sentencepiece_commit"
