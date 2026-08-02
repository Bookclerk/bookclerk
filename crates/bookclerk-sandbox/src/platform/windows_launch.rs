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
use std::os::windows::io::{FromRawHandle, RawHandle};
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
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::SandboxError;

const SE_GROUP_ENABLED: u32 = 0x0000_0004;

/// Limits applied to the kill-on-close Job Object that owns the guest tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobResourceLimits {
    pub memory_bytes: Option<usize>,
    pub cpu_rate_percent: Option<u32>,
    pub active_processes: Option<u32>,
}

/// Inputs for an AppContainer CreateProcess launch.
#[derive(Debug)]
pub struct LaunchRequest<'a> {
    pub exe: &'a Path,
    pub cmdline: String,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub sec: &'a SecurityCapabilities,
    pub job: JobResourceLimits,
}

/// A running AppContainer guest with proxied stdio and a kill-on-close Job.
#[derive(Debug)]
pub struct LaunchedGuest {
    #[allow(dead_code)]
    pub pid: u32,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    process: HANDLE,
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

unsafe fn launch_impl(request: LaunchRequest<'_>) -> Result<LaunchedGuest, SandboxError> {
    let owned_caps = OwnedSecurityCapabilities::from_rappct(request.sec)?;

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: TRUE,
    };

    let (child_stdin, parent_stdin_raw) = create_pipe_pair(&mut sa)?;
    let (parent_stdout_raw, child_stdout) = create_pipe_pair(&mut sa)?;
    let (parent_stderr_raw, child_stderr) = create_pipe_pair(&mut sa)?;

    // Ensure parent ends are not inherited.
    for h in [parent_stdin_raw, parent_stdout_raw, parent_stderr_raw] {
        SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(|err| {
            cleanup_handles(&[
                child_stdin,
                child_stdout,
                child_stderr,
                parent_stdin_raw,
                parent_stdout_raw,
                parent_stderr_raw,
            ]);
            launch_err("SetHandleInformation", &err.to_string())
        })?;
    }

    let job = match CreateJobObjectW(None, PCWSTR::null()) {
        Ok(h) => h,
        Err(err) => {
            cleanup_handles(&[
                child_stdin,
                child_stdout,
                child_stderr,
                parent_stdin_raw,
                parent_stdout_raw,
                parent_stderr_raw,
            ]);
            return Err(launch_err("CreateJobObjectW", &err.to_string()));
        }
    };

    if let Err(err) = configure_job(job, &request.job) {
        let _ = CloseHandle(job);
        cleanup_handles(&[
            child_stdin,
            child_stdout,
            child_stderr,
            parent_stdin_raw,
            parent_stdout_raw,
            parent_stderr_raw,
        ]);
        return Err(err);
    }

    let inherit = [child_stdin, child_stdout, child_stderr];
    let cleanup_all = || {
        let _ = CloseHandle(job);
        cleanup_handles(&[
            child_stdin,
            child_stdout,
            child_stderr,
            parent_stdin_raw,
            parent_stdout_raw,
            parent_stderr_raw,
        ]);
    };

    // Primary path: CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread.
    // PROC_THREAD_ATTRIBUTE_JOB_LIST is attempted first when the env opt-in is set;
    // on Windows CI runners CreateProcessW returned ERROR_INVALID_HANDLE (6) with
    // JOB_LIST + AppContainer + HANDLE_LIST, while the suspended path is reliable.
    // Security invariant holds either way: no guest instruction runs before Job assign.
    let force_assign_fail = std::env::var_os("BOOKCLERK_TEST_FAIL_JOB_ASSIGN").is_some();
    let try_job_list =
        !force_assign_fail && std::env::var_os("BOOKCLERK_AC_USE_JOB_LIST").is_some();

    let prepare_attrs = |count: u32| -> Result<AttributeList, SandboxError> {
        let mut attrs = AttributeList::new(count).inspect_err(|_| cleanup_all())?;
        attrs
            .set_security_capabilities(&owned_caps)
            .inspect_err(|_| cleanup_all())?;
        attrs
            .set_handle_list(&inherit)
            .inspect_err(|_| cleanup_all())?;
        Ok(attrs)
    };

