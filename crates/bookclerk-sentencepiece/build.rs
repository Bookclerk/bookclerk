//! Compiles the SentencePiece Lite runtime vendored under
//! `third_party/sentencepiece-lite` and the C shim in `cpp/capi.cc` into one
//! static library.
//!
//! The runtime is built the way upstream's `lite/CMakeLists.txt` builds it
//! with `SPM_LITE_ENABLE_ABSL=OFF` and `SPM_LITE_ENABLE_UTF8_RANGE=OFF`: no
//! Abseil, no `utf8_range`, only the FlatBuffers headers. Nothing is
//! downloaded and nothing but a C++20 compiler is needed.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| std::ffi::OsString::from(".")),
    );
    let vendored = manifest_dir.join("../../third_party/sentencepiece-lite");
    let lite = vendored.join("lite");
    let shim = manifest_dir.join("cpp/capi.cc");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .include(&lite)
        .include(vendored.join("flatbuffers/include"))
        .file(lite.join("sentencepiece_lite.cc"))
        .file(&shim)
        // The vendored sources are upstream's, unchanged; their warnings are
        // not ours to fix here.
        .warnings(false);
    build.compile("bookclerk_sentencepiece_lite");

    for path in [
        lite.join("sentencepiece_lite.cc"),
        lite.join("sentencepiece_lite.h"),
        lite.join("sentencepiece_lite_generated.h"),
        shim,
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
