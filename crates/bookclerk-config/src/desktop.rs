//! Detect whether a graphical desktop / systray session is available.

/// True when it is reasonable to start the in-process tray (windowing + session bus).
///
/// Linux requires a display (`DISPLAY` or `WAYLAND_DISPLAY`) **and** a session-bus
/// signal (`DBUS_SESSION_BUS_ADDRESS` or `XDG_RUNTIME_DIR`). Windows and macOS
/// return true; headless CI/servers on Linux return false.
#[must_use]
pub fn graphical_session_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        let has_display = env_nonempty("WAYLAND_DISPLAY") || env_nonempty("DISPLAY");
        let has_session_bus = dbus_session_bus_address_is_set() || xdg_runtime_dir_is_set();
        has_display && has_session_bus
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        true
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn dbus_session_bus_address_is_set() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty())
}

#[cfg(target_os = "linux")]
fn xdg_runtime_dir_is_set() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|v| !v.is_empty())
}

#[cfg(any(test, target_os = "linux"))]
fn env_nonempty(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

#[cfg(test)]
mod tests {
    use super::{env_nonempty, graphical_session_available, restore_env};

    #[test]
    fn env_nonempty_rejects_missing_and_empty() {
        let key = "BOOKCLERK_TEST_DESKTOP_ENV_EMPTY";
        std::env::remove_var(key);
        assert!(!env_nonempty(key));
        std::env::set_var(key, "");
        assert!(!env_nonempty(key));
        std::env::set_var(key, "1");
        assert!(env_nonempty(key));
        std::env::remove_var(key);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn graphical_session_available_requires_display_and_session_bus() {
        let display_key = "DISPLAY";
        let wayland_key = "WAYLAND_DISPLAY";
        let bus_key = "DBUS_SESSION_BUS_ADDRESS";
        let runtime_key = "XDG_RUNTIME_DIR";

        let prev_display = std::env::var_os(display_key);
        let prev_wayland = std::env::var_os(wayland_key);
        let prev_bus = std::env::var_os(bus_key);
        let prev_runtime = std::env::var_os(runtime_key);

        // Display alone is not enough (common under systemd with only XDG_RUNTIME_DIR).
        std::env::set_var(display_key, ":0");
        std::env::remove_var(wayland_key);
        std::env::remove_var(bus_key);
        std::env::remove_var(runtime_key);
        assert!(!graphical_session_available());

        // Session bus alone is not enough.
        std::env::remove_var(display_key);
        std::env::set_var(runtime_key, "/run/user/1000");
        assert!(!graphical_session_available());

        // Display + runtime dir qualifies.
        std::env::set_var(display_key, ":0");
        assert!(graphical_session_available());

        restore_env(display_key, prev_display);
        restore_env(wayland_key, prev_wayland);
        restore_env(bus_key, prev_bus);
        restore_env(runtime_key, prev_runtime);
    }
}
