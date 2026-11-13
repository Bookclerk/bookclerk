//! `bookclerkd` — long-running scan / acquire daemon with HTTP control plane.
//!
//! The host lives in the library half of this package; this binary parses
//! the command line and runs it behind the HTTP listeners.

use clap::Parser as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bookclerkd::run(bookclerkd::Args::parse()).await
}
