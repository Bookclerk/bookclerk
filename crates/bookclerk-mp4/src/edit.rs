//! Editing a `moov` that is already in memory: find a box, replace a child, and
//! keep every enclosing size field consistent.
//!
//! Rewriting a sample table means growing or shrinking a box that several
//! ancestors declare the size of, so a splice here is never a plain
//! `Vec::splice`: [`splice_replace`] walks back up the tree and patches each
//! ancestor's size by the same delta.

use crate::error::{Mp4Error, Result};

/// Container boxes this module will descend into while searching. Anything else
/// is treated as a leaf, so a search never wanders into sample payloads.
const CONTAINERS: &[&[u8; 4]] = &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"udta"];

/// Boxes that may hold a spliced child, with the offset where their children
/// begin. Sample entries carry a fixed `AudioSampleEntry` header first.
fn children_start(kind: &[u8], box_start: usize) -> Option<usize> {
    match kind {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"udta" | b"sinf" | b"schi" => {
            Some(box_start + 8)
        }
        // FullBox header, then a 4-byte reserved field before the children.
        b"meta" => Some(box_start + 12),
        // FullBox header + entry_count.
        b"stsd" => Some(box_start + 16),
        b"enca" | b"mp4a" | b"aavd" => Some(box_start + 36),
        _ => None,
    }
}

/// Find the first box of `fourcc` anywhere under the well-known containers.
///
/// Returns the byte range `[start, end)` of the whole box, header included.
///
/// # Arguments
///
/// * `buf` - Destination buffer resized to `size`.
/// * `fourcc` - Numeric `fourcc` value for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub fn find_box_range(buf: &[u8], fourcc: &[u8; 4]) -> Result<Option<(usize, usize)>> {
    let mut stack = vec![(0usize, buf.len())];
    while let Some((start, end)) = stack.pop() {
        let mut pos = start;
        while pos + 8 <= end {
            let size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            if size < 8 || pos + size > end {
                break;
            }
            let kind = &buf[pos + 4..pos + 8];
            let content_start = pos + 8;
            let box_end = pos + size;
            if kind == fourcc {
                return Ok(Some((pos, box_end)));
            }
            let kind: &[u8; 4] = kind.try_into().expect("four byte slice");
            if CONTAINERS.contains(&kind) {
                stack.push((content_start, box_end));
            } else if kind == b"meta" {
                stack.push((content_start + 4, box_end));
            }
            pos = box_end;
        }
    }
    Ok(None)
}

/// Find an immediate child of the box that starts at `parent_start`.
///
/// # Arguments
///
/// * `buf` - Destination buffer resized to `size`.
/// * `parent_start` - Numeric `parent_start` value for this call.
/// * `parent_end` - Numeric `parent_end` value for this call.
/// * `fourcc` - `fourcc` input for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn find_direct_child(
    buf: &[u8],
    parent_start: usize,
    parent_end: usize,
    fourcc: &[u8; 4],
) -> Result<Option<(usize, usize)>> {
    if parent_start + 8 > buf.len() {
        return Err(Mp4Error::container("parent box header truncated"));
    }
    let kind = &buf[parent_start + 4..parent_start + 8];
    let mut pos = parent_start + 8;
    if kind == b"meta" {
        pos += 4;
    }
    find_child_in_range(buf, pos, parent_end, fourcc)
}

/// Find a box of `fourcc` among the siblings laid out in `[start, end)`.
///
/// # Arguments
///
/// * `buf` - Destination buffer resized to `size`.
/// * `start` - Numeric `start` value for this call.
/// * `end` - Numeric `end` value for this call.
/// * `fourcc` - `fourcc` input for this call.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
pub fn find_child_in_range(
    buf: &[u8],
    start: usize,
    end: usize,
    fourcc: &[u8; 4],
) -> Result<Option<(usize, usize)>> {
    let mut pos = start;
    while pos + 8 <= end.min(buf.len()) {
        let size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > end {
            break;
        }
        let kind = &buf[pos + 4..pos + 8];
        let box_end = pos + size;
        if kind == fourcc {
            return Ok(Some((pos, box_end)));
        }
        pos = box_end;
    }
    Ok(None)
}

