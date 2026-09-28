//! Bookclerk-owned AppContainer process launch with correct Job Object ordering.
//!
//! rappct 0.13.3 runs `CreateProcessW` then `AssignProcessToJobObject` while the
//! primary thread is already runnable, and does not `TerminateProcess` on later
//! failures. This module owns CreateProcess for production jail launches:
//!
//! 1. Prefer `PROC_THREAD_ATTRIBUTE_JOB_LIST` so assignment happens before any
//!    guest instruction runs (Windows 10+).
//! 2. Fall back to `CREATE_SUSPENDED` → configure Job → `AssignProcessToJobObject`
//!    → `ResumeThread`.
//! 3. On every failure after a successful `CreateProcessW`, terminate the child
//!    and close process, thread, pipe, and Job handles.
//!
//! Profile / capability SID derivation still uses rappct helpers.

#![cfg(windows)]
#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

use rappct::sid::SidAndAttributes;
use rappct::SecurityCapabilities;
use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT,
    HLOCAL, TRUE, WAIT_FAILED, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::{
    PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
    JobObjectBasicAccountingInformation, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::SandboxError;

/// `SE_GROUP_ENABLED` attribute applied to AppContainer capability SIDs.
const SE_GROUP_ENABLED: u32 = 0x0000_0004;

/// Limits applied to the kill-on-close Job Object that owns the guest tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobResourceLimits {
    /// Maximum committed memory for the Job Object, when set.
    pub memory_bytes: Option<usize>,
    /// CPU rate cap as a percent of one logical processor, when set.
    pub cpu_rate_percent: Option<u32>,
    /// Maximum active processes in the Job Object, when set.
    pub active_processes: Option<u32>,
}

/// Inputs for an AppContainer CreateProcess launch.
#[derive(Debug)]
pub struct LaunchRequest<'a> {
    /// Guest executable path passed as `lpApplicationName`.
    pub exe: &'a Path,
    /// Full `lpCommandLine` including quoted argv[0].
    pub cmdline: String,
    /// Working directory for CreateProcess.
    pub cwd: PathBuf,
    /// Child environment block entries as key/value pairs.
    pub env: Vec<(OsString, OsString)>,
    /// AppContainer security capabilities. `None` launches without a container
    /// (Windows `Isolation::Off` still uses the jail for handle inheritance).
    pub sec: Option<&'a SecurityCapabilities>,
    /// Resource limits applied to the kill-on-close Job Object.
    pub job: JobResourceLimits,
    /// Extra inheritable handles (already duplicated into this process).
    pub extra_handles: Vec<HANDLE>,
    /// Guest stdin handle; `None` creates a pipe and returns the parent write end.
    pub explicit_stdin: Option<HANDLE>,
    /// Guest stdout handle; `None` creates a pipe and returns the parent read end.
    pub explicit_stdout: Option<HANDLE>,
}

/// Reconstruct a [`HANDLE`] from a [`crate::JailHandoff`] integer.
#[must_use]
pub fn handle_from_u64(value: u64) -> HANDLE {
    HANDLE(value as usize as *mut std::ffi::c_void)
}

/// A running AppContainer guest with proxied stdio and a kill-on-close Job.
#[derive(Debug)]
pub struct LaunchedGuest {
    /// Process ID of the primary guest process.
    #[allow(dead_code)]
    pub pid: u32,
    /// Parent write end of the guest stdin pipe.
    pub stdin: Option<File>,
    /// Parent read end of the guest stdout pipe.
    pub stdout: Option<File>,
    /// Parent read end of the guest stderr pipe.
    pub stderr: Option<File>,
    /// Primary process handle kept until drop or [`Self::wait`].
    process: HANDLE,
    /// Kill-on-close Job Object owning the guest tree.
    job: HANDLE,
}

// Process/job handles are consumed by Windows integration tests and diagnostics.
#[allow(dead_code)]
impl LaunchedGuest {
    /// Process handle for diagnostics / tests (do not close).
    #[must_use]
    pub fn process_handle(&self) -> HANDLE {
        self.process
    }

    /// Job Object handle for diagnostics / tests (do not close).
    #[must_use]
    pub fn job_handle(&self) -> HANDLE {
        self.job
    }

    /// Whether `process` is a member of this launch's Job Object.
    pub fn contains_process(&self, process: HANDLE) -> Result<bool, SandboxError> {
        let mut inside = BOOL(0);
        unsafe {
            IsProcessInJob(process, Some(self.job), &mut inside)
                .map_err(|err| launch_err("IsProcessInJob", &err.to_string()))?;
        }
        Ok(inside.as_bool())
    }
}

impl LaunchedGuest {
    /// Wait for the primary process to exit. Dropping this value closes the Job
    /// handle (kill-on-close), terminating any remaining descendants.
    pub fn wait(self, timeout: Option<Duration>) -> Result<u32, SandboxError> {
        let ms = timeout
            .map(|d| d.as_millis().min(u128::from(u32::MAX)) as u32)
            .unwrap_or(INFINITE);
        unsafe {
            let r = WaitForSingleObject(self.process, ms);
            if r == WAIT_FAILED {
                return Err(launch_err("WaitForSingleObject", "process wait failed"));
            }
            if r == WAIT_TIMEOUT {
                return Err(launch_err("wait", "timeout waiting for AppContainer guest"));
            }
            let mut code = 0u32;
            GetExitCodeProcess(self.process, &mut code)
                .map_err(|err| launch_err("GetExitCodeProcess", &err.to_string()))?;
            Ok(code)
        }
    }
}

impl Drop for LaunchedGuest {
    fn drop(&mut self) {
        unsafe {
            if !self.process.is_invalid() {
                let _ = CloseHandle(self.process);
                self.process = HANDLE::default();
            }
            // Closing the job with KILL_ON_JOB_CLOSE terminates the tree.
            if !self.job.is_invalid() {
                let _ = CloseHandle(self.job);
                self.job = HANDLE::default();
            }
            // Stdio Files drop normally.
        }
    }
}

