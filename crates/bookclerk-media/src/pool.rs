//! Bounded pool of confined media worker processes.
//!
//! Every codec operation used to run on tokio's blocking thread pool, inside
//! whichever process owned the acquire. That process also holds the master data
//! encryption key and an open handle to `library.db`, and the codecs it links —
//! LAME (`mp3lame-sys`) and FDK-AAC (`fdk-aac-sys`) — are C libraries parsing
//! attacker-influenced audio.
//!
//! Jobs now run in short-lived child processes confined to the paths their job
//! declared before touching any media. That buys three things at once: the
//! codecs cannot reach the key material, a codec crash fails one book instead
//! of the daemon, and concurrency becomes an explicit bound rather than however
//! many blocking threads tokio happened to grow.
//!
//! On Linux and macOS the worker confines *itself* (Landlock / Seatbelt) at
//! startup. On Windows there is no self-confinement primitive, so the pool
//! launches the worker through `bookclerk-jail`, which `CreateProcess`es it
//! into an AppContainer with an ACL allowlist derived from the same job paths.
//!
//! Workers are per-job rather than long-lived on purpose. Filesystem
//! confinement is irreversible and process-wide, so a reused worker would need
//! a jail wide enough for every job it might later receive. Media operations
//! run for seconds to minutes, which makes the few milliseconds of process
//! spawn irrelevant next to a tight, per-job allowlist.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};

use bookclerk_sandbox::{Enforcement, NetPolicy, Spec, SPEC_ENV};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::error::{MediaError, Result};
use crate::job::{MediaJob, MediaJobOutput, MediaJobReply};

/// Environment variable naming the worker binary explicitly.
pub const WORKER_BIN_ENV: &str = "BOOKCLERK_MEDIA_WORKER";
/// Environment variable the pool sets on each worker to select its failure mode.
pub const WORKER_ENFORCEMENT_ENV: &str = "BOOKCLERK_MEDIA_WORKER_ENFORCEMENT";
/// File name of the worker binary when it sits beside the host executable.
pub const WORKER_BIN_NAME: &str = "bookclerk-media-worker";
/// Environment variable naming the jail launcher (shared with plugin hosts).
pub const JAIL_BIN_ENV: &str = "BOOKCLERK_PLUGIN_JAIL";
/// File name of the jail launcher when it sits beside the host / worker.
pub const JAIL_BIN_NAME: &str = "bookclerk-jail";

/// How strictly a worker must be confined before it will process a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Confinement {
    /// Refuse to run the job unless the worker confined itself. Production
    /// default: a host that cannot sandbox must not silently decode untrusted
    /// media next to the encryption key.
    #[default]
    Required,
    /// Confine where the platform allows and log what did not engage.
    BestEffort,
    /// Run jobs on a blocking thread in this process. Intended for development
    /// and for the crate's own algorithm tests.
    Off,
}

impl Confinement {
    /// Parse the value carried in [`WORKER_ENFORCEMENT_ENV`].
    #[must_use]
    pub fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => Some(Self::Required),
            "best-effort" | "best_effort" => Some(Self::BestEffort),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// The value to pass through [`WORKER_ENFORCEMENT_ENV`].
    #[must_use]
    pub fn as_env_value(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::BestEffort => "best-effort",
            Self::Off => "off",
        }
    }
}

/// Pool sizing and isolation settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaPoolConfig {
    /// Maximum jobs running at once. Zero means "derive from available
    /// parallelism".
    pub workers: usize,
    /// How strictly workers must be confined.
    pub confinement: Confinement,
    /// Explicit worker binary path. When absent the pool looks at
    /// [`WORKER_BIN_ENV`] and then beside the current executable.
    pub worker_bin: Option<PathBuf>,
}

impl MediaPoolConfig {
    /// Settings with `workers: 0` replaced by the count it resolves to.
    ///
    /// Lets two spellings of the same pool compare equal, so reloading a config
    /// that only respelled the worker count does not look like a change.
    #[must_use]
    fn normalized(mut self) -> Self {
        if self.workers == 0 {
            self.workers = default_worker_count();
        }
        self
    }
}

impl From<&bookclerk_config::MediaConfig> for MediaPoolConfig {
    fn from(config: &bookclerk_config::MediaConfig) -> Self {
        Self {
            workers: config.workers,
            confinement: match config.isolation {
                bookclerk_config::Isolation::Required => Confinement::Required,
                bookclerk_config::Isolation::BestEffort => Confinement::BestEffort,
                bookclerk_config::Isolation::Off => Confinement::Off,
            },
            worker_bin: config.worker_bin.clone(),
        }
    }
}

/// Worker binary plus optional spawn-time jail launcher.
#[derive(Debug, Clone)]
struct WorkerLaunch {
    bin: PathBuf,
    /// When set, the pool runs `jail -- worker` so AppContainer can be applied
    /// at CreateProcess (Windows). Absent on platforms that self-confine.
    jail: Option<PathBuf>,
}