/// Replace `buf[start..end]` with `replacement`, fixing every ancestor's size.
///
/// # Arguments
///
/// * `buf` - Destination buffer resized to `size`.
/// * `start` - Numeric `start` value for this call.
/// * `end` - Numeric `end` value for this call.
/// * `replacement` - `replacement` input for this call.
///
/// # Returns
///
/// On success, the inner `Vec<u8>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn splice_replace(buf: &[u8], start: usize, end: usize, replacement: &[u8]) -> Result<Vec<u8>> {
    if start > end || end > buf.len() {
        return Err(Mp4Error::container(format!(
            "splice range {start}..{end} outside buffer of {}",
            buf.len()
        )));
    }
    let old_len = end - start;
    let new_len = replacement.len();
    let delta = new_len as i64 - old_len as i64;
    let ancestors = if delta == 0 {
        Vec::new()
    } else {
        ancestor_size_offsets(buf, start)?
    };
    let mut out = Vec::with_capacity(buf.len() - old_len + new_len);
    out.extend_from_slice(&buf[..start]);
    out.extend_from_slice(replacement);
    out.extend_from_slice(&buf[end..]);
    for (offset, old_size) in ancestors {
        let new_size = (old_size as i64 + delta) as u32;
        if offset + 4 <= out.len() {
            out[offset..offset + 4].copy_from_slice(&new_size.to_be_bytes());
        }
    }
    Ok(out)
}

/// Size-field offsets for every box that strictly contains `at`.
fn ancestor_size_offsets(buf: &[u8], at: usize) -> Result<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut end = buf.len();
    loop {
        let mut found = None;
        let mut child_pos = pos;
        while child_pos + 8 <= end {
            let size =
                u32::from_be_bytes(buf[child_pos..child_pos + 4].try_into().unwrap()) as usize;
            if size < 8 || child_pos + size > end {
                break;
            }
            let kind = &buf[child_pos + 4..child_pos + 8];
            let box_end = child_pos + size;
            if at > child_pos && at < box_end {
                out.push((child_pos, size));
                // A box with no known child layout still declares a size that
                // covers the splice, so it is patched — but the walk stops
                // rather than guess where its children begin.
                found = children_start(kind, child_pos).map(|content| (content, box_end));
                break;
            }
            child_pos = box_end;
        }
        let Some((content, box_end)) = found else {
            break;
        };
        pos = content;
        end = box_end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `moov` → `trak` → `stbl` → `stsz`, sizes big-endian in every header.
    fn nested_moov() -> Vec<u8> {
        fn wrap(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(8 + body.len());
            out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out
        }
        let stsz = wrap(b"stsz", &[1, 2, 3, 4]);
        let stbl = wrap(b"stbl", &stsz);
        let trak = wrap(b"trak", &stbl);
        wrap(b"moov", &trak)
    }

    fn box_size(buf: &[u8], at: usize) -> usize {
        u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize
    }

    #[test]
    fn a_splice_resizes_every_enclosing_box() {
        let moov = nested_moov();
        let (start, end) = find_box_range(&moov, b"stsz").unwrap().unwrap();
        assert_eq!(end - start, 12);

        // Replace the 4-byte body with an 8-byte one: every ancestor grows by 4.
        let mut bigger = Vec::new();
        bigger.extend_from_slice(&16u32.to_be_bytes());
        bigger.extend_from_slice(b"stsz");
        bigger.extend_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]);
        let out = splice_replace(&moov, start, end, &bigger).unwrap();

        assert_eq!(out.len(), moov.len() + 4);
        assert_eq!(box_size(&out, 0), out.len()); // moov
        assert_eq!(box_size(&out, 8), out.len() - 8); // trak
        assert_eq!(box_size(&out, 16), out.len() - 16); // stbl
        assert_eq!(box_size(&out, 24), 16); // stsz
    }

    #[test]
    fn removing_a_child_shrinks_its_ancestors() {
        let moov = nested_moov();
        let (start, end) = find_box_range(&moov, b"stsz").unwrap().unwrap();
        let out = splice_replace(&moov, start, end, &[]).unwrap();
        assert_eq!(out.len(), moov.len() - 12);
        assert_eq!(box_size(&out, 0), out.len());
        assert!(find_box_range(&out, b"stsz").unwrap().is_none());
    }

    #[test]
    fn a_direct_child_search_ignores_deeper_boxes() {
        let moov = nested_moov();
        let (moov_start, moov_end) = (0, moov.len());
        assert!(find_direct_child(&moov, moov_start, moov_end, b"trak")
            .unwrap()
            .is_some());
        // stbl is a grandchild, so a direct-child search must not find it.
        assert!(find_direct_child(&moov, moov_start, moov_end, b"stbl")
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_splice_outside_the_buffer_is_rejected() {
        let moov = nested_moov();
        let err = splice_replace(&moov, 4, moov.len() + 1, &[]).unwrap_err();
        assert!(matches!(err, Mp4Error::Container(_)), "{err}");
    }
}