/// Launch `request` inside an AppContainer and a kill-on-close Job Object.
pub fn launch_appcontainer_guest(
    request: LaunchRequest<'_>,
) -> Result<LaunchedGuest, SandboxError> {
    unsafe { launch_impl(request) }
}

/// Child of [`spawn_with_handle_list`].
///
/// Drop closes the kill-on-close Job and terminates the process tree.
pub struct HandleListChild {
    /// Running guest. Drop closes the Job and kills the tree.
    inner: LaunchedGuest,
}

impl HandleListChild {
    /// Parent write end when stdin was not an explicit inherited handle.
    pub fn take_stdin(&mut self) -> Option<File> {
        self.inner.stdin.take()
    }

    /// Parent read end when stdout was not an explicit inherited handle.
    pub fn take_stdout(&mut self) -> Option<File> {
        self.inner.stdout.take()
    }

    /// Parent read end of the stderr pipe.
    pub fn take_stderr(&mut self) -> Option<File> {
        self.inner.stderr.take()
    }
}

/// Spawn `exe` with no AppContainer, inheriting only the listed handles.
///
/// Stdio handles and `extra` are placed on `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
/// This is the stable replacement for `CommandExt::inherit_handles`, which does
/// not inherit every inheritable handle in the parent. `stdin`, `stdout`, and
/// `extra` are closed in this process after the child inherits them.
///
/// # Errors
///
/// Returns [`SandboxError::Backend`] when CreateProcess fails.
pub fn spawn_with_handle_list(
    exe: &Path,
    cwd: &Path,
    env: Vec<(OsString, OsString)>,
    stdin: Option<OwnedHandle>,
    stdout: Option<OwnedHandle>,
    extra: Vec<OwnedHandle>,
) -> Result<HandleListChild, SandboxError> {
    let explicit_stdin = stdin.map(forget_raw);
    let explicit_stdout = stdout.map(forget_raw);
    let extra_handles = extra.into_iter().map(forget_raw).collect();
    let guest = launch_appcontainer_guest(LaunchRequest {
        exe,
        cmdline: quote_exe(exe),
        cwd: cwd.to_path_buf(),
        env,
        sec: None,
        job: JobResourceLimits {
            memory_bytes: None,
            cpu_rate_percent: None,
            active_processes: None,
        },
        extra_handles,
        explicit_stdin,
        explicit_stdout,
    })?;
    Ok(HandleListChild { inner: guest })
}

/// Drop Rust ownership so CreateProcess can close the raw handle itself.
fn forget_raw(handle: OwnedHandle) -> HANDLE {
    let raw = handle.as_raw_handle();
    std::mem::forget(handle);
    HANDLE(raw)
}

/// Quote `exe` for `lpCommandLine` when the path contains spaces or quotes.
fn quote_exe(exe: &Path) -> String {
    let text = exe.as_os_str().to_string_lossy();
    if text.chars().any(|ch| ch == ' ' || ch == '\t' || ch == '"') {
        format!("\"{text}\"")
    } else {
        text.into_owned()
    }
}