/// Where a pool sends its jobs.
#[derive(Debug)]
enum Runner {
    /// Spawn the worker binary (optionally through `bookclerk-jail`) per job.
    Worker(WorkerLaunch),
    /// Run on a blocking thread in this process, unconfined. Only reached when
    /// the operator turned isolation off.
    InProcess,
    /// Refuse every job, carrying the reason. Reached when isolation was asked
    /// for but the worker could not be resolved; running the job anyway would
    /// mean decoding untrusted media next to the master key.
    Refuse(String),
}

/// Bounded pool of confined media workers.
#[derive(Debug)]
pub struct MediaPool {
    permits: Arc<Semaphore>,
    /// Normalized settings this pool was built from, kept so a later config can
    /// be compared against what is actually running.
    config: MediaPoolConfig,
    runner: Runner,
}

impl Drop for MediaPool {
    /// A pool is dropped when the last handle to it goes away, which for a pool
    /// retired by a config reload is exactly the moment its final job finished.
    /// Logging here makes the end of a drain visible without polling for it.
    fn drop(&mut self) {
        tracing::debug!("media pool retired: {}", self.summary());
    }
}

impl MediaPool {
    /// Build a pool from `config`, resolving the worker binary once.
    ///
    /// Resolution failures are recorded rather than returned, so a
    /// misconfigured host still starts and reports the problem through
    /// [`summary`](Self::summary) and its logs. The refusal happens per job.
    #[must_use]
    pub fn new(config: MediaPoolConfig) -> Self {
        let config = config.normalized();

        let runner = match config.confinement {
            Confinement::Off => Runner::InProcess,
            // BestEffort covers layers the *platform* cannot enforce, so a
            // missing primitive is fine there; a missing worker binary is not,
            // since that is a packaging error and would put codecs back in the
            // host process either way.
            mode => match resolve_runner(mode, config.worker_bin.as_deref()) {
                Ok(launch) => Runner::Worker(launch),
                Err(detail) => {
                    tracing::error!(
                        confinement = mode.as_env_value(),
                        "{detail}; media jobs will be refused"
                    );
                    Runner::Refuse(detail)
                }
            },
        };

        Self {
            permits: Arc::new(Semaphore::new(config.workers)),
            config,
            runner,
        }
    }

    /// A pool that runs every job on a blocking thread in this process.
    #[must_use]
    pub fn in_process() -> Self {
        Self::new(MediaPoolConfig {
            confinement: Confinement::Off,
            ..MediaPoolConfig::default()
        })
    }

