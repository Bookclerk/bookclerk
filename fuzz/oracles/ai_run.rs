//! Shared oracle for the `ai_run` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A run is what a plugin sends on its `AI` binding. Byte 0 modulo 2 picks
//! the form the rest is in: the JSON body the workerd bridge's `/ai/run`
//! route reads (0), or an unpacked `AiRunParams` message, the form a native
//! guest's run crosses the plugin connection in (1). The invariants:
//!
//! - decoding and checking never panic;
//! - [`check_binding_run_params`] accepts the run exactly when the
//!   independent restatement below finds a model name of at most 192 bytes
//!   that is `@<plugin id>/<model id>` or `@library/text-embeddings`, and
//!   1 to 64 texts, none empty and none over 16 KiB;
//! - an accepted name prints back as the caller wrote it, and its model id
//!   with the same input passes [`check_entrypoint_run_params`], the check
//!   the host runs before the model plugin sees the run;
//! - an accepted run round-trips through the codec and is accepted again.

use bookclerk_plugin_abi::ai::{
    check_binding_run_params, check_entrypoint_run_params, decode_ai_run_params_bytes,
    encode_ai_run_params_bytes, AiModelName,
};
use bookclerk_plugin_abi::{AiRunParams, AiTask};

/// Largest input worth decoding; the bridge caps a run body at 2 MiB.
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;

/// Model name cap in bytes (`maxAiModelNameBytes`).
const NAME: usize = 192;
/// Text count cap (`maxAiEmbedTexts`).
const TEXTS: usize = 64;
/// Text cap in bytes (`maxAiEmbedTextBytes`).
const TEXT: usize = 16_384;

/// Runs the invariants on one input.
///
/// # Panics
///
/// When an invariant breaks.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((selector, rest)) = data.split_first() else {
        return;
    };
    let params: AiRunParams = if selector % 2 == 0 {
        match serde_json::from_slice(rest) {
            Ok(params) => params,
            Err(_) => return,
        }
    } else {
        match decode_ai_run_params_bytes(rest) {
            Ok(params) => params,
            Err(_) => return,
        }
    };
    let checked = check_binding_run_params(&params);
    assert_eq!(checked.is_ok(), expected(&params), "run {params:?}");
    let Ok((name, task)) = checked else {
        return;
    };
    assert_eq!(name.to_string(), params.model, "name prints back");
    match (&name, task) {
        (AiModelName::Alias(AiTask::TextEmbeddings), AiTask::TextEmbeddings) => {}
        (AiModelName::Model { model, .. }, AiTask::TextEmbeddings) => {
            let forwarded = AiRunParams {
                model: model.clone(),
                input: params.input.clone(),
            };
            assert_eq!(
                check_entrypoint_run_params(&forwarded),
                Ok(task),
                "the forwarded run is refused"
            );
        }
    }
    let bytes = encode_ai_run_params_bytes(&params)
        .unwrap_or_else(|err| panic!("encode an accepted run: {err}"));
    let again = decode_ai_run_params_bytes(&bytes)
        .unwrap_or_else(|err| panic!("decode an encoded run: {err}"));
    assert_eq!(again, params);
    assert!(
        check_binding_run_params(&again).is_ok(),
        "round trip refused"
    );
}

/// Whether `id` is a plugin id, restated over characters: 2 to 32 lowercase
/// letters, digits and single inner underscores.
fn plugin_id(id: &str) -> bool {
    let chars: Vec<char> = id.chars().collect();
    (2..=32).contains(&chars.len())
        && chars
            .iter()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
        && chars.first() != Some(&'_')
        && chars.last() != Some(&'_')
        && chars.windows(2).all(|pair| pair != ['_', '_'])
}

/// Whether `id` is a model id, restated over characters.
fn model_id(id: &str) -> bool {
    let mut chars = id.chars();
    let head = chars
        .next()
        .is_some_and(|c| matches!(c, 'a'..='z' | '0'..='9'));
    head && id.len() <= NAME - 34
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-'))
}

/// The independent restatement of every run rule.
fn expected(params: &AiRunParams) -> bool {
    let name_ok = params.model.len() <= NAME
        && params.model.strip_prefix('@').is_some_and(|rest| {
            let slash = rest.find('/');
            slash.is_some_and(|at| {
                let (plugin, model) = (&rest[..at], &rest[at + 1..]);
                if plugin == "library" {
                    model == "text-embeddings"
                } else {
                    plugin_id(plugin) && model_id(model)
                }
            })
        });
    let texts = &params.input.text_embeddings.texts;
    let input_ok = !texts.is_empty()
        && texts.len() <= TEXTS
        && texts
            .iter()
            .all(|item| !item.text.is_empty() && item.text.len() <= TEXT);
    name_ok && input_ok
}
