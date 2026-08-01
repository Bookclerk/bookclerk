//! [`cargo` alias dispatcher](../README.md) for the external-plugin dev workflow.

use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use anyhow::{bail, Context, Result};
use bookclerk_dev::{
    default_artifacts, default_files_dir, package, plugins, workspace_root, workspace_version,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bookclerk-dev",
    about = "Bookclerk dev workflow (cargo alias target)"
)]
struct Cli {
    /// Use release profile for builds and `cargo run`.
    #[arg(long, global = true)]
    release: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build all first-party plugin guest binaries.
    BuildPlugins,
    /// Build plugins and copy binaries + manifests to `target/plugin-artifacts`.
    StagePlugins {
        /// Staging directory (default: `$BOOKCLERK_PLUGIN_ARTIFACTS` or `target/plugin-artifacts`).
        #[arg(long, env = "BOOKCLERK_PLUGIN_ARTIFACTS")]
        dest: Option<std::path::PathBuf>,
    },
    /// Build + stage plugins, then `cargo run -p bookclerkd` with `BOOKCLERK_PLUGIN_DIRS` set.
    DevDaemon {
        /// Arguments forwarded to bookclerkd (after `--`).
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Build + stage plugins, then `cargo run -p bookclerk-cli` with `BOOKCLERK_PLUGIN_DIRS` set.
    DevCli {
        /// Arguments forwarded to bookclerk (after `--`).
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Build + stage plugins, then run the staged handshake integration test.
    TestStaged,
    /// Build release plugins and write per-crate archives + SHA256SUMS (current OS/arch).
    PackagePlugins {
        /// Output directory (default: `target/dist/plugins`).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Version segment in archive names (default: workspace version).
        #[arg(long)]
        version: Option<String>,
    },
    /// Build release host binaries and write a host bundle archive (current OS/arch).
    PackageHosts {
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        #[arg(long)]
        version: Option<String>,
    },
    /// Build hosts + bundle platform plugins (`sqlite`, `local`) for installers.
    PackagePlatform {
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        #[arg(long)]
        version: Option<String>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bookclerk-dev: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = workspace_root()?;
    match cli.command {
        Commands::BuildPlugins => plugins::build(&root, cli.release),
        Commands::StagePlugins { dest } => {
            let artifacts = dest.unwrap_or_else(|| default_artifacts(&root));
            plugins::stage(&root, cli.release, &artifacts)
        }
        Commands::DevDaemon { args } => dev_host(&root, cli.release, Host::Daemon, &args),
        Commands::DevCli { args } => dev_host(&root, cli.release, Host::Cli, &args),
        Commands::TestStaged => test_staged(&root, cli.release),
        Commands::PackagePlugins { out, version } => {
            let out = out.unwrap_or_else(|| root.join("target").join("dist").join("plugins"));
            package::package_plugins(
                &root,
                &out,
                version.as_deref().unwrap_or(workspace_version()),
            )
        }
        Commands::PackageHosts { out, version } => {
            let out = out.unwrap_or_else(|| root.join("target").join("dist"));
            package::package_hosts(
                &root,
                &out,
                version.as_deref().unwrap_or(workspace_version()),
            )
        }
        Commands::PackagePlatform { out, version } => {
            let out = out.unwrap_or_else(|| root.join("target").join("dist"));
            package::package_platform(
                &root,
                &out,
                version.as_deref().unwrap_or(workspace_version()),
            )
        }
    }
}

#[derive(Clone, Copy)]
enum Host {
    Daemon,
    Cli,
}

fn cargo(root: &Path) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root);
    cmd
}

fn build_jail(root: &Path, release: bool) -> Result<()> {
    let mut cmd = cargo(root);
    cmd.args(["build", "-p", "bookclerk-jail"]);
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().context("cargo build -p bookclerk-jail")?;
    if status.success() {
        Ok(())
    } else {
        bail!("cargo build -p bookclerk-jail exited with {status}");
    }
}

fn dev_host(root: &Path, release: bool, host: Host, host_args: &[String]) -> Result<()> {
    let artifacts = default_artifacts(root);
    plugins::stage(root, release, &artifacts)?;
    build_jail(root, release)?;

    let files_dir = default_files_dir();
    let package = match host {
        Host::Daemon => "bookclerkd",
        Host::Cli => "bookclerk-cli",
    };

    let mut cmd = cargo(root);
    cmd.args(["run", "-p", package]);
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--");
    cmd.args(host_args);
    cmd.env("BOOKCLERK_FILES_DIR", &files_dir);
    cmd.env("BOOKCLERK_PLUGIN_DIRS", &artifacts);
    cmd.env("BOOKCLERK_PLUGIN_ARTIFACTS", &artifacts);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .with_context(|| format!("cargo run -p {package}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("cargo run -p {package} exited with {status}");
    }
}

fn test_staged(root: &Path, release: bool) -> Result<()> {
    let artifacts = default_artifacts(root);
    plugins::stage(root, release, &artifacts)?;
    build_jail(root, release)?;

    let mut cmd = cargo(root);
    cmd.args([
        "test",
        "-p",
        "bookclerk-plugin-host",
        "--test",
        "staged_plugins",
    ]);
    if release {
        cmd.arg("--release");
    }
    cmd.env("BOOKCLERK_PLUGIN_ARTIFACTS", &artifacts);
    cmd.env(
        "BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT",
        std::env::var_os("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT").unwrap_or_else(|| "1".into()),
    );
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .context("cargo test -p bookclerk-plugin-host --test staged_plugins")?;
    if status.success() {
        Ok(())
    } else {
        bail!("staged plugin test exited with {status}");
    }
}
