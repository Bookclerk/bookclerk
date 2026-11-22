//! Safe wrapper over the SentencePiece Lite tokenizer runtime.
//!
//! A tokenizer turns text into the token ids a transformer model was
//! trained on. [SentencePiece Lite](https://github.com/google/sentencepiece/tree/master/lite)
//! is Google's small C++ runtime for SentencePiece models stored as
//! FlatBuffers (`.spm.fb`): it gives the same ids as full SentencePiece and
//! needs neither Protocol Buffers nor Abseil.
//!
//! The C++ sources are vendored, unchanged, under
//! `third_party/sentencepiece-lite` at the commits its `README.md` pins, and
//! are compiled by this crate's build script. They are Apache-2.0; the
//! license texts sit beside them.
//!
//! The runtime parses the model bytes and the text it is given. This crate
//! is meant for the jailed model plugin, which is where that parsing
//! belongs; a host process must not link it (see
//! `docs/adr/ai-models.md`).
//!
//! ```no_run
//! use bookclerk_sentencepiece::Tokenizer;
//!
//! let model = std::fs::read("tokenizer.spm.fb")?;
//! let tokenizer = Tokenizer::from_bytes(model)?;
//! let encoding = tokenizer.encode("A detective in Victorian London", 512)?;
//! assert!(encoding.ids.len() <= 512);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::ffi::c_void;
use std::ptr::NonNull;

/// The C ABI of `cpp/capi.cc`.
// FFI declarations: the one place this crate names foreign functions.
#[allow(unsafe_code)]
mod ffi {
    use std::ffi::c_void;

    extern "C" {
        /// Opens a model over `data[0..len]`; null when the bytes are not a
        /// valid model. The bytes must outlive the handle.
        pub(super) fn bcspl_new(data: *const u8, len: usize) -> *mut c_void;
        /// Frees a handle from [`bcspl_new`]; null is ignored.
        pub(super) fn bcspl_free(handle: *mut c_void);
        /// Encodes UTF-8 `text[0..len]`, writes at most `cap` ids to `out`
        /// and returns the full id count, or -1 on failure.
        pub(super) fn bcspl_encode(
            handle: *const c_void,
            text: *const u8,
            len: usize,
            out: *mut i32,
            cap: usize,
        ) -> i64;
        /// The id of special piece `which` (see [`super::SpecialPiece`]),
        /// or -1 when the model has none.
        pub(super) fn bcspl_special_id(handle: *const c_void, which: i32) -> i32;
        /// The id of the piece with exactly the text `piece[0..len]`, or -1.
        pub(super) fn bcspl_piece_to_id(handle: *const c_void, piece: *const u8, len: usize)
            -> i32;
        /// Points `*out` at the text of piece `id` inside the model bytes
        /// and returns its length, or -1 when `id` is out of range.
        pub(super) fn bcspl_id_to_piece(handle: *const c_void, id: i32, out: *mut *const u8)
            -> i64;
        /// Number of pieces in the model.
        pub(super) fn bcspl_vocab_size(handle: *const c_void) -> usize;
    }
}

/// Why a tokenizer could not be opened or a text could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenizerError {
    /// The bytes are not a SentencePiece Lite model the runtime accepts.
    #[error("not a valid SentencePiece Lite model")]
    InvalidModel,
    /// The runtime failed to encode the text.
    #[error("the tokenizer could not encode the text")]
    EncodeFailed,
    /// The runtime returned a token id outside the model's vocabulary.
    #[error("the tokenizer returned a token id outside its vocabulary")]
    IdOutOfRange,
}

/// A piece with a fixed role that most models reserve an id for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecialPiece {
    /// Stands in for text no piece covers.
    Unknown,
    /// Marks the beginning of a text.
    Begin,
    /// Marks the end of a text.
    End,
    /// Fills a batch out to one length.
    Padding,
}

impl SpecialPiece {
    /// The selector `bcspl_special_id` takes for this piece.
    const fn selector(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Begin => 1,
            Self::End => 2,
            Self::Padding => 3,
        }
    }
}

/// The token ids of one text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoding {
    /// Token ids in text order, cut to the limit [`Tokenizer::encode`] was
    /// given. Each is below [`Tokenizer::vocab_size`].
    pub ids: Vec<u32>,
    /// How many ids the whole text encodes to. Greater than `ids.len()`
    /// when the text was cut.
    pub total: usize,
}

impl Encoding {
    /// Whether the text had more tokens than the limit allowed.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total > self.ids.len()
    }
}

/// A loaded SentencePiece Lite model.
///
/// Holds the model bytes for as long as the runtime reads from them.
pub struct Tokenizer {
    /// The C++ processor; never null, freed on drop.
    handle: NonNull<c_void>,
    /// The model bytes the processor points into. Never read from Rust, and
    /// never moved or changed while `handle` lives (the heap allocation
    /// stays put when the `Tokenizer` moves).
    _model: Box<[u8]>,
    /// Number of pieces in the model, read once at load.
    vocab_size: usize,
}

// SAFETY: the processor has no thread affinity (no thread-local state, no
// interior pointers to the creating thread's stack), so it may move to and
// be dropped on another thread. It is deliberately not `Sync`.
#[allow(unsafe_code)]
unsafe impl Send for Tokenizer {}

impl std::fmt::Debug for Tokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokenizer")
            .field("vocab_size", &self.vocab_size)
            .finish_non_exhaustive()
    }
}

