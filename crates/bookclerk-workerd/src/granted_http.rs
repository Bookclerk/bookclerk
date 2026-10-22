//! Sans-IO HTTP/1.1 parsing for the granted reverse channel.
//!
//! Every byte on the granted channel comes from a plugin isolate, so the
//! request head, the chunked body framing and the query string are parsed
//! here over plain byte slices, with no socket in sight. [`crate::granted`]
//! feeds these from its reader; unit tests and the `granted_http` fuzz
//! target feed them directly, so all three drive the same code
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! Limits enforced here are structural (head size, chunk-size digits,
//! trailer count). Body byte caps belong to the route and stay in
//! [`crate::granted`].
//!
//! Parsing is strict rather than lenient: the only client is the plugin
//! runtime's own `fetch`, so anything RFC 9112 does not require accepting
//! (bare LF, whitespace before a chunk size, a second transfer coding) is
//! rejected and answered with `400 Bad Request` instead of being guessed at.

use std::fmt;

/// Largest request head (request line plus headers plus the blank line)
/// accepted before the connection is dropped.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Most hex digits a chunk-size field may carry. Sixteen digits already
/// overflow `u64`; real clients send far fewer.
pub const MAX_CHUNK_SIZE_DIGITS: usize = 16;

/// Longest chunk-size line (digits, extensions and CRLF) before the body is
/// rejected as malformed.
pub const MAX_CHUNK_LINE_BYTES: usize = 1024;

/// Most trailer lines accepted after the terminating chunk.
pub const MAX_TRAILER_LINES: usize = 16;

/// Parsed request line and headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    /// HTTP method token as sent (case preserved).
    pub method: String,
    /// Request target as sent, including any query string.
    pub path: String,
    /// Header fields in wire order, names and values trimmed, names with
    /// their original case.
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    /// The path without its query string, and the query string (empty when
    /// absent).
    #[must_use]
    pub fn split_query(&self) -> (&str, &str) {
        self.path
            .split_once('?')
            .unwrap_or((self.path.as_str(), ""))
    }
}

/// Why a request head was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    /// The head grew past [`MAX_HEAD_BYTES`] without a blank line.
    TooLarge,
    /// The head is not UTF-8.
    NotUtf8,
    /// The request line is missing its method or target.
    BadRequestLine,
    /// A header line has no `:`.
    BadHeaderLine,
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("request headers too large"),
            Self::NotUtf8 => f.write_str("request headers are not UTF-8"),
            Self::BadRequestLine => f.write_str("malformed request line"),
            Self::BadHeaderLine => f.write_str("malformed header line"),
        }
    }
}

impl std::error::Error for HeadError {}

/// Outcome of [`parse_request_head`] on the bytes buffered so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadParse {
    /// No blank line yet; read more.
    Incomplete,
    /// A complete head; `consumed` bytes belong to it and the rest of the
    /// buffer is the start of the body.
    Complete {
        /// The parsed head.
        head: RequestHead,
        /// Bytes of `buf` the head occupied, including the blank line.
        consumed: usize,
    },
}

/// Parses an HTTP/1.1 request head from the start of `buf`.
///
/// # Errors
///
/// Returns [`HeadError`] when the head is oversized, not UTF-8, or has a
/// malformed request or header line. An incomplete head is not an error
/// until it exceeds [`MAX_HEAD_BYTES`].
pub fn parse_request_head(buf: &[u8]) -> Result<HeadParse, HeadError> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        if buf.len() > MAX_HEAD_BYTES {
            return Err(HeadError::TooLarge);
        }
        return Ok(HeadParse::Incomplete);
    };
    let consumed = end + 4;
    if consumed > MAX_HEAD_BYTES {
        return Err(HeadError::TooLarge);
    }
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| HeadError::NotUtf8)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    // Lines are split on CRLF only, so a bare CR or LF inside a line is the
    // guest trying to smuggle a second line; RFC 9112 lets a server reject it.
    if request_line.contains(['\r', '\n']) {
        return Err(HeadError::BadRequestLine);
    }
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(HeadError::BadRequestLine);
    };
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.contains(['\r', '\n']) {
            return Err(HeadError::BadHeaderLine);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HeadError::BadHeaderLine);
        };
        // RFC 9112 §5.1: no whitespace between the field name and the colon,
        // and a field line never starts with whitespace (obs-fold is gone).
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(HeadError::BadHeaderLine);
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
    Ok(HeadParse::Complete {
        head: RequestHead {
            method: method.to_string(),
            path: path.to_string(),
            headers,
        },
        consumed,
    })
}

