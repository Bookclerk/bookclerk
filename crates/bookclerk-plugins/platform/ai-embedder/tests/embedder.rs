//! The model plugin on a tiny model, end to end: model directory on disk,
//! `ai` entrypoint, real ONNX Runtime.
//!
//! The model is the tiny one `support/tiny_model.rs` writes: its token
//! tables are a formula, so every vector the plugin returns is checked
//! against arithmetic done in the test, independently of the plugin's code.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#![allow(clippy::missing_docs_in_private_items)]

use std::path::{Path, PathBuf};

use bookclerk_model_pack::Pooling;
use bookclerk_plugin_ai_embedder::plugin::EmbedderRoot;
use bookclerk_plugin_sdk::ai::{check_models, check_run_output, embedding_vectors};
use bookclerk_plugin_sdk::{
    AiEmbedText, AiInput, AiModel, AiModels, AiRunOk, AiRunParams, AiTextEmbeddingsInput,
    AiTextRole, BindingValues, Bindings, Invocation, PluginErrorCode, PluginWorker,
};

#[path = "support/tiny_model.rs"]
mod tiny_model;
use tiny_model::{assert_close, expected, framed, in_base, tokenizer, write_model, BOUNDARY};

/// The pinned ONNX Runtime library, fetched into the workspace's `target/`
/// on first use. `None` on a platform ONNX Runtime has no release for;
/// those tests then have nothing to run on.
fn runtime_library() -> Option<PathBuf> {
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../target"),
        PathBuf::from,
    );
    let library =
        bookclerk_onnxruntime::ensure_in_target(&target).expect("fetch the pinned ONNX Runtime");
    if library.is_none() {
        eprintln!("skipped: no ONNX Runtime release for this platform");
    }
    library
}

async fn entrypoint(models_dir: &Path, library: Option<PathBuf>) -> Box<dyn AiModels> {
    let root = EmbedderRoot::new(Some(models_dir.to_path_buf()), library);
    let describe = root.describe().await.unwrap();
    assert_eq!(describe.id, "embedder");
    root.open(
        Invocation::default(),
        Bindings::from_values(BindingValues::default()),
    )
    .await
    .unwrap()
    .ai
    .expect("ai entrypoint")
}

fn input(texts: &[(&str, AiTextRole)]) -> AiInput {
    AiInput {
        text_embeddings: AiTextEmbeddingsInput {
            texts: texts
                .iter()
                .map(|(text, role)| AiEmbedText {
                    text: (*text).to_owned(),
                    role: *role,
                })
                .collect(),
        },
    }
}

fn documents(texts: &[&str]) -> AiInput {
    input(
        &texts
            .iter()
            .map(|text| (*text, AiTextRole::Document))
            .collect::<Vec<_>>(),
    )
}

/// Runs `model` and checks the reply the way the host does.
async fn run(ai: &dyn AiModels, model: &AiModel, input: AiInput) -> AiRunOk {
    let reply = ai
        .run(AiRunParams {
            model: model.model_id.clone(),
            input: input.clone(),
        })
        .await
        .unwrap();
    check_run_output(model, &input, &reply).expect("the host accepts the reply");
    reply
}

async fn only_model(ai: &dyn AiModels) -> AiModel {
    let list = ai.models().await.unwrap();
    check_models(&list).expect("the host accepts the listing");
    assert_eq!(list.models.len(), 1, "{list:?}");
    list.models.into_iter().next().unwrap()
}

const TEXTS: [&str; 3] = [
    "A detective in Victorian London",
    "Dragons guard a stolen ring beneath the northern mountains. The lighthouse keeper writes to a daughter he has never met.",
    "探偵が指輪を守っている。",
];