/// Creates an AppContainer child with stdio pipes and Job Object membership.
///
/// # Safety
///
/// Caller must ensure `request` paths and Win32 inputs are valid for CreateProcess.
unsafe fn launch_impl(request: LaunchRequest<'_>) -> Result<LaunchedGuest, SandboxError> {
    let owned_caps = match request.sec {
        Some(sec) => Some(OwnedSecurityCapabilities::from_rappct(sec)?),
        None => None,
    };

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: TRUE,
    };

    let (child_stdin, parent_stdin_raw) = match request.explicit_stdin {
        Some(h) => (h, None),
        None => {
            let (read, write) = create_pipe_pair(&mut sa)?;
            (read, Some(write))
        }
    };
    let (parent_stdout_raw, child_stdout) = match request.explicit_stdout {
        Some(h) => (None, h),
        None => {
            let (read, write) = create_pipe_pair(&mut sa)?;
            (Some(read), write)
        }
    };
    let (parent_stderr_raw, child_stderr) = create_pipe_pair(&mut sa)?;

    let close_stdio = || {
        cleanup_handles(&[child_stdin, child_stdout, child_stderr]);
        cleanup_optional(&[parent_stdin_raw, parent_stdout_raw, Some(parent_stderr_raw)]);
    };

    // Parent pipe ends stay non-inheritable. Extra / explicit child ends
    // must be inheritable so HANDLE_LIST can pass them.
    for h in [parent_stdin_raw, parent_stdout_raw, Some(parent_stderr_raw)]
        .into_iter()
        .flatten()
    {
        SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(|err| {
            close_stdio();
            launch_err("SetHandleInformation", &err.to_string())
        })?;
    }
    for &h in request
        .extra_handles
        .iter()
        .chain(request.explicit_stdin.iter())
        .chain(request.explicit_stdout.iter())
    {
        SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT).map_err(|err| {
            close_stdio();
            launch_err("SetHandleInformation(inherit extra)", &err.to_string())
        })?;
    }

    let job = match CreateJobObjectW(None, PCWSTR::null()) {
        Ok(h) => h,
        Err(err) => {
            close_stdio();
            return Err(launch_err("CreateJobObjectW", &err.to_string()));
        }
    };

    if let Err(err) = configure_job(job, &request.job) {
        let _ = CloseHandle(job);
        close_stdio();
        return Err(err.into_sandbox());
    }

    // `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` fails with ERROR_INVALID_PARAMETER
    // when the same value appears twice. Stdio may alias one duplex pipe.
    let inherit = unique_handle_list(
        [child_stdin, child_stdout, child_stderr]
            .into_iter()
            .chain(request.extra_handles.iter().copied()),
    );
    let cleanup_all = || {
        let _ = CloseHandle(job);
        close_stdio();
    };

    // Primary path: CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread.
    // PROC_THREAD_ATTRIBUTE_JOB_LIST is attempted first when the env opt-in is set;
    // on Windows CI runners CreateProcessW returned ERROR_INVALID_HANDLE (6) with
    // JOB_LIST + AppContainer + HANDLE_LIST, while the suspended path is reliable.
    // Security invariant holds either way: no guest instruction runs before Job assign.
    let force_assign_fail = std::env::var_os("BOOKCLERK_TEST_FAIL_JOB_ASSIGN").is_some();
    let try_job_list =
        !force_assign_fail && std::env::var_os("BOOKCLERK_AC_USE_JOB_LIST").is_some();

    let prepare_attrs = |with_job: bool| -> Result<AttributeList, SandboxError> {
        let count = 1 + u32::from(owned_caps.is_some()) + u32::from(with_job);
        let mut attrs = AttributeList::new(count).inspect_err(|_| cleanup_all())?;
        if let Some(caps) = owned_caps.as_ref() {
            attrs
                .set_security_capabilities(caps)
                .inspect_err(|_| cleanup_all())?;
        }
        attrs
            .set_handle_list(&inherit)
            .inspect_err(|_| cleanup_all())?;
        Ok(attrs)
    };

    let (mut attr, use_job_list) = if try_job_list {
        let mut with_job = prepare_attrs(true)?;
        if with_job.set_job_list(&[job]).is_ok() {
            (with_job, true)
        } else {
            tracing::debug!("PROC_THREAD_ATTRIBUTE_JOB_LIST refused; using CREATE_SUSPENDED");
            drop(with_job);
            (prepare_attrs(false)?, false)
        }
    } else {
        (prepare_attrs(false)?, false)
    };

    let mut si_ex: STARTUPINFOEXW = std::mem::zeroed();
    si_ex.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si_ex.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
    si_ex.StartupInfo.hStdInput = child_stdin;
    si_ex.StartupInfo.hStdOutput = child_stdout;
    si_ex.StartupInfo.hStdError = child_stderr;
    si_ex.lpAttributeList = attr.as_mut_ptr();

    let exe_w = wide_os(request.exe.as_os_str());
    let mut cmdline_w = wide_str(&request.cmdline);
    let cwd_w = wide_os(request.cwd.as_os_str());
    let env_block = build_env_block(&request.env);

    // DETACHED_PROCESS keeps conhost.exe out of the Job. CREATE_NO_WINDOW still
    // starts a hidden console host, and that host counts toward
    // JOB_OBJECT_LIMIT_ACTIVE_PROCESS — a 2-process gateway (launcher + workerd)
    // then fails the next CreateProcess with ERROR_ACCESS_DENIED.
    let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS;
    if !use_job_list {
        flags |= CREATE_SUSPENDED;
    }

    let mut pi = PROCESS_INFORMATION::default();
    let mut cp = CreateProcessW(
        PCWSTR(exe_w.as_ptr()),
        Some(PWSTR(cmdline_w.as_mut_ptr())),
        None,
        None,
        true,
        flags,
        Some(env_block.as_ptr().cast()),
        PCWSTR(cwd_w.as_ptr()),
        &si_ex.StartupInfo,
        &mut pi,
    );

    // Capture the Win32 code before any cleanup API can replace `GetLastError`.
    let mut job_list_failure = None;
    let use_job_list = if cp.is_err() && use_job_list {
        let err = cp.expect_err("checked is_err");
        let rendered = render_win_error(&err);
        tracing::warn!(
            stage = "CreateProcessW(JOB_LIST)",
            win32 = rendered.win32,
            hresult = rendered.hresult,
            "CreateProcessW with JOB_LIST failed; retrying CREATE_SUSPENDED"
        );
        job_list_failure = Some(rendered);
        drop(attr);
        let mut retry_attrs = match prepare_attrs(false) {
            Ok(a) => a,
            Err(err) => {
                let diag = query_job_diag(job, None);
                let _ = CloseHandle(job);
                cleanup_handles(&[child_stdin, child_stdout, child_stderr]);
                cleanup_optional(&[parent_stdin_raw, parent_stdout_raw, Some(parent_stderr_raw)]);
                let prior = job_list_failure
                    .as_ref()
                    .map(|e| e.display_code())
                    .unwrap_or_else(|| "unknown".into());
                return Err(launch_err(
                    "CreateProcessW(JOB_LIST)",
                    &format!("original {prior}; attribute rebuild failed: {err}; {diag}"),
                ));
            }
        };
        si_ex.lpAttributeList = retry_attrs.as_mut_ptr();
        flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_UNICODE_ENVIRONMENT
            | CREATE_SUSPENDED
            | DETACHED_PROCESS;
        pi = PROCESS_INFORMATION::default();
        cp = CreateProcessW(
            PCWSTR(exe_w.as_ptr()),
            Some(PWSTR(cmdline_w.as_mut_ptr())),
            None,
            None,
            true,
            flags,
            Some(env_block.as_ptr().cast()),
            PCWSTR(cwd_w.as_ptr()),
            &si_ex.StartupInfo,
            &mut pi,
        );
        attr = retry_attrs;
        false
    } else {
        use_job_list
    };
    let create_failure = cp.err().map(|err| render_win_error(&err));

    // Child pipe / extra ends must not stay open in the parent.
    close_unique_handles(
        [child_stdin, child_stdout, child_stderr]
            .into_iter()
            .chain(request.extra_handles.iter().copied()),
    );
    drop(attr);
    drop(owned_caps);

    if let Some(err) = create_failure {
        let diag = query_job_diag(job, None);
        let _ = CloseHandle(job);
        cleanup_optional(&[parent_stdin_raw, parent_stdout_raw, Some(parent_stderr_raw)]);
        let stage = "CreateProcessW(CREATE_SUSPENDED)";
        let detail = match job_list_failure {
            Some(prior) => format!(
                "JOB_LIST failed {}; suspended retry failed {}; {diag}",
                prior.display_code(),
                err.display_code()
            ),
            None => format!("failed {}; {diag}", err.display_code()),
        };
        return Err(launch_err(stage, &detail));
    }

    // From here, every failure must TerminateProcess + close all handles.
    // Job membership and quotas are read before the job handle is closed.
    let fail_after_create = |stage: &str, detail: &str| -> SandboxError {
        let diag = query_job_diag(job, Some(pi.hProcess));
        unsafe {
            let _ = TerminateProcess(pi.hProcess, 1);
            let _ = WaitForSingleObject(pi.hProcess, 5_000);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(job);
            cleanup_optional(&[parent_stdin_raw, parent_stdout_raw, Some(parent_stderr_raw)]);
        }
        launch_err(stage, &format!("{detail}; {diag}"))
    };

    if force_assign_fail {
        return Err(fail_after_create(
            "AssignProcessToJobObject",
            "BOOKCLERK_TEST_FAIL_JOB_ASSIGN forced failure",
        ));
    }

    if !use_job_list {
        if let Err(err) = AssignProcessToJobObject(job, pi.hProcess) {
            return Err(fail_after_create(
                "AssignProcessToJobObject",
                &err.to_string(),
            ));
        }
        if ResumeThread(pi.hThread) == u32::MAX {
            return Err(fail_after_create(
                "ResumeThread",
                &std::io::Error::last_os_error().to_string(),
            ));
        }
    } else {
        // JOB_LIST path: membership must already be true before any guest
        // instruction runs. Never AssignProcessToJobObject after the fact —
        // that would violate the suspended-until-in-job invariant.
        let mut inside = BOOL(0);
        if IsProcessInJob(pi.hProcess, Some(job), &mut inside).is_err() || !inside.as_bool() {
            return Err(fail_after_create(
                "IsProcessInJob",
                "guest was not in Job Object after PROC_THREAD_ATTRIBUTE_JOB_LIST",
            ));
        }
    }

    let _ = CloseHandle(pi.hThread);

    let stdin = parent_stdin_raw.map(file_from_handle);
    let stdout = parent_stdout_raw.map(file_from_handle);
    let stderr = Some(file_from_handle(parent_stderr_raw));

    Ok(LaunchedGuest {
        pid: pi.dwProcessId,
        stdin,
        stdout,
        stderr,
        process: pi.hProcess,
        job,
    })
}

