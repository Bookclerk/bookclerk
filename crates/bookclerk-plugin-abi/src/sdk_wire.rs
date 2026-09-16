//! Hand-packed Cap'n Proto layout matching the JS/Python plugin SDKs.
//!
//! The typed SDKs encode `ExecuteRequest` with a compact `(4, 3)` struct that
//! omits guest-receipt hints present on the full Cap'n `ExecuteRequest`. The
//! canonical idempotency digest must use that layout so Rust, TypeScript, and
//! Python agree across ABI bumps that extend the generated Cap'n schema.

#![allow(clippy::missing_docs_in_private_items)]

use crate::{
    DbPlanStatementKind, DbResultSelection, DbType, DbValue, ExecuteRequest, PluginError, Result,
    TypedDbStatement,
};

const WORD: usize = 8;

struct CapnpMessage {
    buf: Vec<u8>,
    used_words: usize,
}

impl CapnpMessage {
    fn new() -> Self {
        Self {
            buf: vec![0u8; 256],
            used_words: 1,
        }
    }

    fn alloc(&mut self, n_words: usize) -> usize {
        let off = self.used_words;
        self.used_words += n_words;
        let need = (self.used_words + 1) * WORD;
        if self.buf.len() < need {
            self.buf.resize(need, 0);
        }
        off
    }

    fn init_root(&mut self, data_words: usize, pointer_words: usize) -> usize {
        let off = self.alloc(data_words + pointer_words);
        self.write_struct_pointer(0, off, data_words, pointer_words);
        off
    }

    fn finish(self) -> Vec<u8> {
        let seg = &self.buf[..self.used_words * WORD];
        let mut out = Vec::with_capacity(WORD + seg.len());
        out.extend_from_slice(&(0u32).to_le_bytes());
        out.extend_from_slice(&(self.used_words as u32).to_le_bytes());
        out.extend_from_slice(seg);
        out
    }

    fn write_struct_pointer(
        &mut self,
        ptr_word: usize,
        target_word: usize,
        data_words: usize,
        pointer_words: usize,
    ) {
        let offset = target_word.wrapping_sub(ptr_word + 1) as u64;
        let word = offset << 2 | (data_words as u64) << 32 | (pointer_words as u64) << 48;
        self.set_word(ptr_word, word);
    }

    fn write_list_pointer(
        &mut self,
        ptr_word: usize,
        target_word: usize,
        element_size: u32,
        list_length: u32,
    ) {
        let offset = target_word.wrapping_sub(ptr_word + 1) as u64;
        let word = 1 | offset << 2 | (element_size as u64) << 32 | (list_length as u64) << 35;
        self.set_word(ptr_word, word);
    }

    fn init_struct_list(
        &mut self,
        ptr_word: usize,
        count: usize,
        data_words: usize,
        pointer_words: usize,
    ) -> Vec<usize> {
        if count == 0 {
            let tag_word = self.alloc(1);
            self.write_list_pointer(ptr_word, tag_word, 7, 0);
            let tag = (data_words as u64) << 32 | (pointer_words as u64) << 48;
            self.set_word(tag_word, tag);
            return Vec::new();
        }
        let elem_words = data_words + pointer_words;
        let payload_words = count * elem_words;
        let tag_word = self.alloc(1 + payload_words);
        self.write_list_pointer(ptr_word, tag_word, 7, payload_words as u32);
        let tag = (count as u64) << 2 | (data_words as u64) << 32 | (pointer_words as u64) << 48;
        self.set_word(tag_word, tag);
        (0..count).map(|i| tag_word + 1 + i * elem_words).collect()
    }

    fn pointer_word(&self, struct_word: usize, data_words: usize, pointer_index: usize) -> usize {
        struct_word + data_words + pointer_index
    }

    fn set_text(&mut self, ptr_word: usize, value: &str) {
        let mut encoded = value.as_bytes().to_vec();
        encoded.push(0);
        self.set_byte_list(ptr_word, &encoded);
    }

    fn set_byte_list(&mut self, ptr_word: usize, data: &[u8]) {
        if data.is_empty() {
            self.write_list_pointer(ptr_word, ptr_word + 1, 2, 0);
            return;
        }
        let n_words = data.len().div_ceil(WORD);
        let target = self.alloc(n_words);
        let start = target * WORD;
        self.buf[start..start + data.len()].copy_from_slice(data);
        self.write_list_pointer(ptr_word, target, 2, data.len() as u32);
    }

