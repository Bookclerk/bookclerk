//! Bookclerk desktop entrypoint (Tauri shell + tray).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    bookclerk_desktop_lib::run();
}