    /// Maximum jobs this pool will run at once.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.config.workers
    }

    /// Whether jobs run in a separate, confined process.
    #[must_use]
    pub fn is_isolated(&self) -> bool {
        matches!(self.runner, Runner::Worker(_))
    }

    /// Whether this pool will refuse every job because isolation is required
    /// but unavailable.
    #[must_use]
    pub fn is_refusing(&self) -> bool {
        matches!(self.runner, Runner::Refuse(_))
    }

    /// Resolved worker binary, when one was found.
    #[must_use]
    pub fn worker_bin(&self) -> Option<&PathBuf> {
        match &self.runner {
            Runner::Worker(launch) => Some(&launch.bin),
            Runner::InProcess | Runner::Refuse(_) => None,
        }
    }

    /// One-line summary for startup logs and `bookclerk doctor`.
    #[must_use]
    pub fn summary(&self) -> String {
        match &self.runner {
            Runner::Worker(launch) => match &launch.jail {
                Some(jail) => format!(
                    "media pool: {} workers, confinement={}, worker={}, jail={}",
                    self.config.workers,
                    self.config.confinement.as_env_value(),
                    launch.bin.display(),
                    jail.display()
                ),
                None => format!(
                    "media pool: {} workers, confinement={}, worker={}",
                    self.config.workers,
                    self.config.confinement.as_env_value(),
                    launch.bin.display()
                ),
            },
            Runner::InProcess => format!(
                "media pool: {} workers, in-process (no confinement)",
                self.config.workers
            ),
            Runner::Refuse(detail) => format!(
                "media pool: unusable, jobs will be refused ({detail}); set \
                 media.isolation = \"best-effort\" or \"off\" to accept \
                 unconfined codecs"
            ),
        }
    }

    /// Run `job`, waiting for a free slot first.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::NotIsolated`] when confinement is required but no
    /// worker binary was found, [`MediaError::Worker`] when the worker cannot
    /// be started, exits without a reply, or returns something unparseable,
    /// and propagates the job's own error otherwise.
    pub async fn run(&self, job: MediaJob) -> Result<MediaJobOutput> {
        let label = job.label();

        // Checked before the output directory is created so a refusing pool
        // does not litter the destination with empty folders.
        if let Runner::Refuse(detail) = &self.runner {
            return Err(MediaError::NotIsolated {
                job: label,
                detail: detail.clone(),
            });
        }

        job.prepare_output_dirs()?;

        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|err| MediaError::Worker {
                job: label,
                detail: format!("pool closed: {err}"),
            })?;

        match &self.runner {
            Runner::Worker(launch) => self.run_in_worker(launch.clone(), job).await,
            Runner::InProcess => run_in_process(job).await,
            Runner::Refuse(detail) => Err(MediaError::NotIsolated {
                job: label,
                detail: detail.clone(),
            }),
        }
    }

    async fn run_in_worker(&self, launch: WorkerLaunch, job: MediaJob) -> Result<MediaJobOutput> {
        let label = job.label();
        let request = serde_json::to_vec(&job).map_err(|err| MediaError::Worker {
            job: label,
            detail: format!("could not serialize job: {err}"),
        })?;

        let mut command = match &launch.jail {
            Some(jail) => {
                let mut command = tokio::process::Command::new(jail);
                command.arg("--").arg(&launch.bin);
                command
            }
            None => tokio::process::Command::new(&launch.bin),
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            // The jail keeps the codecs away from the host's files; the
            // environment is the other way they could read them. A worker gets
            // its job over stdin and needs no Bookclerk configuration, so
            // BOOKCLERK_AUTH_PASSWORD, BOOKCLERK_FILES_DIR, operator tokens and
            // cloud credentials are dropped rather than inherited.
            .env_clear();
        for (key, value) in std::env::vars() {
            if worker_env_allowed(&key) {
                command.env(key, value);
            }
        }
        command.env(
            WORKER_ENFORCEMENT_ENV,
            self.config.confinement.as_env_value(),
        );
        if launch.jail.is_some() {
            let spec = media_job_spec(&job, self.config.confinement);
            command.env(
                SPEC_ENV,
                serde_json::to_string(&spec).map_err(|err| MediaError::Worker {
                    job: label,
                    detail: format!("could not encode jail spec: {err}"),
                })?,
            );
        }

        let spawned = match &launch.jail {
            Some(jail) => format!("{} -- {}", jail.display(), launch.bin.display()),
            None => launch.bin.display().to_string(),
        };
        let mut child = command.spawn().map_err(|err| MediaError::Worker {
            job: label,
            detail: format!("could not spawn {spawned}: {err}"),
        })?;

        // Close stdin after the single request so the worker sees EOF and does
        // not wait for a second job.
        {
            let mut stdin = child.stdin.take().ok_or_else(|| MediaError::Worker {
                job: label,
                detail: "worker stdin unavailable".to_string(),
            })?;
            stdin
                .write_all(&request)
                .await
                .map_err(|err| MediaError::Worker {
                    job: label,
                    detail: format!("could not send job: {err}"),
                })?;
            stdin.shutdown().await.ok();
        }

        let mut stdout = child.stdout.take().ok_or_else(|| MediaError::Worker {
            job: label,
            detail: "worker stdout unavailable".to_string(),
        })?;
        let mut reply = Vec::new();
        stdout
            .read_to_end(&mut reply)
            .await
            .map_err(|err| MediaError::Worker {
                job: label,
                detail: format!("could not read reply: {err}"),
            })?;

        let status = child.wait().await.map_err(|err| MediaError::Worker {
            job: label,
            detail: format!("could not await worker: {err}"),
        })?;

        interpret_reply(label, &reply, status.success(), &status.to_string())
    }
}

/// Decide what a worker's reply means, given how its process ended.
///
/// Split out from [`MediaPool::run_in_worker`] so the combinations can be
/// tested without arranging a process that dies in a particular way.
fn interpret_reply(
    job: &'static str,
    reply: &[u8],
    exited_cleanly: bool,
    status: &str,
) -> Result<MediaJobOutput> {
    if reply.is_empty() {
        return Err(MediaError::Worker {
            job,
            detail: format!("worker exited with {status} before replying"),
        });
    }

    match serde_json::from_slice::<MediaJobReply>(reply) {
        // The worker writes its reply and returns SUCCESS with nothing in
        // between, so there is no path where it legitimately claims success and
        // then exits some other way. Reaching here means the process was killed
        // after replying (OOM, a signal) or died in teardown, and the output
        // cannot be vouched for — fail rather than record a book that may be
        // truncated.
        Ok(MediaJobReply::Ok(_)) if !exited_cleanly => Err(MediaError::Worker {
            job,
            detail: format!("worker reported success but exited with {status}"),
        }),
        Ok(MediaJobReply::Ok(output)) => Ok(output),
        // A failure reply is already accompanied by a FAILURE exit, so the
        // status adds nothing; the worker's own message is the useful part.
        Ok(MediaJobReply::Err { message }) => Err(MediaError::Native(message)),
        Err(err) => Err(MediaError::Worker {
            job,
            detail: format!(
                "unparseable reply ({err}); worker exited with {status}: {}",
                String::from_utf8_lossy(reply).trim()
            ),
        }),
    }
}

