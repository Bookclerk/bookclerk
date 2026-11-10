//! `bookclerk-plugin test`: a plugin's own test suite under every database
//! capability profile.
//!
//! Audience: authors whose tests open their `[[databases]]` binding through
//! the SDK's `createTestDatabase` / `create_test_database`. The command runs
//! the plugin's test runner once per profile with `BOOKCLERK_DB_TESTKIT`
//! pointing at the kit binary and `BOOKCLERK_DB_TESTKIT_PROFILE` naming the
//! profile, which the SDK wrappers read as their default profile.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bookclerk_plugin_sdk::{Result, SdkError};

/// The capability profiles the kit serves, in the order a run reports them.
///
/// The kit binary owns the definitions (`bookclerk_db_testkit::Profile`); it
/// refuses a name outside this list, so the two stay aligned by its test.
pub const TEST_DATABASE_PROFILES: [&str; 3] = ["first-party", "d1", "minimum"];

/// Environment variable the SDK wrappers read for the kit binary's path.
pub const KIT_BINARY_ENV: &str = "BOOKCLERK_DB_TESTKIT";

/// Environment variable the SDK wrappers read as their default profile.
pub const KIT_PROFILE_ENV: &str = "BOOKCLERK_DB_TESTKIT_PROFILE";

/// File name of the kit binary without a platform suffix.
const KIT_BINARY_NAME: &str = "bookclerk-db-testkit";

/// The test runner a plugin directory's layout names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestRunner {
    /// `package.json` with a `test` script: `npm test`.
    Npm,
    /// `pyproject.toml`: `python -m pytest`.
    Pytest,
    /// `Cargo.toml`: `cargo test`.
    Cargo,
}

impl TestRunner {
    /// The program and leading arguments that run this runner's suite.
    ///
    /// # Returns
    ///
    /// The executable name followed by the arguments before any the author
    /// passes after `--`.
    #[must_use]
    pub fn argv(self) -> Vec<String> {
        match self {
            Self::Npm => vec![npm_program().into(), "test".into()],
            Self::Pytest => vec![python_program().into(), "-m".into(), "pytest".into()],
            Self::Cargo => vec!["cargo".into(), "test".into()],
        }
    }

    /// Short name for reports (`npm`, `pytest`, `cargo`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pytest => "pytest",
            Self::Cargo => "cargo",
        }
    }
}

/// `npm` is a `.cmd` shim on Windows.
fn npm_program() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

/// Windows installs ship `python`, Unix package managers `python3`.
fn python_program() -> &'static str {
    if cfg!(windows) {
        "python"
    } else {
        "python3"
    }
}

/// Picks the test runner from the files in `plugin_dir`.
///
/// `package.json` wins when it declares a `test` script, then
/// `pyproject.toml`, then `Cargo.toml`, so a TypeScript plugin that also
/// carries a `Cargo.toml` for a WASM module runs its JavaScript suite.
///
/// # Arguments
///
/// * `plugin_dir` - Directory containing `plugin.toml`.
///
/// # Returns
///
/// The runner to invoke.
///
/// # Errors
///
/// Returns [`SdkError`] when no runner is recognized or `package.json`
/// cannot be parsed.
pub fn detect_test_runner(plugin_dir: &Path) -> Result<TestRunner> {
    let package_json = plugin_dir.join("package.json");
    if package_json.is_file() {
        let text = std::fs::read_to_string(&package_json)
            .map_err(|e| SdkError::message(format!("read {}: {e}", package_json.display())))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| SdkError::message(format!("parse {}: {e}", package_json.display())))?;
        if value
            .get("scripts")
            .and_then(|s| s.get("test"))
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            return Ok(TestRunner::Npm);
        }
    }
    if plugin_dir.join("pyproject.toml").is_file() {
        return Ok(TestRunner::Pytest);
    }
    if plugin_dir.join("Cargo.toml").is_file() {
        return Ok(TestRunner::Cargo);
    }
    Err(SdkError::message(format!(
        "no test runner found under {}: expected package.json with a `test` script, \
         pyproject.toml or Cargo.toml",
        plugin_dir.display()
    )))
}

/// Locates the `bookclerk-db-testkit` binary.
///
/// Order: `BOOKCLERK_DB_TESTKIT`, then a sibling of this executable (a
/// workspace `target/<profile>` holds both after
/// `cargo build -p bookclerk-db-testkit`), then `PATH`.
///
/// # Returns
///
/// The binary's path.
///
/// # Errors
///
/// Returns [`SdkError`] naming the three places searched when none holds the
/// binary.
pub fn kit_binary() -> Result<PathBuf> {
    kit_binary_in(
        std::env::var_os(KIT_BINARY_ENV),
        std::env::current_exe().ok(),
        std::env::var_os("PATH"),
    )
}