impl Tokenizer {
    /// Loads a model from the bytes of a `.spm.fb` file.
    ///
    /// # Arguments
    ///
    /// * `model` - The whole file. The runtime verifies the FlatBuffer
    ///   before using it.
    ///
    /// # Errors
    ///
    /// [`TokenizerError::InvalidModel`] when the runtime refuses the bytes.
    pub fn from_bytes(model: Vec<u8>) -> Result<Self, TokenizerError> {
        let model = model.into_boxed_slice();
        // SAFETY: `model` is a live allocation of `model.len()` bytes that
        // this `Tokenizer` owns, keeps at one address and frees only after
        // `Drop` has freed the handle.
        #[allow(unsafe_code)]
        let raw = unsafe { ffi::bcspl_new(model.as_ptr(), model.len()) };
        let handle = NonNull::new(raw).ok_or(TokenizerError::InvalidModel)?;
        // SAFETY: `handle` is a live processor from `bcspl_new`.
        #[allow(unsafe_code)]
        let vocab_size = unsafe { ffi::bcspl_vocab_size(handle.as_ptr()) };
        Ok(Self {
            handle,
            _model: model,
            vocab_size,
        })
    }

    /// Number of pieces in the model; every id is below it.
    #[must_use]
    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }

    /// The id the model reserves for `piece`, or `None` when it has none.
    #[must_use]
    pub fn special_id(&self, piece: SpecialPiece) -> Option<u32> {
        // SAFETY: `handle` is a live processor; the selector is one of the
        // four the shim knows.
        #[allow(unsafe_code)]
        let id = unsafe { ffi::bcspl_special_id(self.handle.as_ptr(), piece.selector()) };
        u32::try_from(id)
            .ok()
            .filter(|id| usize::try_from(*id).is_ok_and(|id| id < self.vocab_size))
    }

    /// The id of the piece whose text is exactly `piece`, or `None` when
    /// the model has no such piece.
    ///
    /// A piece's text is what the model stores: a word-initial piece starts
    /// with `\u{2581}` (`▁`) and a byte piece reads `<0x41>`.
    #[must_use]
    pub fn piece_to_id(&self, piece: &str) -> Option<u32> {
        // SAFETY: `handle` is a live processor and `piece` is `piece.len()`
        // readable bytes.
        #[allow(unsafe_code)]
        let id =
            unsafe { ffi::bcspl_piece_to_id(self.handle.as_ptr(), piece.as_ptr(), piece.len()) };
        u32::try_from(id).ok()
    }

    /// The text of piece `id`, or `None` when `id` is not below
    /// [`Tokenizer::vocab_size`] or the model stores text that is not
    /// UTF-8.
    #[must_use]
    pub fn id_to_piece(&self, id: u32) -> Option<&str> {
        let id = i32::try_from(id).ok()?;
        let mut start: *const u8 = std::ptr::null();
        // SAFETY: `handle` is a live processor and `start` is a writable
        // pointer slot.
        #[allow(unsafe_code)]
        let len = unsafe { ffi::bcspl_id_to_piece(self.handle.as_ptr(), id, &raw mut start) };
        let len = usize::try_from(len).ok()?;
        if start.is_null() {
            return None;
        }
        // SAFETY: the shim returned `len` bytes at `start` inside the model
        // bytes, which this `Tokenizer` owns and never changes; the borrow
        // of `self` keeps them alive for the returned lifetime.
        #[allow(unsafe_code)]
        let bytes = unsafe { std::slice::from_raw_parts(start, len) };
        std::str::from_utf8(bytes).ok()
    }

    /// Encodes `text` into token ids, keeping at most `limit` of them.
    ///
    /// The ids are the model's own: no beginning or end marker is added.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to encode.
    /// * `limit` - The most ids to return; the rest of the text is dropped
    ///   and [`Encoding::total`] says how many there were.
    ///
    /// # Errors
    ///
    /// [`TokenizerError::EncodeFailed`] when the runtime fails, and
    /// [`TokenizerError::IdOutOfRange`] when it returns an id outside the
    /// vocabulary (a model that disagrees with itself).
    pub fn encode(&self, text: &str, limit: usize) -> Result<Encoding, TokenizerError> {
        let mut raw = vec![0_i32; limit];
        // SAFETY: `handle` is a live processor, `text` is `text.len()`
        // readable bytes, and `raw` is `limit` writable `i32`s; the shim
        // writes at most `limit` of them.
        #[allow(unsafe_code)]
        let total = unsafe {
            ffi::bcspl_encode(
                self.handle.as_ptr(),
                text.as_ptr(),
                text.len(),
                raw.as_mut_ptr(),
                limit,
            )
        };
        let total = usize::try_from(total).map_err(|_| TokenizerError::EncodeFailed)?;
        raw.truncate(total.min(limit));
        let ids = raw
            .into_iter()
            .map(|id| {
                u32::try_from(id)
                    .ok()
                    .filter(|id| usize::try_from(*id).is_ok_and(|id| id < self.vocab_size))
                    .ok_or(TokenizerError::IdOutOfRange)
            })
            .collect::<Result<Vec<u32>, TokenizerError>>()?;
        Ok(Encoding { ids, total })
    }
}

impl Drop for Tokenizer {
    fn drop(&mut self) {
        // SAFETY: `handle` came from `bcspl_new` and is freed exactly once,
        // before the model bytes it points into.
        #[allow(unsafe_code)]
        unsafe {
            ffi::bcspl_free(self.handle.as_ptr());
        }
    }
}