impl Default for MediaPool {
    fn default() -> Self {
        Self::new(MediaPoolConfig::default())
    }
}

async fn run_in_process(job: MediaJob) -> Result<MediaJobOutput> {
    let label = job.label();
    tokio::task::spawn_blocking(move || job.run())
        .await
        .map_err(|err| MediaError::Worker {
            job: label,
            detail: format!("in-process job panicked: {err}"),
        })?
}

/// Default concurrency: one job per core, capped so a large machine does not
/// spawn dozens of memory-hungry encoders at once.
fn default_worker_count() -> usize {
    const MAX_DEFAULT_WORKERS: usize = 8;
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .clamp(1, MAX_DEFAULT_WORKERS)
}

/// Decide where jobs will run, or explain why they cannot run at all.
///
/// Both failures are reported at construction so they show up in the startup
/// summary. Otherwise a host that cannot confine would look healthy until the
/// first acquire, then fail every book with an error from a child process.
fn resolve_runner(
    confinement: Confinement,
    configured: Option<&Path>,
) -> std::result::Result<WorkerLaunch, String> {
    let caps = bookclerk_sandbox::capabilities();
    if confinement == Confinement::Required && !caps.can_confine_guest() {
        return Err(format!(
            "this host cannot confine a worker process ({}) [{}]",
            caps.detail, caps.backend
        ));
    }
    let bin = resolve_worker_bin(configured)?;
    let jail = if needs_spawn_jail(&caps) {
        match resolve_jail_bin(Some(&bin)) {
            Ok(jail) => Some(jail),
            Err(err) if confinement == Confinement::Required => {
                return Err(format!(
                    "{err}; Windows AppContainer confinement requires {JAIL_BIN_NAME} \
                     beside the worker (or {JAIL_BIN_ENV})"
                ));
            }
            Err(err) => {
                tracing::warn!(
                    %err,
                    "media pool: spawn-time jail unavailable; workers will run unconfined \
                     under best-effort"
                );
                None
            }
        }
    } else {
        None
    };
    Ok(WorkerLaunch { bin, jail })
}

/// Windows (and any future spawn-only backend): self-confine is unavailable, so
/// the pool must launch through `bookclerk-jail`.
fn needs_spawn_jail(caps: &bookclerk_sandbox::Capabilities) -> bool {
    caps.spawn_filesystem && !caps.filesystem
}

/// Build the jail [`Spec`] for one media job (Windows AppContainer path).
fn media_job_spec(job: &MediaJob, confinement: Confinement) -> Spec {
    let enforcement = match confinement {
        Confinement::Required => Enforcement::Required,
        Confinement::BestEffort => Enforcement::BestEffort,
        Confinement::Off => Enforcement::Disabled,
    };
    Spec {
        label: format!("media-worker:{}", job.label()),
        reads: job.read_paths(),
        writes: job.write_dirs(),
        net: NetPolicy::Deny,
        // The launcher CreateProcess/es the worker binary.
        allow_exec: true,
        system_paths: true,
        enforcement,
        preserve_fds: vec![],
    }
}