/// [`kit_binary`] over explicit inputs, for tests.
fn kit_binary_in(
    env_path: Option<OsString>,
    current_exe: Option<PathBuf>,
    path_var: Option<OsString>,
) -> Result<PathBuf> {
    if let Some(explicit) = env_path.filter(|p| !p.is_empty()) {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return absolute_kit(path);
        }
        return Err(SdkError::message(format!(
            "{KIT_BINARY_ENV} names {} but it is not a file",
            path.display()
        )));
    }
    let file_name = format!("{KIT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);
    if let Some(sibling) = current_exe
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .map(|dir| dir.join(&file_name))
        .filter(|p| p.is_file())
    {
        return absolute_kit(sibling);
    }
    if let Some(found) = path_var
        .map(|var| std::env::split_paths(&var).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join(&file_name))
        .find(|p| p.is_file())
    {
        return absolute_kit(found);
    }
    Err(SdkError::message(format!(
        "{KIT_BINARY_NAME} not found: set {KIT_BINARY_ENV}, build it beside this binary \
         (`cargo build -p bookclerk-db-testkit`), or put it on PATH"
    )))
}

/// The kit path as the runner must see it: absolute, so a path given
/// relative to this command's working directory still names the binary
/// after the runner changes into the plugin directory.
fn absolute_kit(path: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&path)
        .map_err(|e| SdkError::message(format!("resolve {}: {e}", path.display())))
}

