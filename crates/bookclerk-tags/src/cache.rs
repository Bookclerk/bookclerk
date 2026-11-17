//! The bytes a reader has been given so far, and the view parsers read
//! through.

use crate::{ByteSpan, TagError};

/// Disjoint, sorted runs of file bytes; touching runs are merged.
#[derive(Debug, Default)]
pub(crate) struct RangeCache {
    /// `(offset, bytes)` runs, sorted by offset, never overlapping or touching.
    runs: Vec<(u64, Vec<u8>)>,
}

impl RangeCache {
    /// The cached bytes `[offset, offset + len)`, when one run holds them all.
    pub(crate) fn get(&self, offset: u64, len: u64) -> Option<&[u8]> {
        if len == 0 {
            return Some(&[]);
        }
        let end = offset.checked_add(len)?;
        let index = match self.runs.binary_search_by(|(start, _)| start.cmp(&offset)) {
            Ok(index) => index,
            Err(0) => return None,
            Err(index) => index - 1,
        };
        let (start, bytes) = self.runs.get(index)?;
        let run_end = start.checked_add(u64::try_from(bytes.len()).ok()?)?;
        if end > run_end {
            return None;
        }
        let from = usize::try_from(offset - start).ok()?;
        let to = usize::try_from(end - start).ok()?;
        bytes.get(from..to)
    }

    /// Adds `bytes` at `offset`, merging with every run it overlaps or touches.
    pub(crate) fn insert(&mut self, offset: u64, bytes: Vec<u8>) {
        let Some(end) = u64::try_from(bytes.len())
            .ok()
            .and_then(|len| offset.checked_add(len))
        else {
            return;
        };
        let mut merged_start = offset;
        let mut merged_end = end;
        let mut keep = Vec::with_capacity(self.runs.len() + 1);
        let mut absorbed = Vec::new();
        for (start, run) in std::mem::take(&mut self.runs) {
            let run_end = start.saturating_add(u64::try_from(run.len()).unwrap_or(u64::MAX));
            if run_end < offset || start > end {
                keep.push((start, run));
            } else {
                merged_start = merged_start.min(start);
                merged_end = merged_end.max(run_end);
                absorbed.push((start, run));
            }
        }
        let Ok(total) = usize::try_from(merged_end - merged_start) else {
            self.runs = keep;
            return;
        };
        let mut merged = vec![0u8; total];
        absorbed.push((offset, bytes));
        for (start, run) in absorbed {
            if let Ok(at) = usize::try_from(start - merged_start) {
                if let Some(slot) = merged.get_mut(at..at + run.len()) {
                    slot.copy_from_slice(&run);
                }
            }
        }
        keep.push((merged_start, merged));
        keep.sort_by_key(|(start, _)| *start);
        self.runs = keep;
    }
}

/// Why a parse pass stopped early.
#[derive(Debug)]
pub(crate) enum Miss {
    /// These bytes are not cached yet.
    Need(Vec<ByteSpan>),
    /// The file cannot be read.
    Fail(TagError),
}

impl From<TagError> for Miss {
    fn from(err: TagError) -> Self {
        Self::Fail(err)
    }
}

/// A parse pass's result.
pub(crate) type Step<T> = Result<T, Miss>;

/// What a parser reads through: the cache plus the file's size.
#[derive(Clone, Copy)]
pub(crate) struct Source<'a> {
    /// The bytes supplied so far.
    pub(crate) cache: &'a RangeCache,
    /// The file's size in bytes.
    pub(crate) size: u64,
}

impl<'a> Source<'a> {
    /// The bytes `[offset, offset + len)`.
    ///
    /// # Errors
    ///
    /// [`Miss::Need`] when they are not cached yet; a malformed-file error
    /// for `container` when they run past the end of the file.
    pub(crate) fn bytes(&self, offset: u64, len: u64, container: &'static str) -> Step<&'a [u8]> {
        let end = offset.checked_add(len).ok_or(TagError::Malformed {
            container,
            detail: "range overflows",
        })?;
        if end > self.size {
            return Err(Miss::Fail(TagError::Malformed {
                container,
                detail: "structure runs past the end of the file",
            }));
        }
        if len > crate::MAX_FETCH_BYTES {
            return Err(Miss::Fail(TagError::TooLarge));
        }
        self.cache
            .get(offset, len)
            .ok_or_else(|| Miss::Need(vec![ByteSpan { offset, len }]))
    }

    /// Up to `len` bytes from `offset`, fewer at the end of the file.
    ///
    /// # Errors
    ///
    /// As [`Self::bytes`].
    pub(crate) fn up_to(&self, offset: u64, len: u64, container: &'static str) -> Step<&'a [u8]> {
        let len = len.min(self.size.saturating_sub(offset));
        self.bytes(offset, len, container)
    }

    /// Fetches every span in `spans` at once.
    ///
    /// # Returns
    ///
    /// The bytes of each span, in order.
    ///
    /// # Errors
    ///
    /// [`Miss::Need`] naming every span not cached yet, so they come in one
    /// round; a malformed-file error when one runs past the end.
    pub(crate) fn many(&self, spans: &[ByteSpan], container: &'static str) -> Step<Vec<&'a [u8]>> {
        let mut out = Vec::with_capacity(spans.len());
        let mut missing = Vec::new();
        for span in spans {
            match self.bytes(span.offset, span.len, container) {
                Ok(bytes) => out.push(bytes),
                Err(Miss::Need(need)) => missing.extend(need),
                Err(fail) => return Err(fail),
            }
        }
        if missing.is_empty() {
            Ok(out)
        } else {
            Err(Miss::Need(missing))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_merges_overlapping_and_touching_runs() {
        let mut cache = RangeCache::default();
        cache.insert(10, vec![1, 2, 3]);
        cache.insert(20, vec![9]);
        assert_eq!(cache.get(10, 3), Some(&[1, 2, 3][..]));
        assert_eq!(cache.get(9, 2), None);
        assert_eq!(cache.get(12, 2), None);
        cache.insert(13, vec![4, 5]);
        assert_eq!(cache.get(11, 4), Some(&[2, 3, 4, 5][..]));
        cache.insert(14, vec![7, 6, 6, 6, 6, 6, 6]);
        assert_eq!(
            cache.get(10, 11),
            Some(&[1, 2, 3, 4, 7, 6, 6, 6, 6, 6, 6][..])
        );
        assert_eq!(cache.runs.len(), 1);
        assert_eq!(cache.get(0, 0), Some(&[][..]));
        assert_eq!(cache.get(u64::MAX, 2), None);
    }
}
