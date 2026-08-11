//! CLI entry for the `bookclerk-plugin` binary (feature `tools`).
//!
//! Audience: humans and CI invoking authoring subcommands. Guest plugin crates
//! should leave feature `tools` off so they do not link `bookclerk-workerd`.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use bookclerk_plugin_manifest::{format_manifest, parse};

use super::{check_plugin, package_plugin, smoke_plugin, sync_embed};

/// Runs the authoring CLI (`check` / `fmt` / `sync-embed` / `package` / `smoke`).
///
/// Reads `std::env::args` after the binary name. Unknown commands or missing
/// required flags print usage to stderr and return exit code `2`.
///
/// **Feature gate:** available only with `--features tools` (also required by
/// the `bookclerk-plugin` binary).
///
/// # Returns
///
/// Process [`ExitCode`]: `0` on success, `1` on command failure, `2` on usage
/// errors.
///
/// # Examples
///
/// ```ignore
/// // from src/bin/bookclerk-plugin.rs
/// fn main() -> std::process::ExitCode {
///     bookclerk_plugin_sdk::tools::run_tools_cli()
/// }
/// ```
pub fn run() -> ExitCode {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        eprint_usage();
        return ExitCode::from(2);
    }
    let cmd = args.remove(0);
    match cmd.as_str() {
        "check" => cmd_check(&args),
        "fmt" => cmd_fmt(&args),
        "sync-embed" => cmd_sync_embed(&args),
        "package" => cmd_package(&args),
        "smoke" => cmd_smoke(&args),
        "-h" | "--help" | "help" => {
            eprint_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {other}");
            eprint_usage();
            ExitCode::from(2)
        }
    }
}

fn eprint_usage() {
    eprintln!(
        "\
bookclerk-plugin — Bookclerk plugin authoring helpers

Usage:
  bookclerk-plugin check [dir]
  bookclerk-plugin fmt [--check] [plugin.toml]
  bookclerk-plugin sync-embed [dir]
  bookclerk-plugin package --out <dir> [plugin-dir]
  bookclerk-plugin smoke [dir]
"
    );
}

fn cmd_check(args: &[String]) -> ExitCode {
    let dir = PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    match check_plugin(&dir) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("check failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_sync_embed(args: &[String]) -> ExitCode {
    let dir = PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    match sync_embed(&dir) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("sync-embed failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_smoke(args: &[String]) -> ExitCode {
    let dir = PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    match smoke_plugin(&dir) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("smoke failed: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_fmt(args: &[String]) -> ExitCode {
    let mut check_only = false;
    let mut path = PathBuf::from("plugin.toml");
    for a in args {
        if a == "--check" {
            check_only = true;
        } else if !a.starts_with('-') {
            path = PathBuf::from(a);
        } else {
            eprintln!("unknown fmt flag: {a}");
            return ExitCode::from(2);
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("read {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let formatted = match parse(&text).and_then(|m| format_manifest(&m)) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("fmt failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    if check_only {
        if text == formatted || normalize_nl(&text) == normalize_nl(&formatted) {
            println!("ok {}", path.display());
            ExitCode::SUCCESS
        } else {
            eprintln!("would reformat {}", path.display());
            ExitCode::FAILURE
        }
    } else {
        match std::fs::write(&path, &formatted) {
            Ok(()) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("write {}: {err}", path.display());
                ExitCode::FAILURE
            }
        }
    }
}

fn normalize_nl(s: &str) -> String {
    let mut t = s.replace("\r\n", "\n");
    if !t.ends_with('\n') {
        t.push('\n');
    }
    t
}

fn cmd_package(args: &[String]) -> ExitCode {
    let mut out = None;
    let mut dir = PathBuf::from(".");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--out requires a directory");
                    return ExitCode::from(2);
                }
                out = Some(PathBuf::from(&args[i]));
            }
            a if !a.starts_with('-') => dir = PathBuf::from(a),
            a => {
                eprintln!("unknown package flag: {a}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let Some(out) = out else {
        eprintln!("package requires --out <dir>");
        return ExitCode::from(2);
    };
    match package_plugin(&dir, &out) {
        Ok(path) => {
            println!("packed {}", path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("package failed: {err}");
            ExitCode::FAILURE
        }
    }
}
