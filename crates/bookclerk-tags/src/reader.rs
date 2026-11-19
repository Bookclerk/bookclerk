//! The sans-IO driver: one parse pass per round over the bytes supplied so
//! far, until a pass finishes.

use std::collections::BTreeSet;

use crate::cache::{Miss, RangeCache, Source, Step};
use crate::{
    flac, mp4, mpeg, ByteSpan, TagError, TagRecord, MAX_FETCH_BYTES, MAX_ROUNDS,
    MAX_SPANS_PER_ROUND, MIN_FETCH_BYTES,
};

/// What [`TagReader::poll`] wants next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Supply each of these spans (in any order) with
    /// [`TagReader::supply`], then poll again. Every span lies inside the
    /// file; there are between one and [`MAX_SPANS_PER_ROUND`].
    Need(Vec<ByteSpan>),
    /// The reader is done; later polls return the same answer.
    Done(Result<TagRecord, TagError>),
}

/// Reads one file's tags, asking for the bytes it needs.
///
/// Each poll runs the parser from the start over the bytes supplied so far.
/// A pass that reaches bytes it does not have stops and asks for them (all
/// the ones it can name at once), so the number of rounds is about the
/// depth of the container, not the number of fields. Headers are fetched
/// in windows of at least [`MIN_FETCH_BYTES`].
#[derive(Debug)]
pub struct TagReader {
    /// The file's size in bytes.
    size: u64,
    /// The bytes supplied so far.
    cache: RangeCache,
    /// Spans asked for and not yet supplied.
    pending: BTreeSet<ByteSpan>,
    /// Rounds of [`Poll::Need`] so far.
    rounds: u32,
    /// Bytes supplied so far.
    fetched: u64,
    /// The answer, once there is one.
    done: Option<Result<TagRecord, TagError>>,
}

impl TagReader {
    /// A reader for a file of `size` bytes.
    #[must_use]
    pub fn new(size: u64) -> Self {
        Self {
            size,
            cache: RangeCache::default(),
            pending: BTreeSet::new(),
            rounds: 0,
            fetched: 0,
            done: None,
        }
    }

    /// Advances the reader.
    ///
    /// # Returns
    ///
    /// The spans to supply next, or the answer. While spans from the last
    /// [`Poll::Need`] are still outstanding it asks for those again.
    pub fn poll(&mut self) -> Poll {
        if let Some(done) = &self.done {
            return Poll::Done(done.clone());
        }
        if !self.pending.is_empty() {
            return Poll::Need(self.pending.iter().copied().collect());
        }
        let source = Source {
            cache: &self.cache,
            size: self.size,
        };
        match parse(&source) {
            Ok(record) => self.finish(Ok(record)),
            Err(Miss::Fail(err)) => self.finish(Err(err)),
            Err(Miss::Need(spans)) => {
                self.rounds += 1;
                if self.rounds > MAX_ROUNDS {
                    return self.finish(Err(TagError::TooLarge));
                }
                let spans = self.widen(spans);
                if spans.is_empty() {
                    // A parser asked for bytes it already has: a bug, not
                    // something to loop on.
                    return self.finish(Err(TagError::Malformed {
                        container: "file",
                        detail: "reader made no progress",
                    }));
                }
                let asked: u64 = spans.iter().map(|span| span.len).sum();
                if self.fetched.saturating_add(asked) > MAX_FETCH_BYTES {
                    return self.finish(Err(TagError::TooLarge));
                }
                self.pending = spans.iter().copied().collect();
                Poll::Need(spans)
            }
        }
    }

    /// Whether spans from the last [`Poll::Need`] are still outstanding, so
    /// polling now would only ask for them again.
    #[must_use]
    pub fn is_waiting(&self) -> bool {
        self.done.is_none() && !self.pending.is_empty()
    }

    /// Supplies the bytes of one span the last [`Poll::Need`] asked for.
    ///
    /// # Arguments
    ///
    /// * `offset` - The span's offset.
    /// * `bytes` - Exactly the span's bytes.
    ///
    /// # Errors
    ///
    /// [`TagError::Unrequested`] when no outstanding span starts at
    /// `offset` with that length; the reader is then done with that error.
    pub fn supply(&mut self, offset: u64, bytes: Vec<u8>) -> Result<(), TagError> {
        let span = u64::try_from(bytes.len())
            .ok()
            .map(|len| ByteSpan { offset, len });
        match span {
            Some(span) if self.done.is_none() && self.pending.remove(&span) => {
                self.fetched = self.fetched.saturating_add(span.len);
                self.cache.insert(offset, bytes);
                Ok(())
            }
            _ => {
                self.done = Some(Err(TagError::Unrequested));
                Err(TagError::Unrequested)
            }
        }
    }

