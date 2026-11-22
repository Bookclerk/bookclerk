# SentencePiece Lite (vendored)

The tokenizer runtime the model plugin links, through
`crates/bookclerk-sentencepiece`. The files are upstream's, unchanged.

| Part | Upstream | Pinned at | License |
| --- | --- | --- | --- |
| `lite/sentencepiece_lite.{cc,h,fbs}` | <https://github.com/google/sentencepiece> (`lite/`) | commit `b85b750589487f545a752e0dd51df497ade6f4fc` | Apache-2.0 (`lite/LICENSE`) |
| `flatbuffers/include/flatbuffers/*.h` | <https://github.com/google/flatbuffers> | tag `v25.12.19-2026-02-06-03fffb2` (commit `03fffb25e2d777462b719cb4964249c30b19d58f`) | Apache-2.0 (`flatbuffers/LICENSE`) |
| `lite/sentencepiece_lite_generated.h` | written by that tag's `flatc -c --gen-object-api` from `lite/sentencepiece_lite.fbs` | | as the schema |

Only what the runtime compiles is here: the 16 FlatBuffers headers
`sentencepiece_lite.cc` includes, and none of upstream's converter, cache,
tools or tests. The runtime is built without Abseil and without
`utf8_range` (upstream's `SPM_LITE_ENABLE_ABSL=OFF` and
`SPM_LITE_ENABLE_UTF8_RANGE=OFF`), so it needs a C++20 compiler and nothing
else.

## Updating

Never edit these files by hand. To move to newer upstream commits, change
the pins at the top of `scripts/vendor-sentencepiece-lite.sh`, run it, update
the table above, and run `cargo test -p bookclerk-sentencepiece`: its tests
compare token ids with ids full SentencePiece produced.
