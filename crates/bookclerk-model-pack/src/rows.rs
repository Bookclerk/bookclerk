//! Row packs: the rows of a model's token tables for a set of token ids.
//!
//! A row pack is one little-endian file:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 8 | [`ROWS_MAGIC`] |
//! | 4 | `count`: rows in the pack, at most [`MAX_ROWS`] |
//! | 4 | `row_bytes`: bytes per row, 1 to [`MAX_ROW_BYTES`] |
//! | 4 × `count` | token ids, strictly ascending |
//! | `row_bytes` × `count` | rows, in the order of the ids |
//!
//! One row is the token's row of every table the model description lists,
//! concatenated in that order (see [`crate::PackManifest::row_bytes`]).

use crate::PackError;

/// First eight bytes of every row pack.
pub const ROWS_MAGIC: [u8; 8] = *b"BCROWS1\0";

/// Bytes before the token ids: the magic and the two counts.
pub const ROWS_HEADER_BYTES: usize = 16;

/// Most rows one pack holds. Vocabularies stay well under this (bekko-a8m
/// has 256,000 tokens).
pub const MAX_ROWS: usize = 4_000_000;

/// Most bytes one row has (bekko-a8m's is 388, Harrier's 410).
pub const MAX_ROW_BYTES: usize = 64 * 1024;

/// Bytes one token id takes in a pack.
const ID_BYTES: usize = 4;

/// A checked row pack held in memory.
///
/// Built by [`Rows::parse`]; afterwards every lookup is a binary search
/// over the id column and cannot fail on the pack's content.
#[derive(Clone, PartialEq, Eq)]
pub struct Rows {
    /// The whole file. Checked: header, length and ascending ids.
    bytes: Vec<u8>,
    /// Rows in the pack.
    count: usize,
    /// Bytes per row; never zero.
    row_bytes: usize,
}

impl std::fmt::Debug for Rows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rows")
            .field("count", &self.count)
            .field("row_bytes", &self.row_bytes)
            .finish_non_exhaustive()
    }
}

/// Reads the little-endian `u32` at `offset`, or `None` past the end.
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(ID_BYTES)?;
    let field: [u8; ID_BYTES] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(field))
}

impl Rows {
    /// Checks a row pack and takes ownership of it.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The whole file.
    /// * `row_bytes` - Bytes per row the model's tables add up to.
    ///
    /// # Errors
    ///
    /// The first rule of the layout the file breaks; see [`PackError`].
    pub fn parse(bytes: Vec<u8>, row_bytes: usize) -> Result<Self, PackError> {
        if bytes.len() < ROWS_HEADER_BYTES {
            return Err(PackError::RowsTruncated);
        }
        if !bytes.starts_with(&ROWS_MAGIC) {
            return Err(PackError::RowsMagic);
        }
        let count = u32_at(&bytes, 8)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(PackError::RowsTruncated)?;
        let found = u32_at(&bytes, 12)
            .and_then(|width| usize::try_from(width).ok())
            .ok_or(PackError::RowsTruncated)?;
        if count > MAX_ROWS || found == 0 || found > MAX_ROW_BYTES {
            return Err(PackError::RowsTooLarge);
        }
        if found != row_bytes {
            return Err(PackError::RowsWidth {
                expected: row_bytes,
                found,
            });
        }
        // Within the limits above this cannot overflow a 64-bit length, and
        // a 32-bit one is caught by the checked arithmetic.
        let expected_len = ID_BYTES
            .checked_add(found)
            .and_then(|per_row| per_row.checked_mul(count))
            .and_then(|body| body.checked_add(ROWS_HEADER_BYTES))
            .ok_or(PackError::RowsTooLarge)?;
        if bytes.len() != expected_len {
            return Err(PackError::RowsLength);
        }
        let pack = Self {
            bytes,
            count,
            row_bytes: found,
        };
        let mut previous: Option<u32> = None;
        for index in 0..count {
            let id = pack.id_at(index).ok_or(PackError::RowsLength)?;
            if previous.is_some_and(|before| before >= id) {
                return Err(PackError::RowsOrder);
            }
            previous = Some(id);
        }
        Ok(pack)
    }

    /// Rows in the pack.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the pack has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Bytes per row.
    #[must_use]
    pub fn row_bytes(&self) -> usize {
        self.row_bytes
    }

    /// The token id of the row at position `index`, or `None` past the end.
    fn id_at(&self, index: usize) -> Option<u32> {
        if index >= self.count {
            return None;
        }
        u32_at(
            &self.bytes,
            ROWS_HEADER_BYTES.checked_add(index.checked_mul(ID_BYTES)?)?,
        )
    }

    /// The token ids in the pack, ascending.
    pub fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.count).filter_map(|index| self.id_at(index))
    }

    /// The row of token `id`.
    ///
    /// # Arguments
    ///
    /// * `id` - A token id of the model's vocabulary.
    ///
    /// # Returns
    ///
    /// The row's [`Rows::row_bytes`] bytes, or `None` when the pack does not
    /// hold the token.
    #[must_use]
    pub fn row(&self, id: u32) -> Option<&[u8]> {
        let (mut low, mut high) = (0usize, self.count);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.id_at(middle)?.cmp(&id) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let rows_start = ROWS_HEADER_BYTES + self.count * ID_BYTES;
                    let start = rows_start + middle * self.row_bytes;
                    return self.bytes.get(start..start + self.row_bytes);
                }
            }
        }
        None
    }
}