/// The first header named `name` (ASCII case-insensitive).
#[must_use]
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Every transfer coding named by the `Transfer-Encoding` headers, in wire
/// order, lowercased and trimmed, with empty list elements dropped.
#[must_use]
pub fn transfer_encodings(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .flat_map(|(_, value)| value.split(','))
        .map(|coding| coding.trim().to_ascii_lowercase())
        .filter(|coding| !coding.is_empty())
        .collect()
}

/// True when a `Transfer-Encoding` header lists the `chunked` coding as a
/// token (so `chunkedx` or `notchunked` do not count).
#[must_use]
pub fn transfer_encoding_is_chunked(headers: &[(String, String)]) -> bool {
    transfer_encodings(headers)
        .iter()
        .any(|coding| coding == "chunked")
}

/// How the request frames its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFraming {
    /// No `Content-Length` and no chunked encoding: the body runs to EOF.
    UntilEof,
    /// `Content-Length` in bytes.
    Length(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
}

/// Why the body framing headers were rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingError {
    /// `Content-Length` is not a decimal `usize`, or two differ.
    BadContentLength,
    /// Both `Content-Length` and chunked encoding are present.
    Ambiguous,
    /// `Transfer-Encoding` names a coding other than a single `chunked`
    /// (`gzip, chunked`, `identity`, `chunked, chunked`); the channel does
    /// not decode those, so the body length would be unknown.
    UnsupportedTransferEncoding,
}

impl fmt::Display for FramingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadContentLength => f.write_str("malformed Content-Length"),
            Self::Ambiguous => {
                f.write_str("both Content-Length and Transfer-Encoding: chunked present")
            }
            Self::UnsupportedTransferEncoding => {
                f.write_str("unsupported Transfer-Encoding (only a single chunked is accepted)")
            }
        }
    }
}

impl std::error::Error for FramingError {}

/// Decides the body framing from the headers.
///
/// A `Content-Length` that is not a decimal number, or that disagrees with a
/// second `Content-Length`, is rejected rather than ignored, so a malformed
/// header can never turn a bounded body into a read-until-EOF one. A
/// `Transfer-Encoding` is accepted only when it names exactly one coding and
/// that coding is `chunked`; any other coding list is rejected because the
/// channel cannot know where such a body ends.
///
/// # Errors
///
/// Returns [`FramingError`] on a malformed or ambiguous framing.
pub fn body_framing(headers: &[(String, String)]) -> Result<BodyFraming, FramingError> {
    let mut length: Option<usize> = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        // RFC 9112: `Content-Length = 1*DIGIT`; `str::parse` would also take
        // a leading `+`, which is not a length.
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(FramingError::BadContentLength);
        }
        let parsed: usize = value.parse().map_err(|_| FramingError::BadContentLength)?;
        match length {
            Some(seen) if seen != parsed => return Err(FramingError::BadContentLength),
            _ => length = Some(parsed),
        }
    }
    let encodings = transfer_encodings(headers);
    let chunked = match encodings.as_slice() {
        [] => false,
        [only] if only == "chunked" => true,
        _ => return Err(FramingError::UnsupportedTransferEncoding),
    };
    match (length, chunked) {
        (Some(_), true) => Err(FramingError::Ambiguous),
        (Some(len), false) => Ok(BodyFraming::Length(len)),
        (None, true) => Ok(BodyFraming::Chunked),
        (None, false) => Ok(BodyFraming::UntilEof),
    }
}

/// True when a well-formed `Content-Length` declares more than `cap` bytes.
/// A malformed one is left for [`body_framing`] to reject.
#[must_use]
pub fn declared_content_length_over_cap(headers: &[(String, String)], cap: u32) -> bool {
    matches!(
        body_framing(headers),
        Ok(BodyFraming::Length(len)) if len > usize::try_from(cap).unwrap_or(usize::MAX)
    )
}

/// Why a chunked body was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkedError {
    /// A chunk-size field is empty, not hex, or longer than
    /// [`MAX_CHUNK_SIZE_DIGITS`].
    BadChunkSize,
    /// A chunk-size line ran past [`MAX_CHUNK_LINE_BYTES`] without CRLF.
    ChunkLineTooLong,
    /// Chunk data was not followed by CRLF.
    MissingChunkTerminator,
    /// More than [`MAX_TRAILER_LINES`] trailer lines, or a trailer line
    /// longer than [`MAX_CHUNK_LINE_BYTES`].
    BadTrailer,
}

