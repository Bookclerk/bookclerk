//! Author-tools conformance against shared fixtures.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn bookclerk_plugin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bookclerk-plugin"));
    cmd.current_dir(repo_root());
    cmd
}

fn check_fixture(name: &str) -> std::process::Output {
    let dir = repo_root()
        .join("crates/bookclerk-plugin-abi/fixtures/tools")
        .join(name);
    bookclerk_plugin()
        .args(["check", dir.to_str().expect("utf8 path")])
        .output()
        .expect("run check")
}

fn assert_check_fails(name: &str, needle: &str) {
    let out = check_fixture(name);
    assert!(!out.status.success(), "{name} should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(needle),
        "{name} stderr missing {needle:?}: {err}"
    );
}

#[test]
fn check_valid_workerd() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-workerd");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_accepts_leading_separator_from_cargo_alias() {
    // `cargo plugin -- check <dir>` forwards `--` before the subcommand.
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-workerd");
    let out = bookclerk_plugin()
        .args(["--", "check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_rejects_outbound_without_domains() {
    let dir =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-outbound-no-domains");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn check_valid_logo_url() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-logo-url");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_valid_logo_path() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-logo-path");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(out.status.success(), "{:?}", out);
}

#[test]
fn check_rejects_logo_javascript() {
    let dir =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-logo-javascript");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn check_rejects_logo_vbscript() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-logo-vbscript");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

#[test]
fn check_rejects_logo_parent() {
    let dir = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-logo-parent");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

/// Recursively copies `src` into `dst` (created if missing).
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).expect("create dir");
    for entry in std::fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// Copies the `valid-compat-date-future` fixture into a temp dir with its