/// Writes a row pack.
///
/// # Arguments
///
/// * `row_bytes` - Bytes per row.
/// * `rows` - Token id and row, in strictly ascending id order.
///
/// # Returns
///
/// The file, which [`Rows::parse`] accepts.
///
/// # Errors
///
/// [`PackError::RowsTooLarge`] over the limits, [`PackError::RowsOrder`]
/// when the ids do not ascend, and [`PackError::RowsWidth`] for a row of
/// another length.
pub fn encode_rows(row_bytes: usize, rows: &[(u32, &[u8])]) -> Result<Vec<u8>, PackError> {
    if rows.len() > MAX_ROWS || row_bytes == 0 || row_bytes > MAX_ROW_BYTES {
        return Err(PackError::RowsTooLarge);
    }
    let count = u32::try_from(rows.len()).map_err(|_| PackError::RowsTooLarge)?;
    let width = u32::try_from(row_bytes).map_err(|_| PackError::RowsTooLarge)?;
    let mut out =
        Vec::with_capacity(ROWS_HEADER_BYTES + rows.len() * (ID_BYTES.saturating_add(row_bytes)));
    out.extend_from_slice(&ROWS_MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    let mut previous: Option<u32> = None;
    for (id, row) in rows {
        if previous.is_some_and(|before| before >= *id) {
            return Err(PackError::RowsOrder);
        }
        if row.len() != row_bytes {
            return Err(PackError::RowsWidth {
                expected: row_bytes,
                found: row.len(),
            });
        }
        previous = Some(*id);
        out.extend_from_slice(&id.to_le_bytes());
    }
    for (_, row) in rows {
        out.extend_from_slice(row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-row pack of two-byte rows.
    fn sample() -> Vec<u8> {
        encode_rows(
            2,
            &[
                (3, &[30, 31][..]),
                (7, &[70, 71][..]),
                (900_000, &[9, 0][..]),
            ],
        )
        .expect("encode")
    }

    #[test]
    fn a_written_pack_reads_back_row_for_row() {
        let pack = Rows::parse(sample(), 2).expect("parse");
        assert_eq!(pack.len(), 3);
        assert!(!pack.is_empty());
        assert_eq!(pack.row_bytes(), 2);
        assert_eq!(pack.ids().collect::<Vec<_>>(), [3, 7, 900_000]);
        assert_eq!(pack.row(3), Some(&[30, 31][..]));
        assert_eq!(pack.row(7), Some(&[70, 71][..]));
        assert_eq!(pack.row(900_000), Some(&[9, 0][..]));
        for missing in [0, 4, 8, u32::MAX] {
            assert_eq!(pack.row(missing), None);
        }
        assert!(format!("{pack:?}").contains("count: 3"));
    }

    #[test]
    fn an_empty_pack_is_valid_and_holds_nothing() {
        let pack = Rows::parse(encode_rows(5, &[]).expect("encode"), 5).expect("parse");
        assert!(pack.is_empty());
        assert_eq!(pack.row(0), None);
    }

    #[test]
    fn every_broken_layout_rule_is_refused() {
        assert_eq!(
            Rows::parse(vec![0; 15], 2).unwrap_err(),
            PackError::RowsTruncated
        );
        let mut bad_magic = sample();
        bad_magic[0] = b'X';
        assert_eq!(Rows::parse(bad_magic, 2).unwrap_err(), PackError::RowsMagic);
        assert_eq!(
            Rows::parse(sample(), 3).unwrap_err(),
            PackError::RowsWidth {
                expected: 3,
                found: 2
            }
        );
        let mut short = sample();
        short.pop();
        assert_eq!(Rows::parse(short, 2).unwrap_err(), PackError::RowsLength);
        let mut long = sample();
        long.push(0);
        assert_eq!(Rows::parse(long, 2).unwrap_err(), PackError::RowsLength);
        let mut unordered = sample();
        unordered[ROWS_HEADER_BYTES..ROWS_HEADER_BYTES + 4].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(Rows::parse(unordered, 2).unwrap_err(), PackError::RowsOrder);
        let mut zero_width = sample();
        zero_width[12..16].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            Rows::parse(zero_width, 0).unwrap_err(),
            PackError::RowsTooLarge
        );
        let mut huge = sample();
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Rows::parse(huge, 2).unwrap_err(), PackError::RowsTooLarge);
    }

    #[test]
    fn the_writer_refuses_what_the_reader_would() {
        assert_eq!(encode_rows(0, &[]).unwrap_err(), PackError::RowsTooLarge);
        assert_eq!(
            encode_rows(MAX_ROW_BYTES + 1, &[]).unwrap_err(),
            PackError::RowsTooLarge
        );
        assert_eq!(
            encode_rows(1, &[(2, &[0][..]), (2, &[0][..])]).unwrap_err(),
            PackError::RowsOrder
        );
        assert_eq!(
            encode_rows(2, &[(2, &[0][..])]).unwrap_err(),
            PackError::RowsWidth {
                expected: 2,
                found: 1
            }
        );
    }
}