impl fmt::Display for ChunkedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadChunkSize => f.write_str("malformed chunk size"),
            Self::ChunkLineTooLong => f.write_str("chunk-size line too long"),
            Self::MissingChunkTerminator => f.write_str("chunk data not followed by CRLF"),
            Self::BadTrailer => f.write_str("malformed chunked trailer"),
        }
    }
}

impl std::error::Error for ChunkedError {}

/// Where the decoder is inside the chunked framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkedState {
    /// Expecting a chunk-size line.
    Size,
    /// Inside chunk data with this many bytes still to copy.
    Data(u64),
    /// Expecting the CRLF that ends a chunk's data.
    DataCrlf,
    /// After the zero-size chunk, consuming trailer lines; the count is
    /// how many have been seen.
    Trailers(usize),
    /// The terminating blank line has been consumed.
    Done,
}

/// Progress made by one [`ChunkedDecoder::decode`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkedProgress {
    /// Input bytes consumed; the caller drops them from its buffer.
    pub consumed: usize,
    /// True once the terminating chunk and its trailers are consumed.
    pub done: bool,
}

/// Incremental `Transfer-Encoding: chunked` decoder.
///
/// Decoding is incremental so a chunk of any declared size streams through
/// without being buffered whole: the caller's buffer only ever holds what the
/// peer has sent, never what it promised. The decoder itself keeps a few
/// bytes of state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkedDecoder {
    /// Current framing position.
    state: ChunkedState,
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkedDecoder {
    /// A decoder positioned before the first chunk-size line.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: ChunkedState::Size,
        }
    }

    /// True once the terminating chunk has been consumed.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.state == ChunkedState::Done
    }

    /// Decodes as much of `input` as is complete, appending chunk data to
    /// `out`.
    ///
    /// Stops at the first incomplete element (a size line without its CRLF,
    /// data the peer has not sent yet) and reports how many bytes were
    /// consumed; the caller keeps the rest and calls again with more input.
    ///
    /// # Errors
    ///
    /// Returns [`ChunkedError`] on malformed framing; the decoder is then
    /// unusable and the connection should be dropped.
    pub fn decode(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<ChunkedProgress, ChunkedError> {
        let mut pos = 0usize;
        loop {
            let rest = &input[pos..];
            match self.state {
                ChunkedState::Done => {
                    return Ok(ChunkedProgress {
                        consumed: pos,
                        done: true,
                    });
                }
                ChunkedState::Size => {
                    let Some(line_end) = find_crlf(rest, MAX_CHUNK_LINE_BYTES)
                        .map_err(|()| ChunkedError::ChunkLineTooLong)?
                    else {
                        break;
                    };
                    let size = parse_chunk_size(&rest[..line_end])?;
                    pos += line_end + 2;
                    self.state = if size == 0 {
                        ChunkedState::Trailers(0)
                    } else {
                        ChunkedState::Data(size)
                    };
                }
                ChunkedState::Data(remaining) => {
                    if rest.is_empty() {
                        break;
                    }
                    let take = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(rest.len());
                    out.extend_from_slice(&rest[..take]);
                    pos += take;
                    // `take <= remaining` by construction.
                    let left = remaining.saturating_sub(u64::try_from(take).unwrap_or(u64::MAX));
                    self.state = if left == 0 {
                        ChunkedState::DataCrlf
                    } else {
                        ChunkedState::Data(left)
                    };
                }
                ChunkedState::DataCrlf => {
                    if rest.len() < 2 {
                        break;
                    }
                    if &rest[..2] != b"\r\n" {
                        return Err(ChunkedError::MissingChunkTerminator);
                    }
                    pos += 2;
                    self.state = ChunkedState::Size;
                }
                ChunkedState::Trailers(seen) => {
                    let Some(line_end) = find_crlf(rest, MAX_CHUNK_LINE_BYTES)
                        .map_err(|()| ChunkedError::BadTrailer)?
                    else {
                        break;
                    };
                    pos += line_end + 2;
                    if line_end == 0 {
                        self.state = ChunkedState::Done;
                    } else if seen + 1 > MAX_TRAILER_LINES {
                        return Err(ChunkedError::BadTrailer);
                    } else {
                        self.state = ChunkedState::Trailers(seen + 1);
                    }
                }
            }
        }
        Ok(ChunkedProgress {
            consumed: pos,
            done: self.state == ChunkedState::Done,
        })
    }
}