    let (mut attr, use_job_list) = if try_job_list {
        let mut with_job = prepare_attrs(3)?;
        if with_job.set_job_list(&[job]).is_ok() {
            (with_job, true)
        } else {
            tracing::debug!("PROC_THREAD_ATTRIBUTE_JOB_LIST refused; using CREATE_SUSPENDED");
            drop(with_job);
            (prepare_attrs(2)?, false)
        }
    } else {
        (prepare_attrs(2)?, false)
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

    let mut flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;
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

    // If JOB_LIST CreateProcess failed, rebuild attributes without it and retry
    // suspended — measured ERROR_INVALID_HANDLE on some hosts with JOB_LIST.
    let use_job_list = if cp.is_err() && use_job_list {
        tracing::warn!(
            "CreateProcessW with JOB_LIST failed ({}); retrying CREATE_SUSPENDED",
            std::io::Error::last_os_error()
        );
        drop(attr);
        let mut retry_attrs = match prepare_attrs(2) {
            Ok(a) => a,
            Err(err) => {
                let _ = CloseHandle(job);
                cleanup_handles(&[
                    child_stdin,
                    child_stdout,
                    child_stderr,
                    parent_stdin_raw,
                    parent_stdout_raw,
                    parent_stderr_raw,
                ]);
                return Err(err);
            }
        };
        si_ex.lpAttributeList = retry_attrs.as_mut_ptr();
        flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
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

    // Child pipe ends must not stay open in the parent.
    let _ = CloseHandle(child_stdin);
    let _ = CloseHandle(child_stdout);
    let _ = CloseHandle(child_stderr);
    drop(attr);
    drop(owned_caps);

    if cp.is_err() {
        let _ = CloseHandle(job);
        cleanup_handles(&[parent_stdin_raw, parent_stdout_raw, parent_stderr_raw]);
        return Err(launch_err(
            "CreateProcessW",
            &format!(
                "AppContainer launch failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }

    // From here, every failure must TerminateProcess + close all handles.
    let fail_after_create = |stage: &str, detail: &str| -> SandboxError {
        unsafe {
            let _ = TerminateProcess(pi.hProcess, 1);
            let _ = WaitForSingleObject(pi.hProcess, 5_000);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(job);
            cleanup_handles(&[parent_stdin_raw, parent_stdout_raw, parent_stderr_raw]);
        }
        launch_err(stage, detail)
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

    let stdin = Some(file_from_handle(parent_stdin_raw));
    let stdout = Some(file_from_handle(parent_stdout_raw));
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

fn configure_job(job: HANDLE, limits: &JobResourceLimits) -> Result<(), SandboxError> {
    unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(bytes) = limits.memory_bytes {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = bytes;
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
        .map_err(|err| launch_err("SetInformationJobObject(ext)", &err.to_string()))?;

        if let Some(percent) = limits.cpu_rate_percent {
            let mut cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE
                    | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                ..Default::default()
            };
            cpu.Anonymous.CpuRate = percent.clamp(1, 100) * 100;
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                &cpu as *const _ as *const _,
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
            .map_err(|err| launch_err("SetInformationJobObject(cpu)", &err.to_string()))?;
        }
    }
    Ok(())
}

struct AttributeList {
    _buf: Vec<u8>,
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttributeList {
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

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.ptr
    }

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

struct OwnedSecurityCapabilities {
    _app_sid: LocalSid,
    _cap_sids: Vec<LocalSid>,
    _caps: Box<[SID_AND_ATTRIBUTES]>,
    sc: SECURITY_CAPABILITIES,
}

impl OwnedSecurityCapabilities {
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

    fn as_ptr(&self) -> *const SECURITY_CAPABILITIES {
        &self.sc
    }
}

struct LocalSid {
    psid: PSID,
}

impl LocalSid {
    fn from_sddl(sddl: &str) -> Result<Self, SandboxError> {
        let wide = wide_str(sddl);
        let mut psid = PSID(ptr::null_mut());
        unsafe {
            ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut psid)
                .map_err(|err| launch_err("ConvertStringSidToSidW", &err.to_string()))?;
        }
        Ok(Self { psid })
    }

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

fn file_from_handle(handle: HANDLE) -> File {
    unsafe { File::from_raw_handle(handle.0 as RawHandle) }
}

fn cleanup_handles(handles: &[HANDLE]) {
    for &h in handles {
        if !h.is_invalid() && h != HANDLE::default() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

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

fn launch_err(stage: &str, detail: &str) -> SandboxError {
    SandboxError::Backend {
        label: "appcontainer".into(),
        backend: "appcontainer",
        detail: format!("{stage}: {detail}"),
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
