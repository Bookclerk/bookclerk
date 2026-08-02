//! Applies a confinement policy to itself, then becomes the program it was
//! asked to run.
//!
//! ```text
//! BOOKCLERK_JAIL_SPEC='{"label":"plugin:libro", …}' bookclerk-jail /path/to/guest
//! ```
//!
//! # Why a separate process
//!
//! Landlock and Seatbelt restrictions are inherited across `exec` and cannot be
//! relaxed afterwards, so a parent that confines itself and then `exec`s another
//! binary hands that binary a jail it has no way to refuse. That is the only
//! arrangement that works for a plugin, because the plugin binary *is* the
//! untrusted part: a guest asked to confine itself would simply not.
//!
//! Doing it in the host instead is not an option. Both backends allocate, which
//! is unsafe after `fork` in a threaded process, and Landlock's `restrict_self`
//! binds the calling thread rather than the process — a runtime's worker threads
//! would stay unconfined. This binary is single-threaded, so it has neither
//! problem.
//!
//! # Windows
//!
//! Windows has no self-confinement primitive. The launcher therefore
//! `CreateProcess`es the guest into an AppContainer (via
//! [`bookclerk_sandbox::spawn::run_appcontainer`]) and proxies stdio until the
//! guest exits, instead of confining itself and `exec`ing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use bookclerk_sandbox::{Spec, SPEC_ENV};

mod fds;

fn main() -> ExitCode {
    let (program, args) = match parse_args(std::env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(err) => return fail(&err),
    };

    let spec = match read_spec(std::env::var(SPEC_ENV)) {
        Ok(spec) => spec,
        Err(err) => return fail(&err),
    };

    // Before confining rather than after: the sweep reads the kernel's own
    // listing of this process's descriptors, and that listing lives at a path
    // the allowlist has no reason to name.
    if let Err(err) = fds::close_inherited(&spec.preserve_fds) {
        return fail(&err);
    }

    #[cfg(windows)]
    {
        windows_run(&spec, &program, &args)
    }

    #[cfg(not(windows))]
    {
        if let Err(err) = confine(&spec) {
            return fail(&err);
        }
        exec(&program, &args)
    }
}