/// Why `SetInformationJobObject` refused a limit.
enum JobConfigError {
    /// The kernel does not implement this Job control.
    Unsupported(String),
    /// The call failed for a reason other than missing support.
    Failed(String),
}

impl JobConfigError {
    /// Classifies `err` as unsupported or failed, keeping the Win32 code.
    fn from_win(stage: &str, err: &windows::core::Error) -> Self {
        let rendered = render_win_error(err);
        let detail = format!("{stage}: {}", rendered.display_code());
        if rendered.unsupported {
            Self::Unsupported(detail)
        } else {
            Self::Failed(detail)
        }
    }

    /// Maps this failure onto a sandbox launch error for `configure_job`.
    fn into_sandbox(self) -> SandboxError {
        match self {
            Self::Unsupported(detail) | Self::Failed(detail) => {
                launch_err("configure_job", &detail)
            }
        }
    }

    /// Maps this failure onto an I/O error for session-Job setup.
    fn into_io(self) -> std::io::Error {
        match self {
            Self::Unsupported(detail) => {
                std::io::Error::new(std::io::ErrorKind::Unsupported, detail)
            }
            Self::Failed(detail) => std::io::Error::other(detail),
        }
    }
}

/// Applies memory, CPU rate, and process limits plus kill-on-close to `job`.
///
/// CPU is applied only when `limits.cpu_rate_percent` is set. Sibling inner
/// Jobs omit it so a nested rate is not a fraction of the outer session cap.
///
/// # Errors
///
/// Returns [`JobConfigError::Unsupported`] when the kernel rejects the control
/// as unimplemented, and [`JobConfigError::Failed`] for every other failure.
fn configure_job(job: HANDLE, limits: &JobResourceLimits) -> Result<(), JobConfigError> {
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(bytes) = limits.memory_bytes {
            // Job-wide commit charge (main + children), aligned with Linux cgroup memory.max.
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = bytes;
        }
        if let Some(n) = limits.active_processes {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = n;
        }
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|err| JobConfigError::from_win("SetInformationJobObject(ext)", &err))?;

        if let Some(percent) = limits.cpu_rate_percent {
            let mut cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE
                    | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                ..Default::default()
            };
            // Job CpuRate is % of *all* processors; Spec percent is one-core.
            let logical_cpus = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1);
            cpu.Anonymous.CpuRate = crate::windows_job_cpu_rate(percent, logical_cpus);
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                &cpu as *const _ as *const _,
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
            .map_err(|err| JobConfigError::from_win("SetInformationJobObject(cpu)", &err))?;
        }
    }
    Ok(())
}

/// Owned `PROC_THREAD_ATTRIBUTE_LIST` buffer for `STARTUPINFOEXW`.
struct AttributeList {
    /// Backing storage for the attribute list.
    _buf: Vec<u8>,
    /// Pointer into `_buf` passed to Win32 APIs.
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttributeList {
    /// Allocates an attribute list with `count` entries.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when `InitializeProcThreadAttributeList` fails.
    fn new(count: u32) -> Result<Self, SandboxError> {
        let mut bytes = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(None, count, Some(0), &mut bytes);
        }
        let mut buf = vec![0u8; bytes];
        let ptr = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
        unsafe {
            InitializeProcThreadAttributeList(Some(ptr), count, Some(0), &mut bytes)
                .map_err(|err| launch_err("InitializeProcThreadAttributeList", &err.to_string()))?;
        }
        Ok(Self { _buf: buf, ptr })
    }

    /// Returns the list pointer for `STARTUPINFOEXW.lpAttributeList`.
    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.ptr
    }

    /// Sets `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` on the list.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when `UpdateProcThreadAttribute` fails.
    fn set_security_capabilities(
        &mut self,
        caps: &OwnedSecurityCapabilities,
    ) -> Result<(), SandboxError> {
        unsafe {
            UpdateProcThreadAttribute(
                self.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                Some(caps.as_ptr().cast()),
                size_of::<SECURITY_CAPABILITIES>(),
                None,
                None,
            )
            .map_err(|err| launch_err("UpdateProcThreadAttribute(security)", &err.to_string()))
        }
    }

    /// Sets `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` for stdio inheritance.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when `UpdateProcThreadAttribute` fails.
    fn set_handle_list(&mut self, handles: &[HANDLE]) -> Result<(), SandboxError> {
        unsafe {
            UpdateProcThreadAttribute(
                self.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                std::mem::size_of_val(handles),
                None,
                None,
            )
            .map_err(|err| launch_err("UpdateProcThreadAttribute(handles)", &err.to_string()))
        }
    }

    /// Sets `PROC_THREAD_ATTRIBUTE_JOB_LIST` for pre-create Job assignment.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when `UpdateProcThreadAttribute` fails.
    fn set_job_list(&mut self, jobs: &[HANDLE]) -> Result<(), SandboxError> {
        unsafe {
            UpdateProcThreadAttribute(
                self.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                std::mem::size_of_val(jobs),
                None,
                None,
            )
            .map_err(|err| launch_err("UpdateProcThreadAttribute(job_list)", &err.to_string()))
        }
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr);
        }
    }
}

