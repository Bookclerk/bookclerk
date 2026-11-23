//! The formatter corpus: every first-party `plugin.toml`, formatted by the
//! Rust formatter, committed under
//! `crates/bookclerk-plugin-abi/fixtures/tools/fmt-corpus/`.
//!
//! The TypeScript and Python SDK formatters compare their output for each
//! source against the same files, so all three agree on every table a real
//! manifest uses. `UPDATE_FMT_CORPUS=1 cargo test -p bookclerk-plugin-manifest
//! --test fmt_corpus` rewrites the corpus; without it the test fails when a
//! file is stale, missing or left over.

// Test code: expect and panics on a broken tree are the failure report.
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bookclerk_plugin_manifest::{format_manifest, PluginManifest};

/// Repository root, two levels above this crate.
///
/// # Panics
///
/// When the root cannot be resolved.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Directories whose `plugin.toml` files (at any depth) join the corpus.
const SOURCE_ROOTS: &[&str] = &["crates/bookclerk-plugins", "examples"];

/// Path segments a source is never read from (build output, installed
/// packages, test fixtures that are invalid on purpose).
const SKIPPED_SEGMENTS: &[&str] = &["target", "node_modules", "fixtures", ".venv"];

/// Collects every `plugin.toml` under `dir`, depth first, into `out`.
///
/// # Arguments
///
/// * `dir` - Directory to walk.
/// * `out` - Paths found so far.
///
/// # Panics
///
/// When a directory under `dir` cannot be read.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read source dir")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if path.is_dir() {
            if !SKIPPED_SEGMENTS.contains(&name.as_str()) {
                collect(&path, out);
            }
        } else if name == "plugin.toml" {
            out.push(path);
        }
    }
}

/// Corpus file name for a source: its directory relative to the repo root,
/// with `/` as `__`.
///
/// # Arguments
///
/// * `root` - Repository root.
/// * `source` - A `plugin.toml` path under `root`.
///
/// # Returns
///
/// `<slug>.fmt.toml`.
///
/// # Panics
///
/// When `source` has no parent or is not under `root`.
fn corpus_name(root: &Path, source: &Path) -> String {
    let dir = source
        .parent()
        .expect("plugin.toml has a parent")
        .strip_prefix(root)
        .expect("source under the repo root");
    let slug = dir
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("__");
    format!("{slug}.fmt.toml")
}

/// Every first-party `plugin.toml` formats to its committed corpus file,
/// and the index lists exactly those files (`UPDATE_FMT_CORPUS=1` rewrites
/// them instead).
///
/// # Panics
///
/// When a source fails to parse or format, or the corpus is stale.
#[test]
fn fmt_corpus_is_current() {
    let root = repo_root();
    let corpus = root.join("crates/bookclerk-plugin-abi/fixtures/tools/fmt-corpus");
    let update = std::env::var_os("UPDATE_FMT_CORPUS").is_some();
    let mut sources = Vec::new();
    for dir in SOURCE_ROOTS {
        collect(&root.join(dir), &mut sources);
    }
    assert!(!sources.is_empty(), "no first-party plugin.toml found");

    let mut index = BTreeMap::new();
    let mut stale = Vec::new();
    for source in &sources {
        let text = std::fs::read_to_string(source).expect("read plugin.toml");
        let manifest = PluginManifest::parse(&text)
            .unwrap_or_else(|err| panic!("{}: {err}", source.display()));
        let formatted = format_manifest(&manifest).expect("format manifest");
        let again = PluginManifest::parse(&formatted).expect("formatted output parses");
        assert_eq!(
            manifest,
            again,
            "{}: formatting changed the manifest",
            source.display()
        );
        let name = corpus_name(&root, source);
        let rel = source
            .strip_prefix(&root)
            .expect("under root")
            .to_string_lossy()
            .replace('\\', "/");
        index.insert(name.clone(), rel);
        let gold = corpus.join(&name);
        if std::fs::read_to_string(&gold).ok().as_deref() != Some(formatted.as_str()) {
            if update {
                std::fs::create_dir_all(&corpus).expect("create corpus dir");
                std::fs::write(&gold, &formatted).expect("write corpus file");
            } else {
                stale.push(name);
            }
        }
    }

    let mut index_json = serde_json::to_string_pretty(&index).expect("index json");
    index_json.push('\n');
    let index_path = corpus.join("index.json");
    if std::fs::read_to_string(&index_path).ok().as_deref() != Some(index_json.as_str()) {
        if update {
            std::fs::write(&index_path, &index_json).expect("write index");
        } else {
            stale.push("index.json".into());
        }
    }

    for entry in std::fs::read_dir(&corpus).expect("read corpus dir") {
        let path = entry.expect("corpus entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if name == "index.json" || index.contains_key(&name) {
            continue;
        }
        if update {
            std::fs::remove_file(&path).expect("remove left-over corpus file");
        } else {
            stale.push(format!("{name} (no source)"));
        }
    }

    assert!(
        stale.is_empty(),
        "fmt corpus is stale: {stale:?}; run UPDATE_FMT_CORPUS=1 cargo test -p \
         bookclerk-plugin-manifest --test fmt_corpus"
    );
}