/// Position of the first CRLF in `buf`, `Ok(None)` when none is complete
/// yet, or `Err(())` when more than `max` bytes passed without one.
fn find_crlf(buf: &[u8], max: usize) -> Result<Option<usize>, ()> {
    match buf.windows(2).position(|w| w == b"\r\n") {
        Some(pos) if pos + 2 > max => Err(()),
        Some(pos) => Ok(Some(pos)),
        None if buf.len() >= max => Err(()),
        None => Ok(None),
    }
}

/// Parses one chunk-size field (hex digits, then an optional `;extension`).
///
/// The line must start with a hex digit (RFC 9112 §7.1 allows no leading
/// whitespace); trailing spaces and tabs before the extension or CRLF are
/// tolerated as the usual bad-whitespace allowance.
///
/// # Errors
///
/// Returns [`ChunkedError::BadChunkSize`] when the digits are missing, not
/// hex, preceded by whitespace, or more than [`MAX_CHUNK_SIZE_DIGITS`].
fn parse_chunk_size(line: &[u8]) -> Result<u64, ChunkedError> {
    let field = line
        .iter()
        .position(|&b| b == b';')
        .map_or(line, |end| &line[..end]);
    let digits = trim_trailing_ascii_ws(field);
    if digits.is_empty() || digits.len() > MAX_CHUNK_SIZE_DIGITS {
        return Err(ChunkedError::BadChunkSize);
    }
    let mut size: u64 = 0;
    for &b in digits {
        let digit = u64::from(
            char::from(b)
                .to_digit(16)
                .ok_or(ChunkedError::BadChunkSize)?,
        );
        size = size
            .checked_mul(16)
            .and_then(|s| s.checked_add(digit))
            .ok_or(ChunkedError::BadChunkSize)?;
    }
    Ok(size)
}

/// `bytes` without trailing spaces and tabs.
fn trim_trailing_ascii_ws(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t'))
        .map_or(0, |p| p + 1);
    &bytes[..end]
}

/// The percent-decoded value of the first `name=` pair in `query`.
#[must_use]
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| percent_decode(v))
}

/// Decodes `%HH` escapes and `+` (as space); malformed escapes are kept as
/// written, and the result is UTF-8 with replacement characters.
///
/// Works on bytes throughout: a `%` escape may end inside a multi-byte
/// character, so slicing the `str` would panic.
#[must_use]
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Some(decoded) = hex_pair(bytes[i + 1], bytes[i + 2]) {
                out.push(decoded);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte two hex digits spell, if both are hex.
fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let hi = char::from(hi).to_digit(16)?;
    let lo = char::from(lo).to_digit(16)?;
    u8::try_from(hi * 16 + lo).ok()
}

/// Why a request body was rejected or cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// The framing headers were malformed or ambiguous.
    Framing(FramingError),
    /// The chunked framing was malformed.
    Chunked(ChunkedError),
    /// The body (declared or received) exceeds the route's byte cap.
    TooLarge {
        /// The cap the body ran past, in bytes.
        cap: usize,
    },
    /// The stream ended before the declared `Content-Length` or the
    /// terminating chunk.
    Truncated,
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framing(err) => err.fmt(f),
            Self::Chunked(err) => err.fmt(f),
            Self::TooLarge { cap } => write!(f, "body exceeds {cap} bytes"),
            Self::Truncated => f.write_str("body ended before its declared end"),
        }
    }
}

impl std::error::Error for BodyError {}

impl From<FramingError> for BodyError {
    fn from(err: FramingError) -> Self {
        Self::Framing(err)
    }
}

impl From<ChunkedError> for BodyError {
    fn from(err: ChunkedError) -> Self {
        Self::Chunked(err)
    }
}

/// Sans-IO reader for one request body under its declared framing.
///
/// [`crate::granted`] feeds it whatever bytes arrive on the socket and sends
/// or collects what it yields; the `granted_http` fuzz target and the corpus
/// replay feed it the same bytes in arbitrary splits. Framing dispatch, the
/// route's byte cap and end-of-stream handling therefore run through one
/// code path for all three (`docs/adr/untrusted-input-fuzzing.md`).
///
/// A `Content-Length` body yields its first `len` bytes and ignores anything
/// after them (the connection closes after one request). A chunked body is
/// decoded incrementally; a read-until-EOF body yields every byte and is
/// done only at [`Self::finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyReader {
    /// How the request framed its body.
    framing: BodyFraming,
    /// Route byte cap on the decoded body, when the route has one.
    cap: Option<usize>,
    /// Decoded body bytes yielded so far.
    yielded: usize,
    /// Undecoded chunked framing bytes awaiting more input.
    pending: Vec<u8>,
    /// Chunked decoder state; unused for the other framings.
    chunked: ChunkedDecoder,
    /// True once the body is complete.
    done: bool,
}

