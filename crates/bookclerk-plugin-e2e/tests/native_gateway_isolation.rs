//! Concurrent native-behind-workerd sessions keep separate grants and state.
//!
//! Overlapping launches also exercise the live proxy challenge. Another
//! session's secret on this link closes it, and an unrelated child without
//! `BOOKCLERK_SESSION_CHALLENGE` cannot complete a handshake on an endpoint it
//! can see. After both sessions have completed that handshake, each guest asks
//! the proxy it actually holds for that proxy's channel tag. A's tag is A's,
//! not B's, whatever B's numeric fd or handle is. The same RPC reports
//! `extra_status = absent` because production handoff does not add the
//! test-only extra endpoint. A missing or failed observation is not absence.
//! A child of B that does not inherit still cannot open B's endpoint. That
//! child check is the parent-to-child boundary, not the A-to-B one.
//!
//! On Windows, inheritable event sentinels are created before `Install::spawn`
//! and are not placed on `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
//! `GetHandleInformation` on the inherited proxy must succeed before and after
//! the probe. Each sentinel is `DuplicateHandle` only. Denial is Win32 5 or 6
//! on every sentinel. A duplicate is not success: `SetEvent` on that copy plus
//! `WaitForSingleObject(0)` on the host event distinguishes an inherited object
//! from a numeric collision, and both fail the test. Unix reports
//! `unsupported` and that result is not a denial.

#[path = "native_gateway/channel.rs"]
mod channel;
#[path = "native_gateway/harness.rs"]
mod ng_harness;

use std::time::SystemTime;

