//! Compile plugin Cap'n Proto schemas into Rust (Cap'n Proto RPC interfaces).

fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/plugin.capnp")
        .file("schema/plugin_host.capnp")
        .run()
        .expect("compile plugin capnp schemas");
}