    fn set_u16(&mut self, word: usize, field_index: usize, value: u16) {
        let off = word * WORD + field_index * 2;
        self.buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn set_u32(&mut self, word: usize, field_index: usize, value: u32) {
        let off = word * WORD + field_index * 4;
        self.buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn set_i64(&mut self, word: usize, field_index: usize, value: i64) {
        let off = word * WORD + field_index * 8;
        self.buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn set_f64(&mut self, word: usize, field_index: usize, value: f64) {
        let off = word * WORD + field_index * 8;
        self.buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn set_bool(&mut self, word: usize, bit_index: usize, value: bool) {
        let byte_off = word * WORD + (bit_index >> 3);
        let mask = 1u8 << (bit_index & 7);
        if value {
            self.buf[byte_off] |= mask;
        } else {
            self.buf[byte_off] &= !mask;
        }
    }

    fn set_u64(&mut self, word: usize, field_index: usize, value: u64) {
        let off = word * WORD + field_index * 8;
        self.buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn set_word(&mut self, word: usize, value: u64) {
        let off = word * WORD;
        self.buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }
}

fn db_type_ord(ty: DbType) -> u16 {
    match ty {
        DbType::Unspecified => 0,
        DbType::Bool => 1,
        DbType::Int64 => 2,
        DbType::Float64 => 3,
        DbType::Text => 4,
        DbType::Bytes => 5,
    }
}

fn kind_ord(kind: DbPlanStatementKind) -> u16 {
    match kind {
        DbPlanStatementKind::Execute => 0,
        DbPlanStatementKind::Select => 1,
        DbPlanStatementKind::Returning => 2,
    }
}

fn selection_ord(sel: DbResultSelection) -> u16 {
    match sel {
        DbResultSelection::Discard => 0,
        DbResultSelection::AffectedRows => 1,
        DbResultSelection::Rows => 2,
    }
}

/// Writes one `DbValue` into a `(2, 1)` SDK struct slot.
///
/// # Errors
///
/// Returns when a float64 bind is not finite, or TEXT contains U+0000.
fn write_db_value(msg: &mut CapnpMessage, word: usize, value: &DbValue) -> Result<()> {
    match value {
        DbValue::Null(ty) => {
            msg.set_u16(word, 0, db_type_ord(*ty));
            msg.set_u16(word, 1, 0);
        }
        DbValue::Boolean(x) => {
            msg.set_bool(word, 0, *x);
            msg.set_u16(word, 1, 1);
        }
        DbValue::Int64(n) => {
            msg.set_i64(word, 1, *n);
            msg.set_u16(word, 1, 2);
        }
        DbValue::Float64(n) => {
            if !n.is_finite() {
                return Err(PluginError::invalid_params("float64 value is not finite"));
            }
            msg.set_f64(word, 1, *n);
            msg.set_u16(word, 1, 3);
        }
        DbValue::Text(s) => {
            crate::sql_text::require_portable_text(s)?;
            msg.set_u16(word, 1, 4);
            msg.set_text(msg.pointer_word(word, 2, 0), s);
        }
        DbValue::Bytes(d) => {
            msg.set_u16(word, 1, 5);
            msg.set_byte_list(msg.pointer_word(word, 2, 0), d);
        }
    }
    Ok(())
}

/// Writes one `TypedDbStatement` into a `(1, 2)` SDK struct slot.
///
/// # Errors
///
/// Returns when a parameter bind cannot be encoded.
fn write_statement(msg: &mut CapnpMessage, word: usize, stmt: &TypedDbStatement) -> Result<()> {
    const DATA_WORDS: usize = 1;
    crate::sql_text::require_portable_text(&stmt.sql)?;
    msg.set_text(msg.pointer_word(word, DATA_WORDS, 0), &stmt.sql);
    msg.set_u16(word, 0, kind_ord(stmt.kind));
    msg.set_u16(word, 1, selection_ord(stmt.result_selection));
    msg.set_u32(word, 1, stmt.max_rows);
    let param_words = msg.init_struct_list(
        msg.pointer_word(word, DATA_WORDS, 1),
        stmt.parameters.len(),
        2,
        1,
    );
    for (param_word, param) in param_words.into_iter().zip(stmt.parameters.iter()) {
        write_db_value(msg, param_word, param)?;
    }
    Ok(())
}

/// Encodes `ExecuteRequest` using the compact SDK Cap'n layout.
///
/// # Errors
///
/// Returns when `statements` is empty or a bind cannot be encoded.
pub fn encoded_execute_request_sdk_bytes(req: &ExecuteRequest) -> Result<Vec<u8>> {
    if req.statements.is_empty() {
        return Err(PluginError::invalid_params(
            "execute statements must be non-empty",
        ));
    }
    let mut msg = CapnpMessage::new();
    const ROOT_DATA: usize = 4;
    const ROOT_PTRS: usize = 3;
    let root = msg.init_root(ROOT_DATA, ROOT_PTRS);
    msg.set_text(msg.pointer_word(root, ROOT_DATA, 0), &req.operation_id);
    msg.set_text(msg.pointer_word(root, ROOT_DATA, 1), &req.request_hash);
    msg.set_u64(root, 3, req.deadline_unix_ms);
    let stmt_words = msg.init_struct_list(
        msg.pointer_word(root, ROOT_DATA, 2),
        req.statements.len(),
        1,
        2,
    );
    for (stmt_word, stmt) in stmt_words.into_iter().zip(req.statements.iter()) {
        write_statement(&mut msg, stmt_word, stmt)?;
    }
    Ok(msg.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbPlanStatementKind, DbResultSelection};

    /// Golden bytes must match `packages/plugin-sdk-python` `encode_execute_request`.
    ///
    /// # Panics
    ///
    /// Panics when the fixed golden request cannot be encoded.
    #[test]
    fn sdk_layout_matches_python_golden_bytes() {
        let req = ExecuteRequest {
            operation_id: String::new(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let bytes = encoded_execute_request_sdk_bytes(&req).expect("golden request encodes");
        const PYTHON_GOLDEN: &str = "000000001100000000000000040003000000000000000000000000000000000000000000000000000000000000000000090000000a000000090000000a000000090000001f0000000000000000000000000000000000000004000000010002000100020001000000050000004a000000090000000700000053454c454354203100000000000000000000000002000100";
        assert_eq!(hex::encode(bytes), PYTHON_GOLDEN);
    }
}