/// Keeps AppContainer SID allocations alive for `SECURITY_CAPABILITIES`.
struct OwnedSecurityCapabilities {
    /// Package SID converted from rappct.
    _app_sid: LocalSid,
    /// Capability SIDs converted from rappct.
    _cap_sids: Vec<LocalSid>,
    /// `SID_AND_ATTRIBUTES` slice referenced by `sc`.
    _caps: Box<[SID_AND_ATTRIBUTES]>,
    /// Win32 structure passed to CreateProcess extended attributes.
    sc: SECURITY_CAPABILITIES,
}

impl OwnedSecurityCapabilities {
    /// Builds owned Win32 security capabilities from rappct inputs.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when SID SDDL conversion fails.
    fn from_rappct(sec: &SecurityCapabilities) -> Result<Self, SandboxError> {
        let app = LocalSid::from_sddl(sec.package.as_string())?;
        let mut cap_sids = Vec::with_capacity(sec.caps.len());
        for SidAndAttributes { sid_sddl, .. } in &sec.caps {
            cap_sids.push(LocalSid::from_sddl(sid_sddl)?);
        }
        let caps_vec: Vec<SID_AND_ATTRIBUTES> = cap_sids
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_psid(),
                Attributes: SE_GROUP_ENABLED,
            })
            .collect();
        let caps = caps_vec.into_boxed_slice();
        let sc = SECURITY_CAPABILITIES {
            AppContainerSid: app.as_psid(),
            Capabilities: caps.as_ptr() as *mut _,
            CapabilityCount: caps.len() as u32,
            Reserved: 0,
        };
        Ok(Self {
            _app_sid: app,
            _cap_sids: cap_sids,
            _caps: caps,
            sc,
        })
    }

    /// Returns a pointer to the embedded `SECURITY_CAPABILITIES`.
    fn as_ptr(&self) -> *const SECURITY_CAPABILITIES {
        &self.sc
    }
}

/// Owned PSID allocated with `ConvertStringSidToSidW`.
struct LocalSid {
    /// Win32 SID pointer freed on drop.
    psid: PSID,
}

impl LocalSid {
    /// Parses an SDDL string into an owned SID.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Backend`] when Win32 rejects the SDDL.
    fn from_sddl(sddl: &str) -> Result<Self, SandboxError> {
        let wide = wide_str(sddl);
        let mut psid = PSID(ptr::null_mut());
        unsafe {
            ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut psid)
                .map_err(|err| launch_err("ConvertStringSidToSidW", &err.to_string()))?;
        }
        Ok(Self { psid })
    }

    /// Returns the raw PSID for Win32 trustee and capability structs.
    fn as_psid(&self) -> PSID {
        self.psid
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        if !self.psid.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.psid.0)));
            }
            self.psid = PSID(ptr::null_mut());
        }
    }
}

/// Returns `(read, write)` ends from `CreatePipe`.
fn create_pipe_pair(sa: &mut SECURITY_ATTRIBUTES) -> Result<(HANDLE, HANDLE), SandboxError> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe {
        CreatePipe(&mut read, &mut write, Some(sa), 0)
            .map_err(|err| launch_err("CreatePipe", &err.to_string()))?;
    }
    Ok((read, write))
}

/// Wraps a Win32 handle in a `File` that closes the handle on drop.
fn file_from_handle(handle: HANDLE) -> File {
    unsafe { File::from_raw_handle(handle.0 as RawHandle) }
}

