//! Inventory gate: product plugins and Echo examples are `api_version = 2`.

use std::fs;
use std::path::Path;

use bookclerk_plugin_manifest::PluginManifest;

fn walk_plugin_tomls(root: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_plugin_tomls(&path, out);
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) != Some("plugin.toml") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        out.push((path.display().to_string(), text));
    }
}

#[test]
fn product_and_echo_manifests_are_api_version_2() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    walk_plugin_tomls(&workspace.join("crates/bookclerk-plugins"), &mut files);
    walk_plugin_tomls(&workspace.join("examples"), &mut files);
    assert!(
        !files.is_empty(),
        "expected plugin.toml files under crates/bookclerk-plugins and examples"
    );
    for (path, text) in &files {
        let manifest =
            PluginManifest::parse(text).unwrap_or_else(|err| panic!("{path}: parse failed: {err}"));
        assert_eq!(manifest.api_version, 2, "{path} must be api_version = 2");
    }
}

fn walk_rs_files(root: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "target" || name == "node_modules" {
                continue;
            }
            walk_rs_files(&path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn product_guests_implement_plugin_root_not_v2_wrappers() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    walk_rs_files(&workspace.join("crates/bookclerk-plugins"), &mut files);
    walk_rs_files(&workspace.join("examples"), &mut files);
    assert!(
        !files.is_empty(),
        "expected .rs files under crates/bookclerk-plugins and examples"
    );
    for path in &files {
        let text = fs::read_to_string(path).unwrap_or_default();
        let display = path.display();
        assert!(
            !text.contains("V2PluginRoot"),
            "{display} must not instantiate V2PluginRoot; implement PluginRoot directly"
        );
        assert!(
            !text.contains("impl BookclerkPlugin"),
            "{display} must not implement the removed BookclerkPlugin wrap"
        );
    }
}