    /// Stores and returns the answer.
    fn finish(&mut self, result: Result<TagRecord, TagError>) -> Poll {
        self.pending.clear();
        self.done = Some(result.clone());
        Poll::Done(result)
    }

    /// Turns a parser's request into the spans to ask for: each grown to
    /// [`MIN_FETCH_BYTES`] (within the file), overlapping and nearby ones
    /// joined, already-cached ones dropped, at most
    /// [`MAX_SPANS_PER_ROUND`] of them.
    fn widen(&self, spans: Vec<ByteSpan>) -> Vec<ByteSpan> {
        let mut grown: Vec<ByteSpan> = spans
            .into_iter()
            .filter(|span| self.cache.get(span.offset, span.len).is_none())
            .filter_map(|span| {
                let room = self.size.checked_sub(span.offset)?;
                let len = span.len.max(MIN_FETCH_BYTES).min(room);
                (len > 0).then_some(ByteSpan {
                    offset: span.offset,
                    len,
                })
            })
            .collect();
        grown.sort();
        let mut joined: Vec<ByteSpan> = Vec::with_capacity(grown.len());
        for span in grown {
            if let Some(last) = joined.last_mut() {
                let last_end = last.offset.saturating_add(last.len);
                if span.offset <= last_end {
                    let end = span.offset.saturating_add(span.len).max(last_end);
                    last.len = end - last.offset;
                    continue;
                }
            }
            joined.push(span);
        }
        joined.truncate(MAX_SPANS_PER_ROUND);
        joined
    }
}

/// Reads a whole in-memory file in one pass.
///
/// The reference the fuzz oracle compares [`TagReader`] against: the same
/// parser with every byte available up front.
///
/// # Errors
///
/// The [`TagError`] the reader would report.
pub fn read_all(file: &[u8]) -> Result<TagRecord, TagError> {
    let size = u64::try_from(file.len()).map_err(|_| TagError::TooLarge)?;
    let mut cache = RangeCache::default();
    cache.insert(0, file.to_vec());
    let source = Source {
        cache: &cache,
        size,
    };
    match parse(&source) {
        Ok(record) => Ok(record),
        Err(Miss::Fail(err)) => Err(err),
        // Unreachable with the whole file cached, short of a parser bug.
        Err(Miss::Need(_)) => Err(TagError::Malformed {
            container: "file",
            detail: "reader made no progress",
        }),
    }
}

/// One parse pass: sniff the container, then read it.
fn parse(src: &Source<'_>) -> Step<TagRecord> {
    let head = src.up_to(0, 12, "file")?;
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        mp4::read(src)
    } else if head.starts_with(b"fLaC") {
        flac::read(src)
    } else if head.starts_with(b"ID3") || mpeg::is_frame_sync(head) {
        mpeg::read(src)
    } else {
        Err(Miss::Fail(TagError::Unsupported))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Drives a reader over `file`, supplying exactly what it asks for.
    pub(crate) fn drive(file: &[u8]) -> Result<TagRecord, TagError> {
        let mut reader = TagReader::new(u64::try_from(file.len()).unwrap_or(u64::MAX));
        loop {
            match reader.poll() {
                Poll::Done(result) => return result,
                Poll::Need(spans) => {
                    for span in spans {
                        let start = usize::try_from(span.offset).unwrap_or(usize::MAX);
                        let end = usize::try_from(span.offset + span.len).unwrap_or(usize::MAX);
                        let bytes = file.get(start..end).map(<[u8]>::to_vec).unwrap_or_default();
                        if reader.supply(span.offset, bytes).is_err() {
                            return Err(TagError::Unrequested);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unknown_files_are_refused() {
        assert_eq!(drive(b"hello world, not audio"), Err(TagError::Unsupported));
        assert_eq!(drive(b""), Err(TagError::Unsupported));
        assert_eq!(read_all(b"nope"), Err(TagError::Unsupported));
    }

    #[test]
    fn unrequested_bytes_end_the_read() {
        let mut reader = TagReader::new(100);
        assert!(matches!(reader.poll(), Poll::Need(_)));
        assert_eq!(reader.supply(5, vec![0; 3]), Err(TagError::Unrequested));
        assert_eq!(reader.poll(), Poll::Done(Err(TagError::Unrequested)));
    }

    #[test]
    fn outstanding_spans_are_asked_again() {
        let mut reader = TagReader::new(10);
        let Poll::Need(first) = reader.poll() else {
            panic!("need");
        };
        assert_eq!(first, vec![ByteSpan { offset: 0, len: 10 }]);
        assert_eq!(reader.poll(), Poll::Need(first));
    }
}