/// Closes every non-invalid handle in `handles`, ignoring errors.
fn cleanup_handles(handles: &[HANDLE]) {
    for &h in handles {
        if !h.is_invalid() && h != HANDLE::default() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Closes every present handle in `handles`.
fn cleanup_optional(handles: &[Option<HANDLE>]) {
    for handle in handles.iter().flatten() {
        cleanup_handles(&[*handle]);
    }
}

/// Handle values for `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, each value once.
///
/// Windows rejects a repeated entry. Callers may still point both standard
/// handles at one value; that value is listed a single time.
fn unique_handle_list(handles: impl IntoIterator<Item = HANDLE>) -> Vec<HANDLE> {
    let mut out: Vec<HANDLE> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for h in handles {
        if seen.insert(h.0 as usize) {
            out.push(h);
        }
    }
    out
}

/// Closes each distinct non-invalid handle once.
fn close_unique_handles(handles: impl IntoIterator<Item = HANDLE>) {
    let mut seen = std::collections::HashSet::new();
    for h in handles {
        let key = h.0 as usize;
        if !h.is_invalid() && h != HANDLE::default() && seen.insert(key) {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Encodes a Rust str as a NUL-terminated UTF-16 vector for Win32 APIs.
fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Encodes an `OsStr` as a NUL-terminated UTF-16 vector for Win32 APIs.
fn wide_os(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Builds a double-NUL-terminated environment block for `CreateProcessW`.
fn build_env_block(env: &[(OsString, OsString)]) -> Vec<u16> {
    let mut block = Vec::new();
    for (k, v) in env {
        let mut kv = OsString::from(k);
        kv.push("=");
        kv.push(v);
        block.extend(kv.as_os_str().encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// CPU-rate control block read back from a Job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCpuControl {
    /// `JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP` is set.
    pub hard_cap: bool,
    /// Raw Job `CpuRate` (cycles per 10_000 of the whole machine).
    pub cpu_rate: u32,
}

/// Win32 code captured from a `windows::core::Error` before later API calls.
struct RenderedWinError {
    /// Win32 code when the HRESULT uses `FACILITY_WIN32`, otherwise the bits.
    win32: u32,
    /// `HRESULT` bits from [`windows::core::Error::code`].
    hresult: i32,
    /// Whether the code is an explicit "not implemented" failure.
    unsupported: bool,
}

impl RenderedWinError {
    /// `win32=<code> hresult=<0x........>` with no command line or environment.
    fn display_code(&self) -> String {
        format!("win32={} hresult={:#010x}", self.win32, self.hresult as u32)
    }
}

/// `FACILITY_WIN32` in an HRESULT.
const FACILITY_WIN32: u32 = 7;
/// `ERROR_NOT_SUPPORTED`.
const WIN32_NOT_SUPPORTED: u32 = 50;
/// `ERROR_INVALID_FUNCTION`.
const WIN32_INVALID_FUNCTION: u32 = 1;
/// `ERROR_CALL_NOT_IMPLEMENTED`.
const WIN32_CALL_NOT_IMPLEMENTED: u32 = 120;

/// Splits `err` into a Win32 code and an HRESULT, marking unimplemented codes.
fn render_win_error(err: &windows::core::Error) -> RenderedWinError {
    let hresult = err.code().0;
    let bits = hresult as u32;
    let facility = (bits >> 16) & 0x1fff;
    let win32 = if facility == FACILITY_WIN32 {
        bits & 0xffff
    } else {
        bits
    };
    let unsupported = matches!(
        win32,
        WIN32_NOT_SUPPORTED | WIN32_INVALID_FUNCTION | WIN32_CALL_NOT_IMPLEMENTED
    );
    RenderedWinError {
        win32,
        hresult,
        unsupported,
    }
}

/// Job membership and quota snapshot. No command line or environment.
fn query_job_diag(job: HANDLE, process: Option<HANDLE>) -> String {
    let active = query_active_processes(job)
        .map(|n| n.to_string())
        .unwrap_or_else(|err| format!("error:{err}"));
    let limit = query_active_process_limit(job)
        .map(|n| match n {
            Some(v) => v.to_string(),
            None => "unset".into(),
        })
        .unwrap_or_else(|err| format!("error:{err}"));
    let member = match process {
        Some(process) => match query_in_job(job, process) {
            Ok(true) => "yes".into(),
            Ok(false) => "no".into(),
            Err(err) => format!("error:{err}"),
        },
        None => "n/a".into(),
    };
    format!("job active={active} limit={limit} member={member}")
}

/// Active process count from `JobObjectBasicAccountingInformation`.
///
/// # Errors
///
/// Returns the Win32 error text when the query fails.
fn query_active_processes(job: HANDLE) -> Result<u32, String> {
    unsafe {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
        QueryInformationJobObject(
            Some(job),
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut _,
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            None,
        )
        .map_err(|err| err.to_string())?;
        Ok(info.ActiveProcesses)
    }
}

/// `ActiveProcessLimit` when that Job limit is enabled.
///
/// # Errors
///
/// Returns the Win32 error text when the query fails. `Ok(None)` means the
/// limit flag is unset.
fn query_active_process_limit(job: HANDLE) -> Result<Option<u32>, String> {
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        QueryInformationJobObject(
            Some(job),
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            None,
        )
        .map_err(|err| err.to_string())?;
        let flags = info.BasicLimitInformation.LimitFlags;
        if (flags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS) == JOB_OBJECT_LIMIT_ACTIVE_PROCESS {
            Ok(Some(info.BasicLimitInformation.ActiveProcessLimit))
        } else {
            Ok(None)
        }
    }
}

/// Whether `process` is a member of `job`.
///
/// # Errors
///
/// Returns the Win32 error text when `IsProcessInJob` fails.
fn query_in_job(job: HANDLE, process: HANDLE) -> Result<bool, String> {
    let mut inside = BOOL(0);
    unsafe {
        // Win32 order is process, then job. The job handle is not a process.
        IsProcessInJob(process, Some(job), &mut inside).map_err(|err| err.to_string())?;
    }
    Ok(inside.as_bool())
}

/// Enabled CPU-rate control on `job`, if the kernel implements the query.
///
/// # Errors
///
/// Returns the rendered Win32 code when the query fails for a reason other
/// than missing support. `Ok(None)` means the control is unimplemented or off.
fn query_cpu_control(job: HANDLE) -> Result<Option<SessionCpuControl>, String> {
    unsafe {
        let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
        if let Err(err) = QueryInformationJobObject(
            Some(job),
            JobObjectCpuRateControlInformation,
            &mut cpu as *mut _ as *mut _,
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            None,
        ) {
            let rendered = render_win_error(&err);
            if rendered.unsupported {
                return Ok(None);
            }
            return Err(rendered.display_code());
        }
        let flags = cpu.ControlFlags;
        let enabled =
            (flags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE) == JOB_OBJECT_CPU_RATE_CONTROL_ENABLE;
        if !enabled {
            return Ok(None);
        }
        Ok(Some(SessionCpuControl {
            hard_cap: (flags & JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP)
                == JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            cpu_rate: cpu.Anonymous.CpuRate,
        }))
    }
}

/// Formats an AppContainer launch failure as [`SandboxError::Backend`].
fn launch_err(stage: &str, detail: &str) -> SandboxError {
    SandboxError::Backend {
        label: "appcontainer".into(),
        backend: "appcontainer",
        detail: format!("{stage}: {detail}"),
    }
}

/// Host-owned Job that holds both sibling `bookclerk-jail` processes.
///
/// Nested per-guest Jobs created by the jail still work; this Job is the
/// session-level `KILL_ON_JOB_CLOSE` cap (aggregate memory / PIDs / CPU).
pub struct SessionJob {
    /// Owned job-object handle as an integer so `SessionJob` is `Send`.
    ///
    /// The kernel object can be used from the vat thread. Drop closes it and
    /// kills the tree (`KILL_ON_JOB_CLOSE`). `0` means already closed.
    handle: isize,
}

impl SessionJob {
    /// Create a kill-on-close Job with `limits`.
    ///
    /// `BOOKCLERK_TEST_FAIL_SESSION_JOB` injects `create`, `configure`, or
    /// `unsupported` before any sibling could be assigned. Those failures do
    /// not leave a job handle behind.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::Unsupported`] when Job configuration is
    /// explicitly unsupported. Every other create or configure failure is a
    /// generic I/O error. Callers under required isolation must fail closed.
    pub fn create(limits: &crate::ResourceLimits) -> std::io::Result<Self> {
        if let Some(mode) = std::env::var_os("BOOKCLERK_TEST_FAIL_SESSION_JOB") {
            match mode.to_string_lossy().as_ref() {
                "unsupported" => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "BOOKCLERK_TEST_FAIL_SESSION_JOB=unsupported",
                    ));
                }
                "create" => {
                    return Err(std::io::Error::other(
                        "BOOKCLERK_TEST_FAIL_SESSION_JOB=create",
                    ));
                }
                "configure" => {
                    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                        .map_err(std::io::Error::other)?;
                    let _ = unsafe { CloseHandle(job) };
                    return Err(std::io::Error::other(
                        "BOOKCLERK_TEST_FAIL_SESSION_JOB=configure",
                    ));
                }
                _ => {}
            }
        }
        let job =
            unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(std::io::Error::other)?;
        let job_limits = JobResourceLimits {
            memory_bytes: limits.memory_bytes.and_then(|b| usize::try_from(b).ok()),
            cpu_rate_percent: limits.cpu_rate_percent,
            active_processes: limits.active_processes,
        };
        if let Err(err) = configure_job(job, &job_limits) {
            let _ = unsafe { CloseHandle(job) };
            return Err(err.into_io());
        }
        Ok(Self {
            handle: job.0 as isize,
        })
    }

    /// Active processes currently in this Job, including nested child Jobs.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `QueryInformationJobObject` fails.
    pub fn active_process_count(&self) -> std::io::Result<u32> {
        query_active_processes(self.as_handle()).map_err(std::io::Error::other)
    }

    /// Configured `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` value, when that limit is set.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `QueryInformationJobObject` fails.
    pub fn active_process_limit(&self) -> std::io::Result<Option<u32>> {
        query_active_process_limit(self.as_handle()).map_err(std::io::Error::other)
    }

    /// Whether `process` is a member of this Job.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `IsProcessInJob` fails.
    pub fn contains_process(&self, process: RawHandle) -> std::io::Result<bool> {
        let mut inside = BOOL(0);
        unsafe {
            // Win32 order is process, then job. The job handle is not a process.
            IsProcessInJob(HANDLE(process), Some(self.as_handle()), &mut inside)
                .map_err(std::io::Error::other)?;
        }
        Ok(inside.as_bool())
    }

    /// CPU hard-cap currently configured on this Job.
    ///
    /// `None` means no CPU-rate control is enabled (sibling inner Jobs).
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `QueryInformationJobObject` fails.
    pub fn cpu_control(&self) -> std::io::Result<Option<SessionCpuControl>> {
        query_cpu_control(self.as_handle()).map_err(std::io::Error::other)
    }

    /// Total user + kernel time charged to this Job.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `QueryInformationJobObject` fails.
    pub fn cpu_time(&self) -> std::io::Result<std::time::Duration> {
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            QueryInformationJobObject(
                Some(self.as_handle()),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut _,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
            .map_err(std::io::Error::other)?;
            let ticks = info
                .TotalUserTime
                .saturating_add(info.TotalKernelTime)
                .max(0) as u64;
            Ok(std::time::Duration::from_nanos(ticks.saturating_mul(100)))
        }
    }

    /// Assign an already-started process (typically `bookclerk-jail.exe`).
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `AssignProcessToJobObject` fails.
    pub fn assign(&self, process: RawHandle) -> std::io::Result<()> {
        unsafe {
            AssignProcessToJobObject(self.as_handle(), HANDLE(process))
                .map_err(std::io::Error::other)
        }
    }

    /// Reconstruct the Win32 job handle from the stored integer.
    fn as_handle(&self) -> HANDLE {
        HANDLE(self.handle as *mut std::ffi::c_void)
    }
}

impl Drop for SessionJob {
    fn drop(&mut self) {
        if self.handle != 0 {
            // Closing the Job kills the tree (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
            let _ = unsafe { CloseHandle(self.as_handle()) };
            self.handle = 0;
        }
    }
}

/// Terminate every process in `job` (best-effort) then close it.
#[allow(dead_code)]
pub fn terminate_job_tree(job: HANDLE) {
    unsafe {
        if !job.is_invalid() {
            let _ = TerminateJobObject(job, 1);
            let _ = CloseHandle(job);
        }
    }
}

/// Windows handle-list invariants that do not need a live process.
#[cfg(test)]
mod tests {
    use super::unique_handle_list;
    use windows::Win32::Foundation::HANDLE;

    /// A repeated stdio value is listed once for `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
    #[test]
    fn handle_list_lists_each_value_once() {
        let stdin = HANDLE(std::ptr::without_provenance_mut(1));
        let stderr = HANDLE(std::ptr::without_provenance_mut(2));
        let list = unique_handle_list([stdin, stdin, stderr, stdin]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0, stdin.0);
        assert_eq!(list[1].0, stderr.0);
    }

    /// Handles the caller did not pass stay off `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
    #[test]
    fn handle_list_omits_unrelated_handles() {
        let stdin = HANDLE(std::ptr::without_provenance_mut(1));
        let stdout = HANDLE(std::ptr::without_provenance_mut(2));
        let stderr = HANDLE(std::ptr::without_provenance_mut(3));
        let extra = HANDLE(std::ptr::without_provenance_mut(4));
        let decoy = HANDLE(std::ptr::without_provenance_mut(99));
        let list = unique_handle_list([stdin, stdout, stderr, extra]);
        assert_eq!(list.len(), 4);
        assert!(list.iter().all(|handle| handle.0 != decoy.0));
    }

    use std::os::windows::io::AsRawHandle;
    use std::sync::Mutex;

    use super::{launch_appcontainer_guest, LaunchRequest, SessionJob};
    use crate::ResourceLimits;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn limits(active: u32, cpu: Option<u32>) -> ResourceLimits {
        ResourceLimits {
            memory_bytes: None,
            cpu_rate_percent: cpu,
            active_processes: Some(active),
        }
    }

    fn spawn_linger() -> std::process::Child {
        // One process. `cmd /c ping` keeps cmd and ping, and a child created
        // after assignment counts toward JOB_OBJECT_LIMIT_ACTIVE_PROCESS.
        std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn linger")
    }

    /// Outer caps 5, 6, and 7 are the Windows baseline plus extras 0, 1, and 2.
    #[test]
    fn session_job_enforces_active_process_cap_and_keeps_one_cpu_limit() {
        for cap in [5_u32, 6, 7] {
            let job = SessionJob::create(&limits(cap, Some(80))).expect("create job");
            assert_eq!(job.active_process_limit().expect("limit"), Some(cap));
            let cpu = job.cpu_control().expect("cpu query").expect("cpu set");
            assert!(cpu.hard_cap);
            assert_eq!(
                cpu.cpu_rate,
                crate::windows_job_cpu_rate(80, crate::host_logical_cpus())
            );
            let mut children = Vec::new();
            for _ in 0..cap {
                let child = spawn_linger();
                job.assign(child.as_raw_handle())
                    .unwrap_or_else(|err| panic!("assign within cap {cap}: {err}"));
                assert!(job.contains_process(child.as_raw_handle()).expect("member"));
                children.push(child);
            }
            assert_eq!(job.active_process_count().expect("count"), cap);
            let mut extra = spawn_linger();
            let err = job
                .assign(extra.as_raw_handle())
                .expect_err("process past the cap");
            let text = err.to_string().to_ascii_lowercase();
            // Active-process denial is `ERROR_ACCESS_DENIED` on some builds and
            // `ERROR_NOT_ENOUGH_QUOTA` (0x80070718) on windows-latest.
            assert!(
                text.contains("denied") || text.contains("access") || text.contains("quota"),
                "cap {cap} next assign: {err}"
            );
            let _ = extra.kill();
            let _ = extra.wait();
            drop(job);
            for mut child in children {
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn job_without_cpu_rate_does_not_enable_cpu_control() {
        let job = SessionJob::create(&limits(2, None)).expect("create");
        assert!(job.cpu_control().expect("query").is_none());
    }

    #[test]
    fn injected_session_job_failures_do_not_return_a_job() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        for (mode, unsupported) in [
            ("create", false),
            ("configure", false),
            ("unsupported", true),
        ] {
            std::env::set_var("BOOKCLERK_TEST_FAIL_SESSION_JOB", mode);
            let err = match SessionJob::create(&limits(5, Some(80))) {
                Ok(_) => panic!("{mode} returned a job"),
                Err(err) => err,
            };
            if unsupported {
                assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
            } else {
                assert_ne!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
            }
            assert!(err.to_string().contains(mode), "{err}");
        }
        std::env::remove_var("BOOKCLERK_TEST_FAIL_SESSION_JOB");
    }

    #[test]
    fn create_process_failure_preserves_the_original_win32_code() {
        let missing = std::env::temp_dir().join("bookclerk-missing-launch-exe.exe");
        let err = launch_appcontainer_guest(LaunchRequest {
            exe: &missing,
            cmdline: "secret-command-line-must-not-leak".into(),
            cwd: std::env::temp_dir(),
            env: vec![(
                std::ffi::OsString::from("SECRET_ENV"),
                std::ffi::OsString::from("secret-value"),
            )],
            sec: None,
            job: super::JobResourceLimits {
                memory_bytes: None,
                cpu_rate_percent: None,
                active_processes: Some(2),
            },
            extra_handles: Vec::new(),
            explicit_stdin: None,
            explicit_stdout: None,
        })
        .expect_err("missing executable");
        let text = err.to_string();
        assert!(text.contains("CreateProcessW"), "{text}");
        assert!(
            text.contains("win32=2") || text.contains("win32=3"),
            "{text}"
        );
        assert!(text.contains("job active="), "{text}");
        assert!(!text.contains("secret-command-line"), "{text}");
        assert!(!text.contains("secret-value"), "{text}");
        assert!(!text.contains("SECRET_ENV"), "{text}");
    }

    #[test]
    fn cpu_hard_cap_bounds_a_short_burn() {
        let job = SessionJob::create(&limits(2, Some(10))).expect("create");
        let control = job.cpu_control().expect("cpu").expect("enabled");
        assert!(control.hard_cap);
        assert_eq!(
            control.cpu_rate,
            crate::windows_job_cpu_rate(10, crate::host_logical_cpus())
        );
        // Startup runs before the gate, so Job accounting does not include it.
        // The burn itself is 2s of wall time after the process is in the Job.
        let gate = std::env::temp_dir().join(format!(
            "bookclerk-cpu-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let script = format!(
            "$gate='{}'; while(-not (Test-Path -LiteralPath $gate)){{ Start-Sleep -Milliseconds 20 }}; $sw=[Diagnostics.Stopwatch]::StartNew(); while($sw.ElapsedMilliseconds -lt 2000){{}}",
            gate.display().to_string().replace('\'', "''")
        );
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .spawn()
            .expect("powershell");
        job.assign(child.as_raw_handle()).expect("assign burner");
        std::fs::write(&gate, b"go").expect("open gate");
        let _ = child.wait().expect("wait burner");
        let _ = std::fs::remove_file(&gate);
        let cpu = job.cpu_time().expect("cpu time");
        // 10% of one core for 2s is ~200ms. The Job interval can burst; an
        // uncapped burn is ~2s, so 1.2s still shows the hard cap engaged.
        assert!(
            cpu < std::time::Duration::from_millis(1200),
            "10% of one core over ~2s wall exceeded tolerance: {cpu:?}"
        );
        assert!(
            cpu > std::time::Duration::from_millis(15),
            "burner did not accumulate CPU time: {cpu:?}"
        );
    }
}