use bookclerk_plugin_host::{TEST_CHANNEL_IDENT_ENV, TEST_CHANNEL_TAG_FILE};
use ng_harness::{
    assert_no_session_dirs, error_text, open_session, probe, step, wait_for_exit, Install, Listener,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_keep_separate_grants_and_state() {
    // Host-only. Each proxy answers its own tag; the guest is not given it.
    std::env::set_var(TEST_CHANNEL_IDENT_ENV, "1");
    // The proxy is still waiting for its 32-byte challenge until the first
    // connect. These two sessions exist only to exercise that handshake.
    let listener_gate_a = Listener::bind(true).await;
    let listener_gate_b = Listener::bind(true).await;
    let install_gate_a = Install::new(listener_gate_a.port);
    let install_gate_b = Install::new(listener_gate_b.port);
    let (gate_a, gate_b) = tokio::join!(install_gate_a.spawn(), install_gate_b.spawn());
    tokio::join!(open_session(&gate_a), open_session(&gate_b));

    let challenge_b = probe(&gate_b, "session_challenge", 0, "").await;
    assert_eq!(
        challenge_b["ok"], true,
        "B did not publish a challenge: {challenge_b}"
    );
    let challenge_hex = challenge_b["hex"].as_str().unwrap_or("").to_string();
    assert_eq!(
        challenge_hex.len(),
        64,
        "B challenge hex was not 32 bytes (len {})",
        challenge_hex.len()
    );
    let (foreign, child) = tokio::join!(
        probe(&gate_a, "present_challenge", 0, &challenge_hex),
        probe(&gate_b, "unrelated_challenge", 0, "inherit"),
    );
    assert_eq!(foreign["unsupported"], false, "{foreign}");
    assert_eq!(
        foreign["wrote"], true,
        "B's challenge was not written on A's link: {foreign}"
    );
    assert_eq!(foreign["opened_stream"], false, "{foreign}");
    assert_eq!(foreign["completed"], false, "{foreign}");
    assert!(
        foreign["closed"] == true || foreign["refused"] == true,
        "B's challenge on A's link must close or refuse: {foreign}"
    );
    assert_eq!(child["unsupported"], false, "{child}");
    assert_eq!(child["completed"], false, "{child}");
    assert_eq!(child["challenge_env"], false, "{child}");
    assert_eq!(child["opened_stream"], false, "{child}");
    let attempts = child["attempts"].as_array().expect("child attempts");
    assert!(!attempts.is_empty(), "{child}");
    for attempt in attempts {
        assert_eq!(attempt["completed"], false, "{child}");
        assert_eq!(attempt["opened_stream"], false, "{child}");
        let err = attempt["error"].as_str().unwrap_or("");
        assert!(!err.to_ascii_lowercase().contains("unsupported"), "{child}");
        assert!(!err.contains("Unix-only"), "{child}");
        assert!(!err.contains("fd identity"), "{child}");
    }
    #[cfg(unix)]
    {
        let primary = &attempts[0];
        assert_eq!(
            primary["wrote"], true,
            "unrelated child could not see the live proxy: {child}"
        );
        assert_eq!(
            primary["closed"], true,
            "live proxy did not close on the child's zeros: {child}"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        listener_gate_a.accepts(),
        0,
        "foreign challenge produced an accept"
    );
    assert_eq!(
        listener_gate_b.accepts(),
        0,
        "unrelated child produced an accept"
    );
    step(&format!(
        "live endpoint: B's challenge on A's link closed={} refused={} opened_stream=false; unrelated child completed=false wrote={} closed={}",
        foreign["closed"], foreign["refused"], child["wrote"], child["closed"]
    ));
    let gate_gateway_a = gate_a.gateway_pid().expect("gate A gateway");
    let gate_guest_a = gate_a.guest_pid().expect("gate A guest");
    let gate_gateway_b = gate_b.gateway_pid().expect("gate B gateway");
    let gate_guest_b = gate_b.guest_pid().expect("gate B guest");
    drop(gate_a);
    drop(gate_b);
    wait_for_exit(gate_gateway_a).await;
    wait_for_exit(gate_guest_a).await;
    wait_for_exit(gate_gateway_b).await;
    wait_for_exit(gate_guest_b).await;
    assert_no_session_dirs(install_gate_a.files_dir()).await;
    assert_no_session_dirs(install_gate_b.files_dir()).await;

    let listener_a = Listener::bind(true).await;
    let listener_b = Listener::bind(true).await;
    assert_ne!(listener_a.port, listener_b.port);
    let install_a = Install::new(listener_a.port);
    let install_b = Install::new(listener_b.port);
    // Live inheritable objects, omitted from the guest handle allowlist.
    // They must already exist when the guest `CreateProcess` runs.
    #[cfg(windows)]
    let sentinels = SentinelEvents::create(4);

    let (session_a, session_b) = tokio::join!(install_a.spawn(), install_b.spawn());
    tokio::join!(open_session(&session_a), open_session(&session_b));

    let dir_a = session_a
        .session_dir()
        .expect("A session dir")
        .to_path_buf();
    let dir_b = session_b
        .session_dir()
        .expect("B session dir")
        .to_path_buf();
    assert_ne!(dir_a, dir_b);
    let secret_b = dir_b.join("host-secret.txt");
    std::fs::write(&secret_b, b"session-b-private").expect("write B secret");

    let env_a = probe(&session_a, "env_keys", 0, "").await;
    let env_b = probe(&session_b, "env_keys", 0, "").await;
    let proxy_a = env_a["socket_proxy"].as_str().unwrap_or("").to_string();
    let proxy_b = env_b["socket_proxy"].as_str().unwrap_or("").to_string();
    assert!(
        proxy_a.starts_with("fd:") || proxy_a.starts_with("handle:"),
        "A SOCKET_PROXY: {proxy_a}"
    );
    assert!(
        proxy_b.starts_with("fd:") || proxy_b.starts_with("handle:"),
        "B SOCKET_PROXY: {proxy_b}"
    );
    let unlisted = {
        #[cfg(windows)]
        {
            sentinels.payload()
        }
        #[cfg(not(windows))]
        {
            "0".to_string()
        }
    };
    let handles = probe(&session_a, "unlisted_handle", 0, &unlisted).await;
    #[cfg(windows)]
    {
        assert_sentinels_denied(&sentinels, &handles);
        let rows = handles["sentinels"].as_array().expect("sentinels");
        for row in rows {
            step(&format!(
                "unlisted sentinel {} denied os {}",
                row["value"], row["os"]
            ));
        }
        step(&format!(
            "inherited proxy handle usable before={} after={}",
            handles["proxy_usable"], handles["proxy_usable_after"]
        ));
    }
    #[cfg(not(windows))]
    {
        assert_eq!(handles["unsupported"], true, "{handles}");
        assert_ne!(
            handles["denied"], true,
            "unsupported must not count as a denial: {handles}"
        );
        step(
            "unlisted-handle probe unsupported on this platform; denial was not asserted; unsupported is not a denial",
        );
    }
    step(&format!(
        "A session={} B session={} B proxy={proxy_b}",
        dir_a.display(),
        dir_b.display()
    ));

    let stolen = probe(&session_a, "read_path", 0, &secret_b.display().to_string()).await;
    assert_eq!(
        stolen["ok"], false,
        "session A read session B's gateway state: {stolen}"
    );

    let payload = format!(
        "iso-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let before_b = listener_b.accepts();
    let cross = probe(&session_a, "connect", listener_b.port, &payload).await;
    assert_eq!(
        cross["ok"], false,
        "A reached B's granted port through A's proxy: {cross}"
    );
    let error = error_text(&cross);
    assert!(
        error.contains("403") || error.contains("refused") || error.contains("denied"),
        "cross-session connect must be a policy denial, got: {error}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        listener_b.accepts(),
        before_b,
        "A's traffic produced accepts on B's listener"
    );

    let ok_a = probe(&session_a, "connect", listener_a.port, &payload).await;
    assert_eq!(ok_a["ok"], true, "A lost its own grant: {ok_a}");
    let ok_b = probe(&session_b, "connect", listener_b.port, &payload).await;
    assert_eq!(ok_b["ok"], true, "B lost its own grant: {ok_b}");
    assert!(listener_a.wait_for_accepts(1).await);
    assert!(listener_b.wait_for_accepts(before_b + 1).await);

    let tag_a = read_channel_tag(&dir_a);
    let tag_b = read_channel_tag(&dir_b);
    assert_ne!(tag_a, tag_b, "overlapping sessions shared a channel tag");
    let (ident_a, ident_b) = tokio::join!(
        probe(&session_a, "channel_ident", 0, ""),
        probe(&session_b, "channel_ident", 0, ""),
    );
    assert_eq!(ident_a["ok"], true, "A did not read its channel: {ident_a}");
    assert_eq!(ident_b["ok"], true, "B did not read its channel: {ident_b}");
    let reported_a = ident_a["tag"].as_str().unwrap_or("");
    let reported_b = ident_b["tag"].as_str().unwrap_or("");
    let tags_a = channel::observed_channel_tags(&ident_a);
    let tags_b = channel::observed_channel_tags(&ident_b);
    let observed_a: Vec<&str> = tags_a.iter().map(String::as_str).collect();
    let observed_b: Vec<&str> = tags_b.iter().map(String::as_str).collect();
    assert_eq!(
        reported_a, tag_a,
        "A's endpoint was not the channel handed to A: {ident_a}"
    );
    assert_eq!(
        reported_b, tag_b,
        "B's endpoint was not the channel handed to B: {ident_b}"
    );
    assert!(
        observed_a.contains(&reported_a),
        "A's configured tag was missing from the observed set: {ident_a}"
    );
    assert!(
        observed_b.contains(&reported_b),
        "B's configured tag was missing from the observed set: {ident_b}"
    );
    assert_eq!(
        channel::channel_endpoint_isolation(&ident_a, &tag_b),
        Ok(true),
        "A did not prove B's endpoint was absent: {ident_a}"
    );
    assert_eq!(
        channel::channel_endpoint_isolation(&ident_b, &tag_a),
        Ok(true),
        "B did not prove A's endpoint was absent: {ident_b}"
    );
    let accepts_before_child = listener_b.accepts();
    let drive_child = probe(&session_b, "unrelated_drive", listener_b.port, "").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        listener_b.accepts(),
        accepts_before_child,
        "the child of B produced an accept: {drive_child}"
    );
    assert_endpoint_sealed("unrelated child", &drive_child);
    #[cfg(windows)]
    let sentinel_unsupported = false;
    #[cfg(not(windows))]
    let sentinel_unsupported = true;
    step(&format!(
        "channel identity: A tag={reported_a} tags={observed_a:?} extra_status={} B tag={reported_b} tags={observed_b:?} extra_status={}; both extra endpoints absent; child opened_stream={} reached_proxy={} not_inherited={} collided={}; sentinel_unsupported={sentinel_unsupported}; numeric collision is not identity; B accepts unchanged",
        ident_a["extra_status"], ident_b["extra_status"],
        drive_child["opened_stream"],
        drive_child["reached_proxy"],
        drive_child["not_inherited"],
        drive_child["collided"],
    ));

    let gateway_a = session_a.gateway_pid().expect("A gateway");
    let guest_a = session_a.guest_pid().expect("A guest");
    let gateway_b = session_b.gateway_pid().expect("B gateway");
    let guest_b = session_b.guest_pid().expect("B guest");
    drop(session_a);
    drop(session_b);
    wait_for_exit(gateway_a).await;
    wait_for_exit(guest_a).await;
    wait_for_exit(gateway_b).await;
    wait_for_exit(guest_b).await;
    assert_no_session_dirs(install_a.files_dir()).await;
    assert_no_session_dirs(install_b.files_dir()).await;
    step("both sessions tore down cleanly");
}

fn read_channel_tag(dir: &std::path::Path) -> String {
    let text = std::fs::read_to_string(dir.join(TEST_CHANNEL_TAG_FILE)).unwrap_or_else(|err| {
        panic!(
            "channel tag {} in {}: {err}",
            TEST_CHANNEL_TAG_FILE,
            dir.display()
        )
    });
    let tag = text.trim().to_string();
    assert!(!tag.is_empty(), "empty channel tag in {}", dir.display());
    tag
}

/// The child did not inherit a usable authenticated endpoint.
///
/// A collision in some other process is not this result. `not_inherited` means
/// the child could not open the parent's value, or the object at that value
/// is not the pipe (stdio, another file type, or a non-file). Inheritance
/// would have kept the pipe itself at that value. `reached_proxy` would mean
/// the mux server accepted a stream.
fn assert_endpoint_sealed(label: &str, outcome: &serde_json::Value) {
    assert_ne!(
        outcome["unsupported"], true,
        "{label} unsupported is not a result: {outcome}"
    );
    assert_ne!(
        outcome["collided"], true,
        "{label} collision is not absence: {outcome}"
    );
    assert_eq!(
        outcome["opened_stream"], false,
        "{label} opened a stream on the authenticated proxy: {outcome}"
    );
    assert_ne!(
        outcome["reached_proxy"], true,
        "{label} reached the authenticated proxy: {outcome}"
    );
    assert_eq!(
        outcome["not_inherited"], true,
        "{label} did not show the endpoint was absent from the child: {outcome}"
    );
}

/// Inheritable manual-reset events created before the guest is spawned.
///
/// Drop closes them. They are not added to the process handle allowlist.
/// Their values sit above the range a fresh guest allocates. A low value such
/// as 208 can already name some other object in that guest, and
/// `DuplicateHandle` then succeeds without inheriting the event.
#[cfg(windows)]
struct SentinelEvents {
    handles: Vec<usize>,
}

#[cfg(windows)]
impl SentinelEvents {
    /// `count` unnamed manual-reset events, nonsignaled, `bInheritHandle = TRUE`.
    ///
    /// Low handle slots stay occupied until those events exist, then the
    /// fillers are closed. The sentinels remain open and inheritable. A
    /// duplicate of one of these values is the omitted event, not another
    /// object that happened to reuse a low slot.
    #[allow(unsafe_code)]
    fn create(count: usize) -> Self {
        extern "system" {
            fn CreateEventW(
                attrs: *mut core::ffi::c_void,
                manual: i32,
                initial: i32,
                name: *const u16,
            ) -> *mut core::ffi::c_void;
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        }
        #[repr(C)]
        struct SecurityAttributes {
            n_length: u32,
            lp_security_descriptor: *mut core::ffi::c_void,
            b_inherit_handle: i32,
        }
        /// Occupied low slots. Closed before spawn so they cannot be inherited.
        struct CloseFillers(Vec<*mut core::ffi::c_void>);
        impl Drop for CloseFillers {
            fn drop(&mut self) {
                extern "system" {
                    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
                }
                for handle in self.0.drain(..) {
                    unsafe {
                        CloseHandle(handle);
                    }
                }
            }
        }
        let open_event = |inherit: bool| {
            let mut attrs = SecurityAttributes {
                n_length: std::mem::size_of::<SecurityAttributes>() as u32,
                lp_security_descriptor: core::ptr::null_mut(),
                b_inherit_handle: i32::from(inherit),
            };
            let handle = unsafe {
                CreateEventW(
                    core::ptr::addr_of_mut!(attrs).cast(),
                    1,
                    0,
                    core::ptr::null(),
                )
            };
            assert!(
                !handle.is_null(),
                "CreateEventW failed: {}",
                std::io::Error::last_os_error()
            );
            handle
        };
        // Value 208 collided with an unrelated guest object on Windows CI.
        // A fresh guest does not open enough handles to reach this value.
        const MIN_VALUE: usize = 0x4000;
        const MAX_ALLOCS: usize = 8192;
        let mut fillers = CloseFillers(Vec::new());
        let mut created = Self {
            handles: Vec::with_capacity(count),
        };
        while created.handles.len() < count {
            assert!(
                fillers.0.len() + created.handles.len() < MAX_ALLOCS,
                "could not allocate {count} sentinel handles at or above {MIN_VALUE:#x}"
            );
            let probe = open_event(false);
            if (probe as usize) < MIN_VALUE {
                fillers.0.push(probe);
                continue;
            }
            unsafe {
                CloseHandle(probe);
            }
            let handle = open_event(true);
            if handle as usize >= MIN_VALUE {
                created.handles.push(handle as usize);
            } else {
                fillers.0.push(handle);
            }
        }
        drop(fillers);
        created
    }

    fn payload(&self) -> String {
        self.handles
            .iter()
            .map(|handle| (*handle as u64).to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// `WAIT_OBJECT_0` means this process's event was signaled.
    #[allow(unsafe_code)]
    fn signaled(&self, value: u64) -> bool {
        extern "system" {
            fn WaitForSingleObject(handle: *mut core::ffi::c_void, millis: u32) -> u32;
        }
        let handle = self
            .handles
            .iter()
            .copied()
            .find(|candidate| *candidate as u64 == value)
            .unwrap_or_else(|| panic!("sentinel {value} was not created by this process"));
        let waited = unsafe { WaitForSingleObject(handle as *mut core::ffi::c_void, 0) };
        waited == 0
    }
}

#[cfg(windows)]
impl Drop for SentinelEvents {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        extern "system" {
            fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        }
        for handle in &self.handles {
            unsafe {
                CloseHandle(*handle as *mut core::ffi::c_void);
            }
        }
    }
}

/// Every sentinel is Win32 5 or 6. A duplicate fails the test: signaled means
/// the omitted object was inherited, and an unsignaled duplicate is a numeric
/// collision rather than a skip.
#[cfg(windows)]
fn assert_sentinels_denied(sentinels: &SentinelEvents, report: &serde_json::Value) {
    assert_eq!(report["unsupported"], false, "{report}");
    assert_eq!(
        report["proxy_usable"], true,
        "inherited proxy handle was not usable: {report}"
    );
    assert_eq!(
        report["proxy_usable_after"], true,
        "inherited proxy handle was not usable after the sentinel probe: {report}"
    );
    let rows = report["sentinels"].as_array().expect("sentinel rows");
    assert_eq!(rows.len(), 4, "every sentinel must be reported: {report}");
    for row in rows {
        let value = row["value"].as_u64().unwrap_or_else(|| {
            panic!("sentinel value was not a number: {row}");
        });
        if row["duplicated"] == true {
            if sentinels.signaled(value) {
                panic!("omitted sentinel {value} was inherited: {report}");
            }
            panic!("sentinel {value} duplicated a different object (numeric collision): {report}");
        }
        let os = row["os"].as_u64().unwrap_or(0);
        assert!(
            os == 5 || os == 6,
            "sentinel {value} must be access-denied or invalid, not skipped: {report}"
        );
    }
    assert_eq!(report["denied"], true, "{report}");
}