#[tokio::test(flavor = "multi_thread")]
async fn vectors_are_the_models_arithmetic_on_the_published_ids() {
    let Some(library) = runtime_library() else {
        return;
    };
    let models = tempfile::tempdir().unwrap();
    let dir = write_model(models.path(), "tiny", Pooling::Mean, "");
    dir.add_even();
    dir.add_odd();
    let ai = entrypoint(models.path(), Some(library)).await;

    let health = ai.health().await.unwrap();
    assert!(health.ok, "{}", health.detail);
    assert_eq!(health.detail, "1 model(s) installed");

    let model = only_model(ai.as_ref()).await;
    assert_eq!(model.model_id, "tiny");
    assert_eq!(model.dimensions, Some(8));
    assert_eq!(model.max_tokens, Some(32));
    assert_eq!(model.languages, ["even", "odd"]);

    let reply = run(ai.as_ref(), &model, documents(&TEXTS)).await;
    let output = &reply.output.text_embeddings;
    assert_eq!(output.revision, model.revision);
    assert!(output.fallbacks.is_empty());
    assert_eq!(reply.model, None);
    let vectors = embedding_vectors(output);
    for (vector, text) in vectors.iter().zip(TEXTS) {
        assert_close(vector, &expected(&framed(text)));
        let length: f32 = vector.iter().map(|n| n * n).sum::<f32>().sqrt();
        assert!((length - 1.0).abs() < 1e-5);
    }
    // The second text is longer than the model's 32 tokens: it was cut,
    // and the framing kept.
    assert_eq!(framed(TEXTS[1]).len(), 32);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graph_that_pools_itself_gives_the_same_vectors() {
    let Some(library) = runtime_library() else {
        return;
    };
    let models = tempfile::tempdir().unwrap();
    let dir = write_model(models.path(), "pooled", Pooling::None, "");
    dir.add_even();
    dir.add_odd();
    let ai = entrypoint(models.path(), Some(library)).await;
    let model = only_model(ai.as_ref()).await;
    let reply = run(ai.as_ref(), &model, documents(&TEXTS)).await;
    for (vector, text) in embedding_vectors(&reply.output.text_embeddings)
        .iter()
        .zip(TEXTS)
    {
        assert_close(vector, &expected(&framed(text)));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_gets_the_models_query_prefix() {
    let Some(library) = runtime_library() else {
        return;
    };
    let models = tempfile::tempdir().unwrap();
    let dir = write_model(models.path(), "tiny", Pooling::Mean, "Query: ");
    dir.add_even();
    dir.add_odd();
    let ai = entrypoint(models.path(), Some(library)).await;
    let model = only_model(ai.as_ref()).await;
    let reply = run(
        ai.as_ref(),
        &model,
        input(&[
            ("a stolen ring", AiTextRole::Query),
            ("a stolen ring", AiTextRole::Document),
        ]),
    )
    .await;
    let vectors = embedding_vectors(&reply.output.text_embeddings);
    assert_close(&vectors[0], &expected(&framed("Query: a stolen ring")));
    assert_close(&vectors[1], &expected(&framed("a stolen ring")));
}

/// What the plugin should run for `text` when only the tokens `present`
/// accepts have rows: missing tokens spelled character by character.
fn spelled(text: &str, present: impl Fn(u32) -> bool) -> (Vec<u32>, bool) {
    let tokenizer = tokenizer();
    let boundary = tokenizer.piece_to_id(BOUNDARY).unwrap();
    let mut ids = Vec::new();
    let mut fallback = false;
    for id in tokenizer.encode(text, 30).unwrap().ids {
        if present(id) {
            ids.push(id);
            continue;
        }
        fallback = true;
        for character in tokenizer.id_to_piece(id).unwrap().chars() {
            let single = character.to_string();
            ids.push(if single == BOUNDARY {
                boundary
            } else {
                tokenizer.piece_to_id(&single).unwrap_or(3)
            });
        }
    }
    ids.truncate(30);
    ids.insert(0, 2);
    ids.push(1);
    (ids, fallback)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_language_pack_is_spelled_out_reported_and_picked_up_when_added() {
    let Some(library) = runtime_library() else {
        return;
    };
    let models = tempfile::tempdir().unwrap();
    let dir = write_model(models.path(), "tiny", Pooling::Mean, "");
    dir.add_even();
    let ai = entrypoint(models.path(), Some(library)).await;
    let before = only_model(ai.as_ref()).await;
    assert_eq!(before.languages, ["even"]);

    // "é" is a single character: its row is in the base rows whatever
    // packs are loaded.
    let texts = ["Dragons guard a stolen ring", "é"];
    let tokenizer = tokenizer();
    let has_row = |id: u32| in_base(&tokenizer, id) || id.is_multiple_of(2);
    let reply = run(ai.as_ref(), &before, documents(&texts)).await;
    let output = &reply.output.text_embeddings;
    let vectors = embedding_vectors(output);
    let (ids, fallback) = spelled(texts[0], has_row);
    assert!(fallback, "the text needs a token of the missing pack");
    assert_close(&vectors[0], &expected(&ids));
    assert_close(&vectors[1], &expected(&framed(texts[1])));
    assert_eq!(output.fallbacks, [0]);

    // The host adds the pack: the listing shows it under a new revision,
    // and the next run loads the model again and needs no fallback.
    dir.add_odd();
    let after = only_model(ai.as_ref()).await;
    assert_eq!(after.languages, ["even", "odd"]);
    assert_ne!(after.revision, before.revision);
    let reply = run(ai.as_ref(), &after, documents(&texts)).await;
    let output = &reply.output.text_embeddings;
    assert!(output.fallbacks.is_empty());
    assert_close(&embedding_vectors(output)[0], &expected(&framed(texts[0])));

    // A pack of the wrong length is not a pack: the model goes back to the
    // revision without it.
    std::fs::write(dir.dir.join("odd.rows"), b"truncated").unwrap();
    assert_eq!(only_model(ai.as_ref()).await.revision, before.revision);
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_files_are_refused_and_broken_directories_are_not_listed() {
    let Some(library) = runtime_library() else {
        return;
    };
    let models = tempfile::tempdir().unwrap();
    let dir = write_model(models.path(), "tiny", Pooling::Mean, "");
    let ai = entrypoint(models.path(), Some(library)).await;
    let model = only_model(ai.as_ref()).await;
    let params = || AiRunParams {
        model: "tiny".into(),
        input: documents(&["a ring"]),
    };

    // Same length, one byte changed: the SHA-256 in the description no
    // longer matches, so the model does not load.
    let mut rows = std::fs::read(dir.dir.join("base.rows")).unwrap();
    *rows.last_mut().unwrap() ^= 1;
    std::fs::write(dir.dir.join("base.rows"), &rows).unwrap();
    let err = ai.run(params()).await.unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unavailable, "{err:?}");
    assert!(err.message.contains("base.rows"), "{err:?}");
    *rows.last_mut().unwrap() ^= 1;
    std::fs::write(dir.dir.join("base.rows"), &rows).unwrap();
    run(ai.as_ref(), &model, documents(&["a ring"])).await;

    // Models this plugin does not have, and names that are not model ids.
    for missing in ["absent", "../tiny", "", "Tiny"] {
        let err = ai
            .run(AiRunParams {
                model: missing.into(),
                input: documents(&["a ring"]),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::NotFound, "{missing:?}: {err:?}");
    }
    // Input of no task this plugin serves.
    let err = ai
        .run(AiRunParams {
            model: "tiny".into(),
            input: AiInput::default(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::InvalidParams);

    // A directory named differently from its model, one without a
    // description and one missing a file are skipped and explained.
    write_model(models.path(), "tiny", Pooling::Mean, "");
    std::fs::rename(models.path().join("tiny"), models.path().join("renamed")).unwrap();
    std::fs::create_dir(models.path().join("empty")).unwrap();
    let partial = write_model(models.path(), "partial", Pooling::Mean, "");
    std::fs::remove_file(partial.dir.join("core.onnx")).unwrap();
    std::fs::create_dir(models.path().join("Not-A-Model-Id")).unwrap();
    assert!(ai.models().await.unwrap().models.is_empty());
    let health = ai.health().await.unwrap();
    assert!(health.ok);
    assert!(
        health.detail.starts_with("0 model(s) installed; "),
        "{}",
        health.detail
    );
    assert!(health
        .detail
        .contains("renamed: the directory is named `renamed` but describes model `tiny`"));
    assert!(health.detail.contains("empty: cannot read `model.json`"));
    assert!(health
        .detail
        .contains("partial: `core.onnx` is missing or incomplete"));
    assert!(!health.detail.contains("Not-A-Model-Id"));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_files_the_plugin_lists_nothing_and_says_why() {
    let models = tempfile::tempdir().unwrap();
    // No models directory yet, and no runtime library beside the plugin.
    let ai = entrypoint(&models.path().join("models"), None).await;
    assert!(ai.models().await.unwrap().models.is_empty());
    let health = ai.health().await.unwrap();
    assert!(!health.ok);
    assert_eq!(
        health.detail,
        "the ONNX Runtime library is missing beside the plugin"
    );

    // A model is listed without the library, since listing loads nothing,
    // but it cannot run.
    write_model(&models.path().join("models"), "tiny", Pooling::Mean, "");
    assert_eq!(ai.models().await.unwrap().models.len(), 1);
    let err = ai
        .run(AiRunParams {
            model: "tiny".into(),
            input: documents(&["a ring"]),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unavailable);

    // A plugin started without a data directory has no models at all.
    let root = EmbedderRoot::new(None, None);
    let ai = root
        .open(
            Invocation::default(),
            Bindings::from_values(BindingValues::default()),
        )
        .await
        .unwrap()
        .ai
        .unwrap();
    assert!(ai.models().await.unwrap().models.is_empty());
    let err = ai
        .run(AiRunParams {
            model: "tiny".into(),
            input: documents(&["a ring"]),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::NotFound);
}

#[test]
fn the_library_name_is_the_one_the_fetcher_installs() {
    assert_eq!(
        bookclerk_plugin_ai_embedder::RUNTIME_LIBRARY,
        bookclerk_onnxruntime::LIBRARY_FILE_NAME
    );
}
