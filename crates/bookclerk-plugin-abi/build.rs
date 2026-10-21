//! Compile plugin Cap'n Proto schemas into Rust (Cap'n Proto RPC interfaces).

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(clippy::expect_used)]

fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/plugin.capnp")
        .file("schema/plugin_host.capnp")
        .run()
        .expect("compile plugin capnp schemas");
}