impl BodyReader {
    /// Starts a reader for the body `headers` describe.
    ///
    /// # Errors
    ///
    /// Returns [`BodyError::Framing`] when the framing headers are malformed
    /// or ambiguous.
    pub fn new(headers: &[(String, String)]) -> Result<Self, BodyError> {
        Ok(Self::for_framing(body_framing(headers)?))
    }

    /// Starts a reader for an already decided framing.
    #[must_use]
    pub fn for_framing(framing: BodyFraming) -> Self {
        Self {
            framing,
            cap: None,
            yielded: 0,
            pending: Vec::new(),
            chunked: ChunkedDecoder::new(),
            done: matches!(framing, BodyFraming::Length(0)),
        }
    }

    /// Caps the decoded body at `cap` bytes; a body that declares or
    /// delivers more fails with [`BodyError::TooLarge`].
    #[must_use]
    pub fn with_cap(mut self, cap: usize) -> Self {
        self.cap = Some(cap);
        self
    }

    /// The framing this reader follows.
    #[must_use]
    pub fn framing(&self) -> BodyFraming {
        self.framing
    }

    /// True once the body is complete (a read-until-EOF body only after
    /// [`Self::finish`]).
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Feeds the next bytes from the stream, appending decoded body bytes to
    /// `out`.
    ///
    /// # Returns
    ///
    /// `true` once the body is complete; later input is ignored.
    ///
    /// # Errors
    ///
    /// [`BodyError::TooLarge`] when the declared or received body passes the
    /// cap, or [`BodyError::Chunked`] on malformed chunked framing. Either
    /// error is final.
    pub fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<bool, BodyError> {
        if self.done {
            return Ok(true);
        }
        match self.framing {
            BodyFraming::Length(len) => {
                self.check_cap(len)?;
                let take = input.len().min(len - self.yielded);
                out.extend_from_slice(&input[..take]);
                self.yielded += take;
                self.done = self.yielded == len;
            }
            BodyFraming::UntilEof => {
                let total = self.yielded.saturating_add(input.len());
                self.check_cap(total)?;
                out.extend_from_slice(input);
                self.yielded = total;
            }
            BodyFraming::Chunked => {
                self.pending.extend_from_slice(input);
                let before = out.len();
                let decoded = self.chunked.decode(&self.pending, out);
                let yielded = self.yielded.saturating_add(out.len() - before);
                // The cap is judged on the bytes decoded so far, before any
                // framing error further into this push, so the verdict does
                // not depend on how the stream was split: a reader fed one
                // byte at a time hits the cap before it sees the bad framing.
                // Nothing past the cap is yielded: the bytes this call
                // decoded are taken back. Framing bytes the decoder has not
                // consumed yet are bounded by the longest chunk line, so
                // `pending` cannot grow without bound. Bytes after the
                // terminating chunk belong to the connection, not the body.
                if let Err(err) = self.check_cap(yielded) {
                    out.truncate(before);
                    return Err(err);
                }
                // Bytes decoded ahead of a framing error stay in `out`: they
                // are exactly what a reader fed smaller pieces had already
                // yielded, and the caller discards the body on any error.
                let progress = decoded?;
                self.pending.drain(..progress.consumed);
                self.yielded = yielded;
                self.done = progress.done;
                if self.done {
                    self.pending.clear();
                }
            }
        }
        Ok(self.done)
    }

    /// Reports end of stream.
    ///
    /// # Errors
    ///
    /// [`BodyError::Truncated`] when a `Content-Length` or chunked body is
    /// incomplete. A read-until-EOF body completes here.
    pub fn finish(&mut self) -> Result<(), BodyError> {
        match self.framing {
            BodyFraming::UntilEof => {
                self.done = true;
                Ok(())
            }
            BodyFraming::Length(_) | BodyFraming::Chunked => {
                if self.done {
                    Ok(())
                } else {
                    Err(BodyError::Truncated)
                }
            }
        }
    }