/// Locate `bookclerk-jail`: env, beside the worker, then beside the current exe.
fn resolve_jail_bin(worker: Option<&Path>) -> std::result::Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(JAIL_BIN_ENV) {
        return check_bin(Path::new(&path), JAIL_BIN_ENV);
    }
    let name = format!("{JAIL_BIN_NAME}{}", std::env::consts::EXE_SUFFIX);
    if let Some(worker) = worker {
        if let Some(dir) = worker.parent() {
            let candidate = dir.join(&name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let exe = std::env::current_exe()
        .map_err(|err| format!("could not locate the current executable: {err}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    if dir.join(&name).is_file() {
        return Ok(dir.join(&name));
    }
    if dir.file_name().is_some_and(|last| last == "deps") {
        if let Some(parent) = dir.parent() {
            if parent.join(&name).is_file() {
                return Ok(parent.join(name));
            }
        }
    }
    Err(format!(
        "{JAIL_BIN_NAME} not found beside the worker/host and {JAIL_BIN_ENV} is unset"
    ))
}

/// Locate the worker binary: the configured path, then [`WORKER_BIN_ENV`], then
/// beside the current executable, which covers both an installed layout and
/// `target/debug`.
///
/// Every branch checks that the candidate is really there. Handing an
/// unresolvable path to the pool would turn a packaging mistake into an
/// unconfined encode.
fn resolve_worker_bin(configured: Option<&Path>) -> std::result::Result<PathBuf, String> {
    if let Some(path) = configured {
        // Config folds BOOKCLERK_MEDIA_WORKER into `worker_bin` before the pool
        // sees it, so the two are indistinguishable here. Name both rather than
        // sending someone to a config.toml that never mentioned this path.
        return check_worker_bin(path, "media.worker_bin (or BOOKCLERK_MEDIA_WORKER)");
    }
    if let Some(path) = std::env::var_os(WORKER_BIN_ENV) {
        return check_worker_bin(Path::new(&path), WORKER_BIN_ENV);
    }
    let exe = std::env::current_exe()
        .map_err(|err| format!("could not locate the current executable: {err}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let candidate = dir.join(format!("{WORKER_BIN_NAME}{}", std::env::consts::EXE_SUFFIX));
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(format!(
        "{WORKER_BIN_NAME} not found in {} and {WORKER_BIN_ENV} is unset",
        dir.display()
    ))
}

fn check_worker_bin(path: &Path, source: &str) -> std::result::Result<PathBuf, String> {
    check_bin(path, source)
}

fn check_bin(path: &Path, source: &str) -> std::result::Result<PathBuf, String> {
    if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "{source} points at {}, which is not a file",
            path.display()
        ))
    }
}

/// Environment keys a media worker may inherit.
///
/// A strict allowlist, and a shorter one than the plugin host's: a worker
/// decodes the files its job named and talks over stdio, so it needs no `PATH`
/// (it is executed by absolute path), no `HOME`, and nothing from Bookclerk's
/// own configuration. What remains either changes how the C codecs behave or
/// makes a crash diagnosable.
fn worker_env_allowed(key: &str) -> bool {
    const ALLOW: &[&str] = &[
        // Windows resolves system DLLs through these; the loader fails without
        // them. Ignored elsewhere.
        "SystemRoot",
        "SystemDrive",
        "windir",
        // Locale and timezone reach libc formatting and metadata timestamps.
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        // Turns a codec panic into something reportable.
        "RUST_BACKTRACE",
    ];
    ALLOW
        .iter()
        .any(|allowed| key.eq_ignore_ascii_case(allowed))
}

/// The process-wide pool.
///
/// Behind a lock rather than a `OnceLock` so a config reload can replace it.
/// Callers take an `Arc`, which is what makes the replacement safe: see
/// [`replace_pool`].
static POOL: RwLock<Option<Arc<MediaPool>>> = RwLock::new(None);

fn read_pool() -> std::sync::RwLockReadGuard<'static, Option<Arc<MediaPool>>> {
    // A panic in a `[media]` code path should not take every later media job
    // with it; the data behind this lock is a single `Arc`, so there is no
    // torn state to protect against.
    POOL.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_pool() -> std::sync::RwLockWriteGuard<'static, Option<Arc<MediaPool>>> {
    POOL.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Install the process-wide pool if none is installed yet.
///
/// # Errors
///
/// Returns the rejected pool when one was already installed. Use
/// [`replace_pool`] to swap a pool that is already running.
pub fn init_pool(pool: MediaPool) -> std::result::Result<(), MediaPool> {
    let mut guard = write_pool();
    if guard.is_some() {
        return Err(pool);
    }
    *guard = Some(Arc::new(pool));
    Ok(())
}

/// Swap in `pool`, returning the one it replaced.
///
/// Safe to call with jobs in flight, because [`pool`] hands out an `Arc` and a
/// job holds it for its whole duration. The moment this returns, the outgoing
/// pool is unreachable, so it can only drain: its permits are released as its
/// jobs finish and it is dropped after the last one. Nothing is interrupted and
/// nothing waits.
///
/// In-flight jobs keep the isolation they started with, which is the only
/// possible answer — a worker's confinement is applied inside the child process
/// at spawn and cannot be changed afterwards.
///
/// While the outgoing pool drains, total codec concurrency can briefly reach the
/// old pool's in-flight count plus the new pool's limit. That overshoot only
/// shrinks, since the old pool never admits another job.
pub fn replace_pool(pool: MediaPool) -> Option<Arc<MediaPool>> {
    write_pool().replace(Arc::new(pool))
}

/// Build the pool from `[media]` config and install it, logging what it will do.
///
/// Safe to call more than once, which is what a config reload does. An
/// unchanged `[media]` leaves the running pool alone; a changed one swaps in a
/// new pool for subsequent jobs and lets the old one drain. See
/// [`replace_pool`].
pub fn init_pool_from_config(config: &bookclerk_config::MediaConfig) {
    let requested = MediaPoolConfig::from(config).normalized();

    let installed = read_pool().as_ref().map(Arc::clone);
    if let Some(installed) = installed {
        if installed.config == requested {
            tracing::debug!("media pool already installed; keeping the existing one");
            return;
        }
        let pool = MediaPool::new(requested);
        let summary = pool.summary();
        let retiring = installed.summary();
        drop(installed);
        replace_pool(pool);
        tracing::info!("[media] changed; new jobs use {summary}");
        tracing::info!("draining previous pool: {retiring}");
        return;
    }

    let pool = MediaPool::new(requested);
    let summary = pool.summary();
    if init_pool(pool).is_err() {
        // Lost a race with another installer; theirs is authoritative.
        tracing::debug!("media pool already installed; keeping the existing one");
        return;
    }
    tracing::info!("{summary}");
}

/// The process-wide pool, creating a default one if startup never installed it.
///
/// Returns an owned handle on purpose. Holding it for the length of a job is
/// what lets a reload swap the pool without disturbing work already running.
pub fn pool() -> Arc<MediaPool> {
    if let Some(pool) = read_pool().as_ref() {
        return Arc::clone(pool);
    }
    let mut guard = write_pool();
    Arc::clone(guard.get_or_insert_with(|| Arc::new(MediaPool::default())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confinement_env_values_round_trip() {
        for mode in [
            Confinement::Required,
            Confinement::BestEffort,
            Confinement::Off,
        ] {
            assert_eq!(
                Confinement::from_env_value(mode.as_env_value()),
                Some(mode),
                "round trip failed for {mode:?}"
            );
        }
        assert_eq!(Confinement::from_env_value("nonsense"), None);
    }

    #[test]
    fn confinement_parsing_is_forgiving_about_case_and_separators() {
        assert_eq!(
            Confinement::from_env_value("  BEST_EFFORT "),
            Some(Confinement::BestEffort)
        );
    }

    #[test]
    fn default_worker_count_is_bounded() {
        let count = default_worker_count();
        assert!((1..=8).contains(&count), "unexpected worker count {count}");
    }

    #[test]
    fn in_process_pool_reports_no_isolation() {
        let pool = MediaPool::in_process();
        assert!(!pool.is_isolated());
        assert!(pool.worker_bin().is_none());
        assert!(pool.summary().contains("in-process"));
    }

    #[test]
    fn explicit_worker_count_is_respected() {
        let pool = MediaPool::new(MediaPoolConfig {
            workers: 3,
            confinement: Confinement::Off,
            worker_bin: None,
        });
        assert_eq!(pool.capacity(), 3);
    }

    /// A missing worker binary used to log a warning and fall through to
    /// in-process execution, which quietly gave `required` the same behaviour
    /// as `off`. The whole point of the pool is that codecs never run beside
    /// the master key, so an unresolvable worker has to fail the job.
    #[tokio::test]
    async fn a_missing_worker_binary_refuses_jobs_instead_of_running_them_unconfined() {
        let dir = tempfile::tempdir().expect("tempdir");
        for confinement in [Confinement::Required, Confinement::BestEffort] {
            let pool = MediaPool::new(MediaPoolConfig {
                workers: 1,
                confinement,
                worker_bin: Some(dir.path().join("no-such-worker")),
            });
            assert!(pool.is_refusing(), "{confinement:?} should refuse");
            assert!(!pool.is_isolated());

            let output = dir.path().join("nested/out.mp3");
            let err = pool
                .run(MediaJob::EncodeMp3 {
                    input: dir.path().join("in.m4b"),
                    output: output.clone(),
                    lame: Box::default(),
                    max_sample_rate: None,
                })
                .await
                .expect_err("job should be refused");
            assert!(
                matches!(err, MediaError::NotIsolated { .. }),
                "expected a refusal, got {err}"
            );
            assert!(
                !output.parent().expect("parent").exists(),
                "a refused job should not create its output directory"
            );
        }
    }

    #[test]
    fn turning_isolation_off_still_runs_in_process() {
        let pool = MediaPool::new(MediaPoolConfig {
            workers: 1,
            confinement: Confinement::Off,
            worker_bin: None,
        });
        assert!(!pool.is_refusing());
        assert!(pool.summary().contains("in-process"));
    }

    #[test]
    fn a_refusing_pool_says_how_to_proceed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = MediaPool::new(MediaPoolConfig {
            workers: 1,
            confinement: Confinement::Required,
            worker_bin: Some(dir.path().join("no-such-worker")),
        });
        let summary = pool.summary();
        assert!(summary.contains("refused"), "{summary}");
        assert!(summary.contains("media.isolation"), "{summary}");
    }

    fn ok_reply(output: &str) -> Vec<u8> {
        serde_json::to_vec(&MediaJobReply::Ok(MediaJobOutput::File {
            output: PathBuf::from(output),
        }))
        .expect("serialize reply")
    }

    /// A worker that is killed after writing its reply — OOM, a signal, a crash
    /// in teardown — would otherwise be recorded as a finished book, because the
    /// reply on stdout says so. The exit status is the only evidence that
    /// something went wrong, so it has to be consulted.
    #[test]
    fn a_success_reply_from_a_worker_that_died_is_not_a_success() {
        let err = interpret_reply("encode_mp3", &ok_reply("/out/book.mp3"), false, "signal: 9")
            .expect_err("a killed worker cannot vouch for its output");
        assert!(
            matches!(err, MediaError::Worker { .. }),
            "expected a worker failure, got {err}"
        );
        assert!(err.to_string().contains("signal: 9"), "{err}");
    }

    #[test]
    fn a_success_reply_from_a_clean_exit_is_a_success() {
        let output = interpret_reply(
            "encode_mp3",
            &ok_reply("/out/book.mp3"),
            true,
            "exit status: 0",
        )
        .expect("clean success");
        assert_eq!(output.output(), Some(Path::new("/out/book.mp3")));
    }

    /// The worker exits FAILURE for its own error replies, so the non-zero
    /// status there is expected and must not mask the codec's message.
    #[test]
    fn a_failure_reply_keeps_the_workers_own_message() {
        let reply = serde_json::to_vec(&MediaJobReply::Err {
            message: "not an audio file".to_string(),
        })
        .expect("serialize reply");
        let err = interpret_reply("package_m4b", &reply, false, "exit status: 1")
            .expect_err("a failure reply is a failure");
        assert!(err.to_string().contains("not an audio file"), "{err}");
    }

    #[test]
    fn a_worker_that_never_replied_names_its_exit() {
        let err =
            interpret_reply("fixup", b"", false, "signal: 11").expect_err("no reply is a failure");
        assert!(err.to_string().contains("signal: 11"), "{err}");
    }

    #[test]
    fn an_unparseable_reply_reports_what_arrived() {
        let err = interpret_reply("fixup", b"not json", true, "exit status: 0")
            .expect_err("garbage is a failure");
        assert!(err.to_string().contains("not json"), "{err}");
    }

    /// End to end through `run`, so the exit status is actually wired into the
    /// decision rather than only being handled by `interpret_reply`.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_pool_rejects_a_worker_that_claims_success_and_exits_nonzero() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("book.mp3");
        let reply = String::from_utf8(ok_reply(&output.display().to_string())).expect("utf8");

        // Stands in for a worker killed between flushing its reply and exiting.
        let fake = dir.path().join("lying-worker");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\ncat >/dev/null\nprintf '%s' '{reply}'\nexit 1\n"),
        )
        .expect("write fake worker");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake worker");

        let pool = MediaPool::new(MediaPoolConfig {
            workers: 1,
            confinement: Confinement::BestEffort,
            worker_bin: Some(fake),
        });
        assert!(pool.is_isolated(), "pool should spawn the fake worker");

        let err = pool
            .run(MediaJob::EncodeMp3 {
                input: dir.path().join("in.m4b"),
                output,
                lame: Box::default(),
                max_sample_rate: None,
            })
            .await
            .expect_err("a worker that exits nonzero has not succeeded");
        assert!(
            err.to_string().contains("reported success"),
            "error should say the reply and the exit disagreed: {err}"
        );
    }

    /// The point of handing out an `Arc`: a job that is already running keeps
    /// the pool it started on, so a reload can swap the global without
    /// disturbing it. The retired pool stays alive exactly as long as its work.
    #[test]
    fn a_replaced_pool_survives_until_its_last_holder_lets_go() {
        let first = MediaPool::new(MediaPoolConfig {
            workers: 2,
            confinement: Confinement::Off,
            worker_bin: None,
        });
        assert!(init_pool(first).is_ok() || read_pool().is_some());

        // Stands in for an in-flight job, which holds its handle across await.
        let in_flight = pool();
        let started_with = in_flight.capacity();

        let replaced = replace_pool(MediaPool::new(MediaPoolConfig {
            workers: started_with + 3,
            confinement: Confinement::Off,
            worker_bin: None,
        }))
        .expect("a pool was installed");

        // New callers see the new pool; the running job still sees the old one.
        assert_eq!(pool().capacity(), started_with + 3);
        assert_eq!(in_flight.capacity(), started_with);
        assert!(
            Arc::ptr_eq(&in_flight, &replaced),
            "the in-flight handle should be the pool that was retired"
        );

        // Not yet drained: the job is still holding it. Counted as a bound
        // rather than an exact number, because sibling tests in this binary
        // share the global pool. A retired pool is unreachable through `pool`,
        // so nothing can newly acquire it and the count can only fall.
        let held = Arc::strong_count(&replaced);
        assert!(held >= 2, "the in-flight handle should still count: {held}");
        drop(in_flight);
        assert!(
            Arc::strong_count(&replaced) < held,
            "finishing the job should have released the retired pool"
        );
    }

    /// A reload compares against what is running, so respelling the worker
    /// count as its resolved value must not register as a change.
    #[test]
    fn a_derived_worker_count_compares_equal_to_its_resolved_spelling() {
        let derived = MediaPoolConfig {
            workers: 0,
            confinement: Confinement::Off,
            worker_bin: None,
        }
        .normalized();
        let explicit = MediaPoolConfig {
            workers: default_worker_count(),
            confinement: Confinement::Off,
            worker_bin: None,
        }
        .normalized();
        assert_eq!(derived, explicit);

        let changed = MediaPoolConfig {
            workers: default_worker_count() + 1,
            confinement: Confinement::Off,
            worker_bin: None,
        }
        .normalized();
        assert_ne!(derived, changed);
    }

    /// The pool keeps the settings it was built from, which is what makes the
    /// reload comparison possible.
    #[test]
    fn a_pool_remembers_the_settings_it_was_built_from() {
        let pool = MediaPool::new(MediaPoolConfig {
            workers: 3,
            confinement: Confinement::Off,
            worker_bin: None,
        });
        assert_eq!(pool.config.workers, 3);
        assert_eq!(pool.config.confinement, Confinement::Off);
        assert_eq!(pool.capacity(), 3);
    }

    #[test]
    fn the_worker_env_allowlist_keeps_secrets_out() {
        assert!(worker_env_allowed("LANG"));
        assert!(worker_env_allowed("RUST_BACKTRACE"));
        // Windows sets these in mixed case and compares case-insensitively.
        assert!(worker_env_allowed("SystemRoot"));
        assert!(worker_env_allowed("SYSTEMROOT"));

        for secret in [
            "BOOKCLERK_AUTH_PASSWORD",
            "BOOKCLERK_OPERATOR_TOKEN",
            "BOOKCLERK_FILES_DIR",
            "AWS_SECRET_ACCESS_KEY",
            "CLOUDFLARE_API_TOKEN",
            "BOOKCLERK_DATABASE_POSTGRES_URL",
        ] {
            assert!(
                !worker_env_allowed(secret),
                "{secret} must not be inherited"
            );
        }
        // The worker is launched by absolute path and never spawns anything.
        assert!(!worker_env_allowed("PATH"));
        assert!(!worker_env_allowed("HOME"));
    }

    /// The allowlist is only worth anything if `run` actually applies it, so
    /// this reads the environment a real spawned child received.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_spawned_worker_does_not_inherit_host_secrets() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("book.mp3");
        let dumped = dir.path().join("child-env");
        let reply = String::from_utf8(ok_reply(&output.display().to_string())).expect("utf8");

        let fake = dir.path().join("env-dumping-worker");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\ncat >/dev/null\nenv >'{}'\nprintf '%s' '{reply}'\n",
                dumped.display()
            ),
        )
        .expect("write fake worker");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake worker");

        // SAFETY: single-threaded within this test's runtime setup, and the
        // value is removed before any assertion can fail out of the function.
        std::env::set_var("BOOKCLERK_AUTH_PASSWORD", "hunter2");
        let pool = MediaPool::new(MediaPoolConfig {
            workers: 1,
            confinement: Confinement::BestEffort,
            worker_bin: Some(fake),
        });
        let result = pool
            .run(MediaJob::EncodeMp3 {
                input: dir.path().join("in.m4b"),
                output,
                lame: Box::default(),
                max_sample_rate: None,
            })
            .await;
        std::env::remove_var("BOOKCLERK_AUTH_PASSWORD");
        result.expect("fake worker replied ok");

        let child_env = std::fs::read_to_string(&dumped).expect("child wrote its env");
        assert!(
            !child_env.contains("hunter2"),
            "child inherited a host secret:\n{child_env}"
        );
        assert!(
            !child_env.contains("BOOKCLERK_AUTH_PASSWORD"),
            "child inherited a host secret key:\n{child_env}"
        );
        // The one variable the pool sets deliberately still arrives.
        assert!(
            child_env.contains(WORKER_ENFORCEMENT_ENV),
            "child lost its enforcement setting:\n{child_env}"
        );
    }

    /// When the platform cannot confine a guest at all, `Required` must refuse
    /// at construction rather than fail per-book on the first acquire.
    #[test]
    fn a_platform_that_cannot_confine_refuses_at_construction() {
        let pool = MediaPool::new(MediaPoolConfig {
            workers: 1,
            confinement: Confinement::Required,
            worker_bin: None,
        });
        if bookclerk_sandbox::capabilities().can_confine_guest() {
            // Host can confine (self or spawn). Discovery may still fail in a
            // test binary without a worker/jail beside it — sibling tests cover
            // that packaging error path.
            return;
        }
        assert!(pool.is_refusing());
        assert!(
            pool.summary().contains("cannot confine"),
            "summary should name the platform limit: {}",
            pool.summary()
        );
    }
}