/// Launch the guest inside an AppContainer and forward its exit status.
#[cfg(windows)]
fn windows_run(spec: &Spec, program: &Path, args: &[OsString]) -> ExitCode {
    use bookclerk_sandbox::Enforcement;

    let missing = missing_paths(spec);
    if !missing.is_empty() {
        return fail(&format!(
            "these allowlist paths do not exist: {}",
            missing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let policy = spec.policy();
    match policy.enforcement_mode() {
        Enforcement::Disabled => {
            eprintln!("bookclerk-jail: AppContainer disabled; running guest unconfined");
            return exec_status(program, args);
        }
        Enforcement::Required | Enforcement::BestEffort => {}
    }

    match bookclerk_sandbox::spawn::plan_appcontainer(&policy) {
        Ok(plan) => eprintln!(
            "bookclerk-jail: windows AppContainer plan: profile={} package_sid={:?} capabilities={:?}",
            plan.profile_name, plan.package_sid, plan.capability_names
        ),
        Err(err) => {
            return match policy.enforcement_mode() {
                Enforcement::Required => fail(&err.to_string()),
                Enforcement::BestEffort | Enforcement::Disabled => {
                    eprintln!(
                        "bookclerk-jail: warning: AppContainer plan failed ({err}); \
                         continuing unconfined"
                    );
                    exec_status(program, args)
                }
            };
        }
    }

    // Do not hand the guest the shape of its own jail (mirrors Unix `exec`
    // which drops SPEC_ENV). CreateProcess inherits this process's environment.
    std::env::remove_var(SPEC_ENV);

    match bookclerk_sandbox::spawn::run_appcontainer(&policy, program, args) {
        Ok(code) => {
            eprintln!("bookclerk-jail: AppContainer guest exited with status {code}");
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        Err(err) => match policy.enforcement_mode() {
            Enforcement::Required => fail(&err.to_string()),
            Enforcement::BestEffort | Enforcement::Disabled => {
                eprintln!(
                    "bookclerk-jail: warning: AppContainer launch failed ({err}); \
                     continuing unconfined"
                );
                exec_status(program, args)
            }
        },
    }
}

/// Split `bookclerk-jail [--] <program> [args…]`.
///
/// A bare `--` is accepted so a caller can pass a program whose name would
/// otherwise look like an option.
fn parse_args<I: Iterator<Item = OsString>>(args: I) -> Result<(PathBuf, Vec<OsString>), String> {
    let mut args = args.peekable();
    if args.peek().is_some_and(|first| first == "--") {
        args.next();
    }
    let program = args
        .next()
        .ok_or_else(|| "usage: bookclerk-jail [--] <program> [args…]".to_string())?;
    Ok((PathBuf::from(program), args.collect()))
}

/// Parse the spec from the environment.
///
/// A missing variable is an error rather than "run unconfined". Reaching this
/// binary at all means the caller asked for a jail.
fn read_spec(raw: Result<String, std::env::VarError>) -> Result<Spec, String> {
    let raw = raw.map_err(|_| format!("{SPEC_ENV} is not set; refusing to run unconfined"))?;
    serde_json::from_str(&raw).map_err(|err| format!("could not parse {SPEC_ENV}: {err}"))
}

/// Apply the policy to this process.
#[cfg(not(windows))]
fn confine(spec: &Spec) -> Result<(), String> {
    // A rule naming a path that is not there is an error to both backends, so
    // the policy quietly drops missing entries. That is the right default for a
    // media job's optional inputs and the wrong one here: a mistyped cache root
    // would narrow the jail silently and surface much later as an unexplained
    // permission error from inside the guest.
    let missing = missing_paths(spec);
    if !missing.is_empty() {
        return Err(format!(
            "these allowlist paths do not exist: {}",
            missing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let report = spec
        .policy()
        .confine_current_process()
        .map_err(|err| err.to_string())?;

    // stderr is inherited from the host, so this lands in the daemon's log next
    // to whatever the guest goes on to say.
    eprintln!("bookclerk-jail: {}", report.summary());
    Ok(())
}

/// Declared paths that are not present on disk.
fn missing_paths(spec: &Spec) -> Vec<&Path> {
    spec.reads
        .iter()
        .chain(spec.writes.iter())
        .map(PathBuf::as_path)
        .filter(|path| !path.exists())
        .collect()
}

/// Replace this process with `program`.
///
/// The guest inherits the stdio pipes the host set up, so the JSON-RPC stream
/// runs between the host and the guest directly with nothing proxying it.
#[cfg(unix)]
fn exec(program: &Path, args: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;

    // The guest has no use for the spec, and not passing it on keeps a plugin
    // from learning the shape of its own jail.
    let err = Command::new(program).args(args).env_remove(SPEC_ENV).exec();
    fail(&format!("could not exec {}: {err}", program.display()))
}

/// Fallback for targets without `exec`: stay in the middle and forward the
/// exit status.
#[cfg(not(unix))]
fn exec_status(program: &Path, args: &[OsString]) -> ExitCode {
    let status = match Command::new(program)
        .args(args)
        .env_remove(SPEC_ENV)
        .status()
    {
        Ok(status) => status,
        Err(err) => return fail(&format!("could not run {}: {err}", program.display())),
    };
    match status.code() {
        Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        None => ExitCode::FAILURE,
    }
}

/// Report why nothing ran.
///
/// The host sees this on the guest's inherited stderr; on its own stdout it
/// sees a closed pipe, because a jail that could not be applied must not be
/// followed by a running guest.
fn fail(message: &str) -> ExitCode {
    eprintln!("bookclerk-jail: {message}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_a_program_and_its_arguments() {
        let (program, args) = parse_args(os(&["/opt/guest", "--verbose"]).into_iter()).expect("ok");
        assert_eq!(program, PathBuf::from("/opt/guest"));
        assert_eq!(args, os(&["--verbose"]));
    }

    #[test]
    fn a_leading_double_dash_is_skipped() {
        let (program, args) = parse_args(os(&["--", "/opt/guest"]).into_iter()).expect("ok");
        assert_eq!(program, PathBuf::from("/opt/guest"));
        assert!(args.is_empty());
    }

    #[test]
    fn no_program_is_a_usage_error() {
        let err = parse_args(std::iter::empty()).expect_err("must fail");
        assert!(err.contains("usage"), "{err}");
    }

    /// Running unconfined is never the fallback.
    #[test]
    fn a_missing_spec_is_refused() {
        let err = read_spec(Err(std::env::VarError::NotPresent)).expect_err("must fail");
        assert!(err.contains("refusing to run unconfined"), "{err}");
    }

    #[test]
    fn a_malformed_spec_is_refused() {
        let err = read_spec(Ok("{not json".to_string())).expect_err("must fail");
        assert!(err.contains("could not parse"), "{err}");
    }

    #[test]
    #[cfg(not(windows))]
    fn a_vanished_allowlist_path_is_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = Spec {
            reads: vec![dir.path().to_path_buf()],
            writes: vec![dir.path().join("nope")],
            ..Spec::new("probe")
        };
        let missing = missing_paths(&spec);
        assert_eq!(missing, vec![dir.path().join("nope").as_path()]);

        let err = confine(&spec).expect_err("must fail");
        assert!(err.contains("do not exist"), "{err}");
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    #[cfg(windows)]
    fn a_vanished_allowlist_path_is_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = Spec {
            reads: vec![dir.path().to_path_buf()],
            writes: vec![dir.path().join("nope")],
            ..Spec::new("probe")
        };
        let missing = missing_paths(&spec);
        assert_eq!(missing, vec![dir.path().join("nope").as_path()]);
    }
}