    /// Fails when `bytes` passes the route cap.
    fn check_cap(&self, bytes: usize) -> Result<(), BodyError> {
        match self.cap {
            Some(cap) if bytes > cap => Err(BodyError::TooLarge { cap }),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    #[test]
    fn body_reader_follows_each_framing_and_its_cap() {
        let h = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        // Content-Length: exactly len bytes, extras ignored, EOF early is an error.
        let mut r = BodyReader::new(&h(&[("Content-Length", "5")])).unwrap();
        let mut out = Vec::new();
        assert!(!r.push(b"hel", &mut out).unwrap());
        assert_eq!(r.finish(), Err(BodyError::Truncated));
        assert!(r.push(b"lo!!", &mut out).unwrap());
        assert_eq!(out, b"hello");
        assert_eq!(r.finish(), Ok(()));
        let mut r = BodyReader::new(&h(&[("Content-Length", "5")]))
            .unwrap()
            .with_cap(4);
        assert_eq!(
            r.push(b"", &mut Vec::new()),
            Err(BodyError::TooLarge { cap: 4 })
        );
        // Until EOF: everything, done at finish, capped.
        let mut r = BodyReader::new(&h(&[])).unwrap().with_cap(6);
        let mut out = Vec::new();
        assert!(!r.push(b"abc", &mut out).unwrap());
        assert!(!r.push(b"def", &mut out).unwrap());
        assert_eq!(r.finish(), Ok(()));
        assert!(r.is_done());
        assert_eq!(out, b"abcdef");
        let mut r = BodyReader::new(&h(&[])).unwrap().with_cap(6);
        assert_eq!(
            r.push(b"abcdefg", &mut Vec::new()),
            Err(BodyError::TooLarge { cap: 6 })
        );
        // Chunked: decoded across splits, truncated at EOF, capped on output.
        let mut r = BodyReader::new(&h(&[("Transfer-Encoding", "chunked")])).unwrap();
        let mut out = Vec::new();
        assert!(!r.push(b"5\r\nhel", &mut out).unwrap());
        assert_eq!(r.finish(), Err(BodyError::Truncated));
        assert!(!r.push(b"lo\r", &mut out).unwrap());
        assert!(r.push(b"\n0\r\n\r\n", &mut out).unwrap());
        assert_eq!(out, b"hello");
        assert_eq!(r.finish(), Ok(()));
        let mut r = BodyReader::new(&h(&[("Transfer-Encoding", "chunked")]))
            .unwrap()
            .with_cap(3);
        let mut out = b"kept".to_vec();
        assert_eq!(
            r.push(b"5\r\nhello\r\n", &mut out),
            Err(BodyError::TooLarge { cap: 3 })
        );
        assert_eq!(out, b"kept", "nothing past the cap is yielded");
        let mut r = BodyReader::new(&h(&[("Transfer-Encoding", "chunked")])).unwrap();
        assert_eq!(
            r.push(b"zz\r\n", &mut Vec::new()),
            Err(BodyError::Chunked(ChunkedError::BadChunkSize))
        );
        assert_eq!(
            BodyReader::new(&h(&[("Content-Length", "+1")])).err(),
            Some(BodyError::Framing(FramingError::BadContentLength))
        );
    }

    use super::*;

    fn head(buf: &[u8]) -> (RequestHead, usize) {
        match parse_request_head(buf).expect("head") {
            HeadParse::Complete { head, consumed } => (head, consumed),
            HeadParse::Incomplete => panic!("incomplete"),
        }
    }

    #[test]
    fn request_head_parses_and_reports_body_prefix() {
        let (h, consumed) =
            head(b"PUT /put?key=a%20b HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc");
        assert_eq!(h.method, "PUT");
        assert_eq!(h.split_query(), ("/put", "key=a%20b"));
        assert_eq!(header(&h.headers, "content-length"), Some("3"));
        assert_eq!(
            consumed,
            b"PUT /put?key=a%20b HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\n".len()
        );
        assert_eq!(
            parse_request_head(b"GET / HTTP/1.1\r\nHost"),
            Ok(HeadParse::Incomplete)
        );
    }

    #[test]
    fn request_head_rejects_oversized_and_malformed() {
        let big = vec![b'a'; MAX_HEAD_BYTES + 1];
        assert_eq!(parse_request_head(&big), Err(HeadError::TooLarge));
        assert_eq!(
            parse_request_head(b"GET\r\n\r\n"),
            Err(HeadError::BadRequestLine)
        );
        assert_eq!(
            parse_request_head(b"GET / HTTP/1.1\r\nnocolon\r\n\r\n"),
            Err(HeadError::BadHeaderLine)
        );
        assert_eq!(
            parse_request_head(b"GET / HTTP/1.1\r\nx: \xff\r\n\r\n"),
            Err(HeadError::NotUtf8)
        );
    }

    #[test]
    fn request_head_rejects_whitespace_names_and_bare_line_breaks() {
        assert!(matches!(
            parse_request_head(b"GET /x HTTP/1.1\r\nContent-Length : 5\r\n\r\n"),
            Err(HeadError::BadHeaderLine)
        ));
        assert!(matches!(
            parse_request_head(b"GET /x HTTP/1.1\r\n Transfer-Encoding: chunked\r\n\r\n"),
            Err(HeadError::BadHeaderLine)
        ));
        assert!(matches!(
            parse_request_head(b"GET /x HTTP/1.1\r\n: v\r\n\r\n"),
            Err(HeadError::BadHeaderLine)
        ));
        assert!(matches!(
            parse_request_head(b"GET /x HTTP/1.1\r\nX: a\nContent-Length: 5\r\n\r\n"),
            Err(HeadError::BadHeaderLine)
        ));
        assert!(matches!(
            parse_request_head(b"GET /x HTTP/1.1\r\nX: a\rb\r\n\r\n"),
            Err(HeadError::BadHeaderLine)
        ));
        assert!(matches!(
            parse_request_head(b"GET /x\nHost: y HTTP/1.1\r\n\r\n"),
            Err(HeadError::BadRequestLine)
        ));
        // Optional whitespace around the value is still fine.
        match parse_request_head(b"GET /x HTTP/1.1\r\nContent-Length:  5 \r\n\r\n") {
            Ok(HeadParse::Complete { head, .. }) => {
                assert_eq!(head.headers, vec![("Content-Length".into(), "5".into())]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn framing_rejects_malformed_and_ambiguous_lengths() {
        let h = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        assert_eq!(body_framing(&h(&[])), Ok(BodyFraming::UntilEof));
        assert_eq!(
            body_framing(&h(&[("Content-Length", "12")])),
            Ok(BodyFraming::Length(12))
        );
        assert_eq!(
            body_framing(&h(&[("Content-Length", "x")])),
            Err(FramingError::BadContentLength)
        );
        assert_eq!(
            body_framing(&h(&[("Content-Length", "+1")])),
            Err(FramingError::BadContentLength)
        );
        assert_eq!(
            body_framing(&h(&[("Content-Length", "")])),
            Err(FramingError::BadContentLength)
        );
        assert_eq!(
            body_framing(&h(&[("Content-Length", "1"), ("content-length", "2")])),
            Err(FramingError::BadContentLength)
        );
        assert_eq!(
            body_framing(&h(&[
                ("Content-Length", "1"),
                ("Transfer-Encoding", "chunked")
            ])),
            Err(FramingError::Ambiguous)
        );
        assert_eq!(
            body_framing(&h(&[("Transfer-Encoding", "Chunked")])),
            Ok(BodyFraming::Chunked)
        );
        assert_eq!(
            body_framing(&h(&[("Transfer-Encoding", " chunked , ")])),
            Ok(BodyFraming::Chunked)
        );
        for te in ["gzip, chunked", "chunked, chunked", "identity", "chunkedx"] {
            assert_eq!(
                body_framing(&h(&[("Transfer-Encoding", te)])),
                Err(FramingError::UnsupportedTransferEncoding),
                "{te}"
            );
            assert_eq!(
                body_framing(&h(&[("Content-Length", "1"), ("Transfer-Encoding", te)])),
                Err(FramingError::UnsupportedTransferEncoding),
                "{te}"
            );
        }
        assert_eq!(
            body_framing(&h(&[
                ("Transfer-Encoding", "gzip"),
                ("Transfer-Encoding", "chunked")
            ])),
            Err(FramingError::UnsupportedTransferEncoding)
        );
        assert!(transfer_encoding_is_chunked(&h(&[(
            "transfer-encoding",
            "gzip, Chunked"
        )])));
        assert!(!transfer_encoding_is_chunked(&h(&[(
            "transfer-encoding",
            "notchunked"
        )])));
        assert!(declared_content_length_over_cap(
            &h(&[("Content-Length", "11")]),
            10
        ));
        assert!(!declared_content_length_over_cap(
            &h(&[("Content-Length", "10")]),
            10
        ));
        assert!(!declared_content_length_over_cap(
            &h(&[("Content-Length", "nope")]),
            10
        ));
    }

    fn decode_all(input: &[u8]) -> Result<(Vec<u8>, usize, bool), ChunkedError> {
        let mut dec = ChunkedDecoder::new();
        let mut out = Vec::new();
        let progress = dec.decode(input, &mut out)?;
        Ok((out, progress.consumed, progress.done))
    }

    #[test]
    fn chunk_size_rejects_leading_whitespace_but_allows_trailing() {
        assert_eq!(
            decode_all(b" 5\r\nhello\r\n0\r\n\r\n"),
            Err(ChunkedError::BadChunkSize)
        );
        assert_eq!(decode_all(b"\t0\r\n\r\n"), Err(ChunkedError::BadChunkSize));
        let (out, _, done) = decode_all(b"5 \r\nhello\r\n0\r\n\r\n").expect("trailing ws");
        assert_eq!(out, b"hello");
        assert!(done);
    }

    #[test]
    fn chunked_decodes_extensions_and_trailers() {
        let (out, consumed, done) =
            decode_all(b"5;ext=1\r\nhello\r\n1\r\n!\r\n0\r\nX-T: 1\r\n\r\nNEXT").unwrap();
        assert_eq!(out, b"hello!");
        assert!(done);
        assert_eq!(
            consumed,
            b"5;ext=1\r\nhello\r\n1\r\n!\r\n0\r\nX-T: 1\r\n\r\n".len()
        );
    }

    #[test]
    fn chunked_never_treats_garbage_as_the_last_chunk() {
        assert_eq!(
            decode_all(b"zz\r\nhello\r\n0\r\n\r\n"),
            Err(ChunkedError::BadChunkSize)
        );
        assert_eq!(
            decode_all(b"\r\n0\r\n\r\n"),
            Err(ChunkedError::BadChunkSize)
        );
        assert_eq!(
            decode_all(b"\xff\xfe\r\n0\r\n\r\n"),
            Err(ChunkedError::BadChunkSize)
        );
        assert_eq!(
            decode_all(b"5\r\nhelloXX0\r\n\r\n"),
            Err(ChunkedError::MissingChunkTerminator)
        );
        assert_eq!(
            decode_all(b"fffffffffffffffff\r\n"),
            Err(ChunkedError::BadChunkSize)
        );
        let long = vec![b'1'; MAX_CHUNK_LINE_BYTES + 1];
        assert_eq!(decode_all(&long), Err(ChunkedError::ChunkLineTooLong));
    }

    #[test]
    fn chunked_streams_a_huge_chunk_without_buffering_it() {
        let mut dec = ChunkedDecoder::new();
        let mut out = Vec::new();
        let p = dec.decode(b"ffffffff\r\nabc", &mut out).unwrap();
        assert_eq!(out, b"abc");
        assert_eq!(p.consumed, b"ffffffff\r\nabc".len());
        assert!(!p.done);
        let p = dec.decode(b"def", &mut out).unwrap();
        assert_eq!(out, b"abcdef");
        assert_eq!(p.consumed, 3);
    }

    #[test]
    fn chunked_is_split_invariant() {
        let wire = b"3\r\nabc\r\nA\r\n0123456789\r\n0\r\n\r\n";
        let (whole, consumed, done) = decode_all(wire).unwrap();
        assert!(done);
        assert_eq!(consumed, wire.len());
        for split in 0..=wire.len() {
            let mut dec = ChunkedDecoder::new();
            let mut out = Vec::new();
            let mut buf = wire[..split].to_vec();
            let p = dec.decode(&buf, &mut out).unwrap();
            buf.drain(..p.consumed);
            buf.extend_from_slice(&wire[split..]);
            let p = dec.decode(&buf, &mut out).unwrap();
            assert!(p.done, "split at {split}");
            assert_eq!(out, whole, "split at {split}");
        }
    }

    #[test]
    fn percent_decode_is_byte_safe() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%C3%A9"), "é");
        assert_eq!(percent_decode("%zz%"), "%zz%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(
            query_param("a=1&key=x%2Fy&key=z", "key"),
            Some("x/y".into())
        );
        assert_eq!(query_param("a=1", "key"), None);
    }
}