/// Runs the plugin's test suite once per profile.
///
/// Each run inherits stdio and gets `BOOKCLERK_DB_TESTKIT` and
/// `BOOKCLERK_DB_TESTKIT_PROFILE` in its environment. Every profile runs
/// even after one fails, so a report names them all.
///
/// # Arguments
///
/// * `plugin_dir` - Directory containing `plugin.toml` and the test runner's
///   manifest.
/// * `profiles` - Profile names to run, each one of
///   [`TEST_DATABASE_PROFILES`].
/// * `runner_args` - Extra arguments appended to the runner's command line
///   (after `--` for `npm test` and `cargo test`).
///
/// # Returns
///
/// One `profile <name>: ok` line per profile for CLI stdout.
///
/// # Errors
///
/// Returns [`SdkError`] when a profile name is unknown, no runner is found,
/// the kit binary is missing, a runner cannot start, or any profile's run
/// exits non-zero (the message lists the failed profiles).
pub fn test_plugin(
    plugin_dir: &Path,
    profiles: &[String],
    runner_args: &[String],
) -> Result<String> {
    for profile in profiles {
        if !TEST_DATABASE_PROFILES.contains(&profile.as_str()) {
            return Err(SdkError::message(format!(
                "unknown profile `{profile}` (one of {})",
                TEST_DATABASE_PROFILES.join(", ")
            )));
        }
    }
    let toml_path = plugin_dir.join("plugin.toml");
    if !toml_path.is_file() {
        return Err(SdkError::message(format!(
            "{} is missing: `test` runs from a plugin directory",
            toml_path.display()
        )));
    }
    let runner = detect_test_runner(plugin_dir)?;
    let kit = kit_binary()?;
    let mut lines = Vec::with_capacity(profiles.len());
    let mut failed = Vec::new();
    for profile in profiles {
        let mut argv = runner.argv();
        if !runner_args.is_empty() {
            if matches!(runner, TestRunner::Npm | TestRunner::Cargo) {
                argv.push("--".into());
            }
            argv.extend(runner_args.iter().cloned());
        }
        eprintln!(
            "bookclerk-plugin test: profile {profile} via {}",
            runner.name()
        );
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(plugin_dir)
            .env(KIT_BINARY_ENV, &kit)
            .env(KIT_PROFILE_ENV, profile)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| SdkError::message(format!("start {}: {e}", argv.join(" "))))?;
        if status.success() {
            lines.push(format!("profile {profile}: ok"));
        } else {
            lines.push(format!("profile {profile}: failed ({status})"));
            failed.push(profile.as_str());
        }
    }
    if failed.is_empty() {
        Ok(lines.join("\n"))
    } else {
        Err(SdkError::message(format!(
            "{}\ntests failed under profile(s) {}",
            lines.join("\n"),
            failed.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).expect("write");
        }
        dir
    }

    #[test]
    fn npm_wins_when_package_json_has_a_test_script() {
        let dir = dir_with(&[
            ("package.json", r#"{"scripts":{"test":"vitest run"}}"#),
            ("Cargo.toml", "[package]\nname = \"wasm\"\n"),
        ]);
        assert_eq!(
            detect_test_runner(dir.path()).expect("runner"),
            TestRunner::Npm
        );
    }

    #[test]
    fn package_json_without_a_test_script_falls_through() {
        let dir = dir_with(&[
            ("package.json", r#"{"scripts":{"build":"tsc"}}"#),
            ("pyproject.toml", ""),
        ]);
        assert_eq!(
            detect_test_runner(dir.path()).expect("runner"),
            TestRunner::Pytest
        );
        let dir = dir_with(&[("package.json", "{}"), ("Cargo.toml", "")]);
        assert_eq!(
            detect_test_runner(dir.path()).expect("runner"),
            TestRunner::Cargo
        );
    }

    #[test]
    fn malformed_package_json_is_an_error() {
        let dir = dir_with(&[("package.json", "{")]);
        let err = detect_test_runner(dir.path()).expect_err("parse error");
        assert!(err.to_string().contains("parse"), "{err}");
    }

    #[test]
    fn no_runner_is_an_error_naming_the_manifests() {
        let dir = dir_with(&[]);
        let err = detect_test_runner(dir.path()).expect_err("no runner");
        assert!(err.to_string().contains("pyproject.toml"), "{err}");
    }

    #[test]
    fn runner_argv_names_the_suite_command() {
        assert_eq!(TestRunner::Cargo.argv(), vec!["cargo", "test"]);
        assert_eq!(TestRunner::Pytest.argv()[1..], ["-m", "pytest"]);
        assert_eq!(TestRunner::Npm.argv()[1], "test");
        assert_eq!(TestRunner::Npm.name(), "npm");
        assert_eq!(TestRunner::Pytest.name(), "pytest");
        assert_eq!(TestRunner::Cargo.name(), "cargo");
    }

    #[test]
    fn kit_binary_prefers_the_environment_variable() {
        let dir = dir_with(&[("kit", "")]);
        let kit = dir.path().join("kit");
        let found = kit_binary_in(Some(kit.clone().into_os_string()), None, None).expect("kit");
        assert_eq!(found, kit);
        let err = kit_binary_in(
            Some(dir.path().join("missing").into_os_string()),
            None,
            None,
        )
        .expect_err("missing file");
        assert!(err.to_string().contains("not a file"), "{err}");
    }

    #[test]
    fn kit_binary_falls_back_to_a_sibling_then_path() {
        let name = format!("{KIT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);
        let sibling_dir = dir_with(&[(name.as_str(), "")]);
        let exe = sibling_dir.path().join("bookclerk-plugin");
        let found = kit_binary_in(Some(OsString::new()), Some(exe), None).expect("sibling");
        assert_eq!(found, sibling_dir.path().join(&name));

        let path_dir = dir_with(&[(name.as_str(), "")]);
        let empty_dir = dir_with(&[]);
        let path_var = std::env::join_paths([empty_dir.path(), path_dir.path()]).expect("join");
        let found = kit_binary_in(
            None,
            Some(empty_dir.path().join("bookclerk-plugin")),
            Some(path_var),
        )
        .expect("path");
        assert_eq!(found, path_dir.path().join(&name));

        let err = kit_binary_in(None, None, None).expect_err("nowhere");
        assert!(err.to_string().contains(KIT_BINARY_ENV), "{err}");
    }

    #[test]
    fn unknown_profile_and_missing_manifest_are_refused_before_running() {
        let dir = dir_with(&[("plugin.toml", ""), ("Cargo.toml", "")]);
        let err = test_plugin(dir.path(), &["postgres".into()], &[]).expect_err("profile");
        assert!(err.to_string().contains("unknown profile"), "{err}");
        let bare = dir_with(&[("Cargo.toml", "")]);
        let err = test_plugin(bare.path(), &["minimum".into()], &[]).expect_err("manifest");
        assert!(err.to_string().contains("plugin.toml"), "{err}");
    }

    #[test]
    fn a_relative_kit_path_is_made_absolute_for_the_runner() {
        // `Cargo.toml` is a file relative to the test's working directory;
        // the runner starts in the plugin directory, so it must get the
        // absolute path.
        let found = kit_binary_in(Some(OsString::from("Cargo.toml")), None, None).expect("kit");
        assert!(found.is_absolute(), "{}", found.display());
        assert_eq!(
            found,
            std::env::current_dir().expect("cwd").join("Cargo.toml")
        );
    }
}
