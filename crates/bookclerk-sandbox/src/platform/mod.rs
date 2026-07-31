//! Per-OS confinement backends.
//!
//! Each backend exposes the same items so [`crate::Policy`] stays
//! platform-agnostic: `BACKEND`, `system_read_paths`, `system_write_paths`,
//! `confine_current_process`, and `capabilities`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
    capabilities, confine_current_process, system_read_paths, system_write_paths, BACKEND,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    capabilities, confine_current_process, system_read_paths, system_write_paths, BACKEND,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    capabilities, confine_current_process, system_read_paths, system_write_paths, BACKEND,
};

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub use unsupported::{
    capabilities, confine_current_process, system_read_paths, system_write_paths, BACKEND,
};