/// `compatibility_date` set to the day after the workerd pin, so the boundary
/// tracks every pin bump.
///
/// # Returns
///
/// The temp dir holding the rewritten fixture; dropping it removes the copy.
fn future_date_fixture() -> tempfile::TempDir {
    let src =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-compat-date-future");
    let newer = chrono::NaiveDate::parse_from_str(
        bookclerk_plugin_manifest::WORKERD_PIN_COMPAT_DATE,
        "%Y-%m-%d",
    )
    .expect("pin is a calendar date")
    .succ_opt()
    .expect("day after pin")
    .to_string();
    let dir = tempfile::tempdir().expect("tempdir");
    copy_dir_all(&src, dir.path());
    let manifest_path = dir.path().join("plugin.toml");
    let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture");
    let rewritten: String = manifest
        .lines()
        .map(|line| {
            if line.starts_with("compatibility_date = ") {
                format!("compatibility_date = \"{newer}\"\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    assert!(
        rewritten.contains(&newer),
        "fixture has no compatibility_date"
    );
    std::fs::write(manifest_path, rewritten).expect("write fixture");
    dir
}

#[test]
fn check_warns_when_compatibility_date_is_newer_than_pin() {
    let dir = future_date_fixture();
    let out = bookclerk_plugin()
        .args(["check", dir.path().to_str().expect("utf8 path")])
        .output()
        .expect("run check");
    assert!(
        out.status.success(),
        "newer date should load: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("Falling back"),
        "stderr missing fallback warning: {err}"
    );
}

#[test]
fn check_rejects_non_calendar_compatibility_date() {
    assert_check_fails("invalid-compat-date-shape", "YYYY-MM-DD");
}

#[test]
fn check_rejects_unknown_compatibility_flag() {
    assert_check_fails("invalid-compat-flag", "not allowed");
}

#[test]
fn check_rejects_experimental_compatibility_flag() {
    assert_check_fails("invalid-compat-experimental", "host-only");
}

#[test]
fn check_rejects_python_without_flag_pair() {
    assert_check_fails("invalid-python-flags-missing", "must include");
}

#[test]
fn check_rejects_python_flags_without_python_module() {
    assert_check_fails("invalid-flags-without-python", "Python module");
}

#[test]
fn check_rejects_module_type_mismatch() {
    assert_check_fails("invalid-module-type", "does not match");
}

#[test]
fn check_rejects_module_path_when_name_matches_a_different_file() {
    assert_check_fails("invalid-module-path", "missing.js");
    assert_check_fails("invalid-module-path", "not in the workerd load set");
}

#[test]
fn check_accepts_explicit_path_over_py_name() {
    let path_wins = check_fixture("valid-module-path-wins");
    assert!(
        path_wins.status.success(),
        "explicit path must win over a .py name: {}",
        String::from_utf8_lossy(&path_wins.stderr)
    );
    let helper = check_fixture("valid-js-with-python-helper");
    assert!(
        helper.status.success(),
        "JS main with a Python helper must lint as JS: {}",
        String::from_utf8_lossy(&helper.stderr)
    );
}

#[test]
fn check_accepts_module_name_when_path_is_omitted() {
    let out = check_fixture("valid-module-name-only");
    assert!(
        out.status.success(),
        "omitted path should use name: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_rejects_kv_oauth_binding() {
    assert_check_fails("invalid-kv-oauth", "collides");
    assert_check_fails("invalid-kv-secret", "collides");
    assert_check_fails("invalid-kv-work-fs", "collides");
    assert_check_fails("invalid-kv-oauth-name", "collides");
    assert_check_fails("invalid-producer-database", "collides");
    let shared = check_fixture("valid-producer-shares-oauth-name");
    assert!(
        shared.status.success(),
        "a producer may reuse the oauth binding name ({})",
        u8::from(shared.status.success())
    );
}

#[test]
fn check_rejects_undeclared_python_file() {
    assert_check_fails("invalid-undeclared-python", "undeclared Python file");
}

#[test]
fn check_rejects_typescript_main_as_not_implemented() {
    assert_check_fails("invalid-module-ts", "not implemented yet");
}

#[test]
fn check_accepts_kv_declaration() {
    let out = check_fixture("valid-kv");
    assert!(
        out.status.success(),
        "kv declaration must be legal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_rejects_bad_kv_namespace() {
    assert_check_fails("invalid-kv-namespace", "namespace");
}

#[test]
fn check_rejects_plugin_producing_host_event() {
    assert_check_fails("invalid-producer-host-event", "emitted by the host");
}

#[test]
fn check_accepts_queues_declaration() {
    let out = check_fixture("valid-queues");
    assert!(
        out.status.success(),
        "queues declaration must stay legal: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn check_rejects_native_with_domains() {
    let dir =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/invalid-native-with-domains");
    let out = bookclerk_plugin()
        .args(["check", dir.to_str().unwrap()])
        .output()
        .expect("run check");
    assert!(!out.status.success());
}

/// `fmt` rewrites the every-section fixture to its gold byte for byte, and
/// `fmt --check` accepts the gold but refuses the unformatted source. The
/// Python and TypeScript SDKs compare their formatters against the same gold.
#[test]
fn fmt_full_matches_gold() {
    let fixture = repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/fmt-full");
    let gold = std::fs::read_to_string(fixture.join("plugin.fmt.toml")).expect("read gold");
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("plugin.toml");
    std::fs::copy(fixture.join("plugin.toml"), &target).expect("copy fixture");

    let check = bookclerk_plugin()
        .args(["fmt", "--check", target.to_str().unwrap()])
        .output()
        .expect("run fmt --check");
    assert!(
        !check.status.success(),
        "the unformatted source passed --check"
    );

    let out = bookclerk_plugin()
        .args(["fmt", target.to_str().unwrap()])
        .output()
        .expect("run fmt");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&target).expect("read output"), gold);

    let gold_check = bookclerk_plugin()
        .args([
            "fmt",
            "--check",
            fixture.join("plugin.fmt.toml").to_str().unwrap(),
        ])
        .output()
        .expect("run fmt --check on the gold");
    assert!(
        gold_check.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&gold_check.stderr)
    );
}

#[test]
fn fmt_check_gold_native() {
    let file =
        repo_root().join("crates/bookclerk-plugin-abi/fixtures/tools/valid-native/plugin.fmt.toml");
    let out = bookclerk_plugin()
        .args(["fmt", "--check", file.to_str().unwrap()])
        .output()
        .expect("run fmt");
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Reads a shared JSON fixture beside the other author-tool fixtures.
fn shared_cases(name: &str) -> serde_json::Value {
    let file = repo_root()
        .join("crates/bookclerk-plugin-abi/fixtures/tools")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&file).expect("read cases")).expect("parse cases")
}

/// Every logo in `logo-cases.json` gets the classification or error text the
/// file names. The Python and TypeScript checkers read the same file.
#[test]
fn validate_logo_matches_shared_cases() {
    use bookclerk_plugin_manifest::logo::{validate_logo, LogoKind};

    let cases = shared_cases("logo-cases.json");
    for case in cases.as_array().expect("an array of cases") {
        let input = case["input"].as_str().expect("input");
        let got = match validate_logo(input) {
            Ok(LogoKind::RemoteUrl(v)) => serde_json::json!({ "input": input, "remote": v }),
            Ok(LogoKind::EmbeddedPath(v)) => serde_json::json!({ "input": input, "embedded": v }),
            Err(err) => serde_json::json!({ "input": input, "error": err.to_string() }),
        };
        assert_eq!(&got, case);
    }
}

/// `compat-date-cases.json` splits dates into ones every checker accepts and
/// ones every checker refuses.
#[test]
fn compatibility_dates_match_shared_cases() {
    use bookclerk_plugin_manifest::workerd_contract::validate_author_compatibility_date;

    let cases = shared_cases("compat-date-cases.json");
    for (list, ok) in [("valid", true), ("invalid", false)] {
        for date in cases[list].as_array().expect("a list of dates") {
            let date = date.as_str().expect("a string date");
            assert_eq!(
                validate_author_compatibility_date(date).is_ok(),
                ok,
                "{date:?}"
            );
        }
    }
}

/// Every manifest in `manifest-cases.json` is accepted, or refused with the
/// exact message the case names. The Python and TypeScript checkers read the
/// same file, so the three agree on the rules and their wording.
#[test]
fn validate_manifest_matches_shared_cases() {
    let doc = shared_cases("manifest-cases.json");
    let part = |case: &serde_json::Value, key: &str| -> String {
        case.get(key)
            .or_else(|| doc.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let prelude = doc["prelude"].as_str().expect("prelude");
    for case in doc["cases"].as_array().expect("an array of cases") {
        let text = format!(
            "{prelude}{}\n{}\n{}",
            part(case, "top"),
            part(case, "base_tables"),
            case["tables"].as_str().unwrap_or_default()
        );
        let got = match bookclerk_plugin_tools::load_manifest(&text) {
            Ok(_) => serde_json::Value::Null,
            Err(err) => err.to_string().into(),
        };
        assert_eq!(got, case["error"], "{}", case["name"]);
    }
}

/// Every main module in `main-module-cases.json` passes the author-model
/// check, or fails with the exact message the case names. The Python and
/// TypeScript checkers read the same file.
#[test]
fn check_main_module_source_matches_shared_cases() {
    use bookclerk_plugin_manifest::Entrypoint;
    use bookclerk_plugin_tools::{check_main_module_source, MainModuleLanguage};

    let cases = shared_cases("main-module-cases.json");
    for case in cases.as_array().expect("an array of cases") {
        let (language, main) = match case["language"].as_str() {
            Some("js") => (MainModuleLanguage::Js, "index.js"),
            Some("python") => (MainModuleLanguage::Python, "main.py"),
            other => panic!("unknown language {other:?}"),
        };
        let entrypoints: Vec<Entrypoint> =
            serde_json::from_value(case["entrypoints"].clone()).expect("entrypoint names");
        let src = case["src"].as_str().expect("src");
        let got = match check_main_module_source(main, src, &entrypoints, language) {
            Ok(()) => serde_json::Value::Null,
            Err(err) => err.to_string().into(),
        };
        assert_eq!(got, case["error"], "{}", case["name"]);
    }
}

/// A plugin directory whose `cargo test` suite records the profile each run
/// was handed, so `bookclerk-plugin test` can be checked without the kit.
fn recording_cargo_plugin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("plugin.toml"), "").expect("plugin.toml");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"recording_plugin\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("Cargo.toml");
    std::fs::create_dir(dir.path().join("src")).expect("src");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        r#"
#[test]
fn record_profile() {
    use std::io::Write as _;
    let kit = std::env::var("BOOKCLERK_DB_TESTKIT").expect("kit path");
    assert!(std::path::Path::new(&kit).is_file(), "{kit}");
    let profile = std::env::var("BOOKCLERK_DB_TESTKIT_PROFILE").expect("profile");
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(std::env::var("RECORD_FILE").expect("record file"))
        .expect("open log");
    writeln!(log, "{profile}").expect("write");
    if let Ok(refused) = std::env::var("FAIL_UNDER") {
        assert_ne!(profile, refused, "refused on purpose");
    }
}
"#,
    )
    .expect("lib.rs");
    dir
}

/// Runs `bookclerk-plugin test` on `plugin`, with the record file and the
/// fake kit path in the environment, and returns the output and the
/// profiles the suite recorded. The suite fails under `fail_under`, when
/// one is named.
fn run_plugin_test(
    plugin: &tempfile::TempDir,
    args: &[&str],
    fail_under: Option<&str>,
) -> (std::process::Output, Vec<String>) {
    let record = plugin.path().join("profiles.log");
    let kit = plugin.path().join("plugin.toml");
    let mut cmd = bookclerk_plugin();
    cmd.arg("test")
        .arg(plugin.path())
        .args(args)
        .env("BOOKCLERK_DB_TESTKIT", &kit)
        .env("RECORD_FILE", &record)
        .env("CARGO_TARGET_DIR", plugin.path().join("target"))
        .env_remove("RUSTFLAGS");
    if let Some(profile) = fail_under {
        cmd.env("FAIL_UNDER", profile);
    }
    let out = cmd.output().expect("run test");
    let recorded = std::fs::read_to_string(&record)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    (out, recorded)
}

#[test]
fn test_runs_the_suite_once_per_profile() {
    let plugin = recording_cargo_plugin();
    let (out, recorded) = run_plugin_test(&plugin, &[], None);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(recorded, ["first-party", "d1", "minimum"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for profile in ["first-party", "d1", "minimum"] {
        assert!(
            stdout.contains(&format!("profile {profile}: ok")),
            "{stdout}"
        );
    }

    let (out, recorded) = run_plugin_test(&plugin, &["--profile", "minimum"], None);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(recorded, ["first-party", "d1", "minimum", "minimum"]);
}

#[test]
fn test_runs_the_later_profiles_after_the_first_one_fails() {
    let plugin = recording_cargo_plugin();
    let (out, recorded) = run_plugin_test(&plugin, &["--", "record_profile"], Some("first-party"));
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(recorded, ["first-party", "d1", "minimum"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("profile first-party: failed"), "{stderr}");
    assert!(stderr.contains("profile d1: ok"), "{stderr}");
    assert!(stderr.contains("profile minimum: ok"), "{stderr}");
    assert!(
        stderr.contains("tests failed under profile(s) first-party"),
        "{stderr}"
    );
}

#[test]
fn test_rejects_bad_usage_before_running_anything() {
    let plugin = recording_cargo_plugin();
    let out = bookclerk_plugin()
        .args(["test", "--profile"])
        .output()
        .expect("run test");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = bookclerk_plugin()
        .args(["test", "--bogus"])
        .output()
        .expect("run test");
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    let (out, recorded) = run_plugin_test(&plugin, &["--profile", "postgres"], None);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(recorded.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown profile"));

    let out = bookclerk_plugin()
        .args(["test", plugin.path().to_str().expect("utf8")])
        .env("BOOKCLERK_DB_TESTKIT", plugin.path().join("missing-kit"))
        .output()
        .expect("run test");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a file"));
}
