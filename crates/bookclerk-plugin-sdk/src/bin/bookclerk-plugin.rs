//! Authoring CLI baked into the Rust guest SDK.

fn main() -> std::process::ExitCode {
    bookclerk_plugin_sdk::tools::run_tools_cli()
}
