//! Gates for spawn-time confinement (Windows AppContainer).
//!
//! Self-confine proofs live in `enforcement.rs` and require
//! `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT`. Spawn-time confinement is demanded
//! separately so Windows CI can fail closed without claiming Landlock/Seatbelt.

const REQUIRE_SPAWN: &str = "BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT";

fn spawn_demanded() -> bool {
    std::env::var(REQUIRE_SPAWN).is_ok_and(|value| !value.trim().is_empty())
}

#[test]
fn spawn_capability_matches_platform_expectations() {
    let caps = bookclerk_sandbox::capabilities();
    assert!(
        caps.can_confine_guest() || !spawn_demanded(),
        "{REQUIRE_SPAWN} is set but this host cannot confine a guest: {} [{}]",
        caps.detail,
        caps.backend
    );

    #[cfg(windows)]
    {
        use bookclerk_sandbox::{NetPolicy, Policy};

        assert!(
            caps.spawn_filesystem,
            "Windows must advertise spawn_filesystem; got {} [{}]",
            caps.detail, caps.backend
        );
        assert!(
            !caps.filesystem,
            "Windows must not claim self-confine filesystem"
        );
        let plan = bookclerk_sandbox::spawn::plan_appcontainer(
            &Policy::new("spawn-test").net(NetPolicy::Outbound),
        )
        .expect("plan_appcontainer");
        assert!(
            plan.package_sid.is_some(),
            "profile SID required on Windows"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        assert!(
            caps.filesystem || !spawn_demanded(),
            "unix hosts confine via self-confine; filesystem should be available"
        );
        assert!(!caps.spawn_filesystem);
    }
}
