// C ABI over the SentencePiece Lite runtime, for `src/lib.rs`.
//
// Every function is `noexcept` and catches what the runtime throws
// (allocation failure), so no C++ exception crosses into Rust.

#include <cstddef>
#include <cstdint>
#include <new>
#include <string_view>
#include <vector>

#include "sentencepiece_lite.h"

using sentencepiece::lite::SentencePieceLiteProcessor;
using sentencepiece::lite::StatusCode;

extern "C" {

// Opens a model over `data[0..len]`, which the caller keeps alive and
// unchanged until `bcspl_free`. Returns null when the bytes are not a valid
// model.
void* bcspl_new(const uint8_t* data, size_t len) noexcept {
  try {
    auto* processor = new SentencePieceLiteProcessor(
        std::string_view(reinterpret_cast<const char*>(data), len));
    if (processor->status() != StatusCode::kOk) {
      delete processor;
      return nullptr;
    }
    return processor;
  } catch (...) {
    return nullptr;
  }
}

// Frees a model `bcspl_new` returned. Null is ignored.
void bcspl_free(void* handle) noexcept {
  delete static_cast<SentencePieceLiteProcessor*>(handle);
}

// Encodes `text[0..len]` (UTF-8) and writes at most `cap` ids to `out`.
// Returns the number of ids the whole text encodes to, which may exceed
// `cap`, or -1 when encoding failed.
int64_t bcspl_encode(const void* handle, const uint8_t* text, size_t len,
                     int32_t* out, size_t cap) noexcept {
  try {
    const auto* processor =
        static_cast<const SentencePieceLiteProcessor*>(handle);
    std::vector<int> ids;
    if (processor->Encode(
            std::string_view(reinterpret_cast<const char*>(text), len),
            &ids) != StatusCode::kOk) {
      return -1;
    }
    const size_t n = ids.size() < cap ? ids.size() : cap;
    for (size_t i = 0; i < n; ++i) out[i] = static_cast<int32_t>(ids[i]);
    return static_cast<int64_t>(ids.size());
  } catch (...) {
    return -1;
  }
}

// The id of a special piece: 0 unknown, 1 beginning of text, 2 end of text,
// 3 padding. Returns -1 when the model has no such piece or `which` is not
// one of those.
int32_t bcspl_special_id(const void* handle, int32_t which) noexcept {
  const auto* processor =
      static_cast<const SentencePieceLiteProcessor*>(handle);
  switch (which) {
    case 0:
      return processor->unk_id();
    case 1:
      return processor->bos_id();
    case 2:
      return processor->eos_id();
    case 3:
      return processor->pad_id();
    default:
      return -1;
  }
}

// Looks `piece[0..len]` up by its exact text. Returns its id, or -1 when the
// model has no such piece.
int32_t bcspl_piece_to_id(const void* handle, const uint8_t* piece,
                          size_t len) noexcept {
  const auto* processor =
      static_cast<const SentencePieceLiteProcessor*>(handle);
  const std::string_view wanted(reinterpret_cast<const char*>(piece), len);
  const int id = processor->PieceToId(wanted);
  // A miss answers the unknown id, so confirm by reading the piece back.
  if (id < 0 || static_cast<size_t>(id) >= processor->vocab_size()) return -1;
  return processor->IdToPiece(id) == wanted ? static_cast<int32_t>(id) : -1;
}

// Points `*out` at the text of piece `id`, inside the model bytes, and
// returns its length. Returns -1 (and leaves `*out` alone) when `id` is not
// below the vocabulary size.
int64_t bcspl_id_to_piece(const void* handle, int32_t id,
                          const uint8_t** out) noexcept {
  const auto* processor =
      static_cast<const SentencePieceLiteProcessor*>(handle);
  if (id < 0 || static_cast<size_t>(id) >= processor->vocab_size()) return -1;
  const std::string_view piece = processor->IdToPiece(id);
  *out = reinterpret_cast<const uint8_t*>(piece.data());
  return static_cast<int64_t>(piece.size());
}

// Number of pieces in the model.
size_t bcspl_vocab_size(const void* handle) noexcept {
  return static_cast<const SentencePieceLiteProcessor*>(handle)->vocab_size();
}

}  // extern "C"
