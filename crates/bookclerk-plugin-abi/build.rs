//! Compile `schema/plugin_v2.capnp` into Rust (Cap'n Proto RPC interfaces).

fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/plugin_v2.capnp")
        .run()
        .expect("compile schema/plugin_v2.capnp");
}
