//! [`cargo` alias dispatcher](../README.md) for the external-plugin dev workflow.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{bail, Context, Result};
use bookclerk_dev::{
    default_artifacts, default_files_dir, ensure_ui_dist, ensure_workerd_for_profile, package,
    plugins, reset_files_dir, workspace_root, workspace_version,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "bookclerk-dev",
    about = "Bookclerk dev workflow (cargo alias target)"
)]
/// Top-level clap parser for `cargo` aliases (`dev`, `build-app`, `reset`, …).
struct Cli {
    /// Use release profile for builds and host binaries.
    #[arg(long, global = true)]
    release: bool,
    #[command(subcommand)]
    /// Subcommand to run (build, stage, package, reset, or exec a host).
    command: Commands,
}

#[derive(Subcommand)]
/// Dev-workflow subcommands dispatched by the `cargo` aliases.
enum Commands {
    /// Build installer / guest packages selected by directory tier.
    ///
    /// At least one of `--platform`, `--optional`, `--examples` is required.
    BuildApp {
        /// `default-members` + guests under `plugins/platform/`.
        #[arg(long)]
        platform: bool,
        /// Guests under `plugins/optional/`.
        #[arg(long)]
        optional: bool,
        /// Guests under `examples/`.
        #[arg(long, env = "BOOKCLERK_DEV_EXAMPLES")]
        examples: bool,
        /// Print resolved Cargo package names (one per line) and exit.
        #[arg(long)]
        print: bool,
    },
    /// Stage **optional** and/or **example** guests to `target/plugin-artifacts`.
    ///
    /// Platform guests are installed into `$BOOKCLERK_FILES_DIR/plugins/` instead
    /// (see `cargo dev` / `install-platform`).
    StagePlugins {
        #[arg(long, env = "BOOKCLERK_PLUGIN_ARTIFACTS")]
        /// Staging directory for optional/example guests (default: `target/plugin-artifacts`).
        dest: Option<PathBuf>,
        #[arg(long)]
        /// When set, stage guests under `plugins/optional/`.
        optional: bool,
        #[arg(long, env = "BOOKCLERK_DEV_EXAMPLES")]
        /// When set, also include reference guests under `examples/`.
        examples: bool,
        #[arg(long)]
        /// Skip Cargo build and only install/stage already-built artifacts.
        skip_build: bool,
    },
    /// Install platform guests into `$BOOKCLERK_FILES_DIR/plugins/`.
    InstallPlatform {
        #[arg(long)]
        /// Skip Cargo build and only install/stage already-built artifacts.
        skip_build: bool,
    },
    /// Full platform build (`default-members` + platform guests), install, run bookclerkd.
    ///
    /// Pass `--optional` / `--examples` to also build and stage those tiers.
    Dev {
        /// Also build and stage optional guests under `plugins/optional/`.
        #[arg(long)]
        optional: bool,
        #[arg(long, env = "BOOKCLERK_DEV_EXAMPLES")]
        /// When set, also include reference guests under `examples/`.
        examples: bool,
        #[arg(long)]
        /// Skip Cargo build and only install/stage already-built artifacts.
        skip_build: bool,
        #[arg(last = true, allow_hyphen_values = true)]
        /// Extra argv forwarded to the host binary after install/stage.
        args: Vec<String>,
    },
    /// Deprecated alias for [`Commands::Dev`].
    DevDaemon {
        #[arg(long)]
        /// Also build and stage optional guests under `plugins/optional/`.
        optional: bool,
        #[arg(long, env = "BOOKCLERK_DEV_EXAMPLES")]
        /// When set, also include reference guests under `examples/`.
        examples: bool,
        #[arg(long)]
        /// Skip Cargo build and only install/stage already-built artifacts.
        skip_build: bool,
        #[arg(last = true, allow_hyphen_values = true)]
        /// Extra argv forwarded to the host binary after install/stage.
        args: Vec<String>,
    },
    /// Same as `dev` but runs the CLI binary (same full platform build).
    DevCli {
        #[arg(long)]
        /// Also build and stage optional guests under `plugins/optional/`.
        optional: bool,
        #[arg(long, env = "BOOKCLERK_DEV_EXAMPLES")]
        /// When set, also include reference guests under `examples/`.
        examples: bool,
        #[arg(long)]
        /// Skip Cargo build and only install/stage already-built artifacts.
        skip_build: bool,
        #[arg(last = true, allow_hyphen_values = true)]
        /// Extra argv forwarded to the host binary after install/stage.
        args: Vec<String>,
    },
    /// Stage optional + examples, then run the staged handshake integration test.
    TestStaged {
        #[arg(long)]
        /// Skip Cargo build and only install/stage already-built artifacts.
        skip_build: bool,
    },
    /// Build release **optional** plugins and write per-crate archives.
    PackagePlugins {
        #[arg(long)]
        /// Output directory for per-crate plugin archives (default: `target/dist/plugins`).
        out: Option<PathBuf>,
        #[arg(long)]
        /// Version stamped into archive names (default: workspace version).
        version: Option<String>,
    },
    /// Build release host binaries (+ jail, media-worker, workerd) archive.
    PackageHosts {
        #[arg(long)]
        /// Output directory for the host archive (default: `target/dist`).
        out: Option<PathBuf>,
        #[arg(long)]
        /// Version stamped into the archive name (default: workspace version).
        version: Option<String>,
    },
    /// Build hosts + platform guests (`sqlite`, `local`) installer archive.
    PackagePlatform {
        #[arg(long)]
        /// Output directory for the platform installer archive (default: `target/dist`).
        out: Option<PathBuf>,
        #[arg(long)]
        /// Version stamped into the archive name (default: workspace version).
        version: Option<String>,
    },
    /// Download/update the pinned Cloudflare `workerd` binary into `target/<profile>/`.
    EnsureWorkerd,
    /// Wipe local app data under `$BOOKCLERK_FILES_DIR` (DB, config, keys, plugins).
    ///
    /// Does **not** remove Cargo `target/` or `.cargo-home/` (use `cargo clean` /
    /// delete `.cargo-home` for those). Requires `--yes`.
    Reset {
        /// When set, allows `cargo reset` to delete `BookclerkFiles/` without a prompt.
        #[arg(long)]
        yes: bool,
        /// Also remove `target/plugin-artifacts`.
        #[arg(long)]
        artifacts: bool,
        /// Override files dir (default: env or `<workspace>/BookclerkFiles`).
        #[arg(long, env = "BOOKCLERK_FILES_DIR")]
        files_dir: Option<PathBuf>,
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

/// Parses argv and dispatches the selected cargo-alias workflow.
fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = workspace_root()?;
    match cli.command {
        Commands::BuildApp {
            platform,
            optional,
            examples,
            print,
        } => {
            if !platform && !optional && !examples {
                bail!("build-app requires --platform and/or --optional and/or --examples");
            }
            let sel = plugins::BuildSelection {
                platform,
                optional,
                examples,
            };
            let pkgs = plugins::packages_for(&root, sel)?;
            if pkgs.is_empty() {
                bail!("build-app selection resolved to no packages");
            }
            if print {
                for pkg in &pkgs {
                    println!("{pkg}");
                }
                return Ok(());
            }
            plugins::build_selection(&root, cli.release, sel)?;
            if platform {
                ensure_ui_dist(&root)?;
                let bin = ensure_workerd_for_profile(&root, cli.release)?;
                eprintln!("workerd ready: {}", bin.display());
            }
            Ok(())
        }
        Commands::StagePlugins {
            dest,
            optional,
            examples,
            skip_build,
        } => {
            let artifacts = dest.unwrap_or_else(|| default_artifacts(&root));
            plugins::stage_plugins(
                &root,
                &artifacts,
                cli.release,
                optional,
                examples,
                skip_build,
            )
        }
        Commands::InstallPlatform { skip_build } => {
            if !skip_build {
                plugins::build_selection(
                    &root,
                    cli.release,
                    plugins::BuildSelection {
                        platform: true,
                        ..Default::default()
                    },
                )?;
            }
            let _ = ensure_workerd_for_profile(&root, cli.release)?;
            plugins::install_platform(&root, &default_files_dir(), cli.release)
        }
        Commands::EnsureWorkerd => {
            let bin = ensure_workerd_for_profile(&root, cli.release)?;
            println!("{}", bin.display());
            Ok(())
        }
        Commands::Dev {
            optional,
            examples,
            skip_build,
            args,
        } => dev_host(
            &root,
            cli.release,
            Host::Daemon,
            &args,
            optional,
            examples,
            skip_build,
        ),
        Commands::DevDaemon {
            optional,
            examples,
            skip_build,
            args,
        } => {
            eprintln!("bookclerk-dev: `dev-daemon` is deprecated; use `cargo dev` (same behavior)");
            dev_host(
                &root,
                cli.release,
                Host::Daemon,
                &args,
                optional,
                examples,
                skip_build,
            )
        }
        Commands::DevCli {
            optional,
            examples,
            skip_build,
            args,
        } => dev_host(
            &root,
            cli.release,
            Host::Cli,
            &args,
            optional,
            examples,
            skip_build,
        ),
        Commands::TestStaged { skip_build } => test_staged(&root, cli.release, skip_build),
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
        Commands::Reset {
            yes,
            artifacts,
            files_dir,
        } => {
            if !yes {
                bail!("refusing to wipe without --yes (see `cargo reset --help`)");
            }
            let files = files_dir.unwrap_or_else(default_files_dir);
            reset_files_dir(&files)?;
            eprintln!("reset files dir {}", files.display());
            if artifacts {
                let dest = default_artifacts(&root);
                if dest.exists() {
                    std::fs::remove_dir_all(&dest)
                        .with_context(|| format!("remove {}", dest.display()))?;
                }
                eprintln!("removed {}", dest.display());
            }
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
/// Which host binary `dev` / `dev-cli` execs after the platform build.
enum Host {
    /// Long-running `bookclerkd` control-plane host.
    Daemon,
    /// Headless `bookclerk` CLI host.
    Cli,
}

impl Host {
    /// Cargo binary name for this host (`bookclerkd` or `bookclerk`).
    fn binary_name(self) -> &'static str {
        match self {
            Host::Daemon => "bookclerkd",
            Host::Cli => "bookclerk",
        }
    }
}

/// `target/` profile directory name (`release` or `debug`).
fn profile_dir(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

/// Prepends `target/<profile>` so jail/media-worker/workerd resolve beside hosts.
fn prepend_helper_path(root: &Path, release: bool, cmd: &mut Command) {
    let helper_dir = root.join("target").join(profile_dir(release));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![helper_dir];
    paths.extend(std::env::split_paths(&path));
    cmd.env("PATH", std::env::join_paths(paths).unwrap_or(path));
}

/// Builds a `cargo` command rooted at the workspace (honors `$CARGO`).
fn cargo(root: &Path) -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root);
    cmd
}

/// Builds/installs platform guests, optionally stages extras, then execs the host.
fn dev_host(
    root: &Path,
    release: bool,
    host: Host,
    host_args: &[String],
    optional: bool,
    examples: bool,
    skip_build: bool,
) -> Result<()> {
    let files_dir = default_files_dir();
    let artifacts = default_artifacts(root);

    if !skip_build {
        plugins::build_selection(
            root,
            release,
            plugins::BuildSelection {
                platform: true,
                optional,
                examples,
            },
        )?;
    }
    // Vite output is not a Cargo artifact: refresh even with --skip-build when
    // `ui/src` is newer than `ui/dist` (a `git pull` otherwise serves a stale SPA).
    ensure_ui_dist(root)?;
    let _ = ensure_workerd_for_profile(root, release)?;
    plugins::install_platform(root, &files_dir, release)?;
    if optional || examples {
        plugins::stage_plugins(root, &artifacts, release, optional, examples, true)?;
    }

    let bin = root
        .join("target")
        .join(profile_dir(release))
        .join(host.binary_name());
    if !bin.is_file() {
        bail!(
            "{} missing; run without --skip-build (or `cargo build-app --platform`)",
            bin.display()
        );
    }

    let mut cmd = Command::new(&bin);
    cmd.current_dir(root);
    cmd.args(host_args);
    cmd.env("BOOKCLERK_FILES_DIR", &files_dir);
    if optional || examples {
        cmd.env("BOOKCLERK_PLUGIN_DIRS", &artifacts);
        cmd.env("BOOKCLERK_PLUGIN_ARTIFACTS", &artifacts);
    }
    prepend_helper_path(root, release, &mut cmd);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .status()
        .with_context(|| format!("exec {}", bin.display()))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{} exited with {status}", bin.display());
    }
}

/// Stages optional+example guests and runs the host handshake integration test.
fn test_staged(root: &Path, release: bool, skip_build: bool) -> Result<()> {
    let files_dir = default_files_dir();
    let artifacts = default_artifacts(root);
    if !skip_build {
        plugins::build_selection(
            root,
            release,
            plugins::BuildSelection {
                platform: true,
                optional: true,
                examples: true,
            },
        )?;
    }
    let _ = ensure_workerd_for_profile(root, release)?;
    plugins::install_platform(root, &files_dir, release)?;
    plugins::stage_plugins(root, &artifacts, release, true, true, true)?;

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
    cmd.env("BOOKCLERK_FILES_DIR", &files_dir);
    cmd.env(
        "BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT",
        std::env::var_os("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT").unwrap_or_else(|| "1".into()),
    );
    prepend_helper_path(root, release, &mut cmd);
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
