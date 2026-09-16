//! Coverage-guided lexer / packer / grammar / TEXT admission.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 2048 {
        return;
    }
    let Ok(sql) = std::str::from_utf8(data) else {
        return;
    };
    let _ = bookclerk_plugin_abi::sql_v1_pack_statements(sql);
    let _ = bookclerk_plugin_abi::validate_sql_v1_grammar(sql, false);
    let _ = bookclerk_plugin_abi::validate_sql_v1_grammar(sql, true);
    let _ = bookclerk_plugin_abi::require_portable_text(sql);
    let _ = bookclerk_plugin_abi::desugar_canonical_sql(sql);
    let _ = bookclerk_plugin_abi::like_pattern_sources(sql);
    let _ = bookclerk_plugin_abi::require_like_patterns_within(
        sql,
        &[],
        bookclerk_plugin_abi::D1_PORTABLE_LIKE_PATTERN_BYTES,
    );
    let _ = bookclerk_plugin_abi::require_function_args_within(sql, 32);
});
