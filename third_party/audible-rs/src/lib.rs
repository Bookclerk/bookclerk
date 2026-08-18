//! Core library of `audible-rs`, a Rust reimplementation of
//! [mkb79/Audible](https://github.com/mkb79/Audible) and
//! [mkb79/audible-cli](https://github.com/mkb79/audible-cli).
//!
//! The public library surface is `api`, `auth`, `models`, `downloader`,
//! and `widevine`. Remaining modules back the `audible` binary and compile
//! only with the `cli` feature.

pub mod api;
pub mod auth;
pub mod crypto;
pub mod downloader;
pub(crate) mod fsutil;
pub mod library_sync;
pub mod models;
pub(crate) mod timefmt;
pub mod widevine;

#[cfg(feature = "cli")]
pub mod activation;
#[cfg(feature = "cli")]
pub mod catalog;
#[cfg(feature = "cli")]
pub mod collections;
#[cfg(feature = "cli")]
pub mod commands;
#[cfg(feature = "cli")]
pub mod config;
#[cfg(feature = "cli")]
pub mod db;
#[cfg(feature = "cli")]
pub mod naming;
#[cfg(feature = "cli")]
pub mod output;
#[cfg(feature = "cli")]
pub mod plugins;
#[cfg(feature = "cli")]
pub mod session;
