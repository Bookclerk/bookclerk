//! Emits `schema/plugin.layout.json` from the Cap'n Proto compiler.
//!
//! The SDK code generator (`scripts/sdk_emitters.py`) needs struct data-word
//! counts, pointer counts, per-field offsets, and union discriminants to emit
//! wire codecs. Those numbers are owned by the compiler's layout algorithm,
//! so this tool runs `capnp compile -o-` on the hand-written schemas and
//! projects the resulting `CodeGeneratorRequest` into a stable JSON document
//! that the Python emitter consumes. Nothing here is hand-derived.
//!
//! Usage: `cargo run -q -p abi-layout -- [--schema-dir DIR] [--out FILE]`.
//! With no `--out` the JSON is written to stdout so drift checks can compare
//! without touching the tree.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use capnp::schema_capnp::{code_generator_request, field, node, type_, value};
use capnp::serialize;
use serde::Serialize;

/// Schema files compiled into the layout document, in dependency order.
const SCHEMA_FILES: &[&str] = &["plugin.capnp", "plugin_host.capnp"];

/// Wire type of one field or list element.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum WireType {
    /// Zero-size marker (union members with no payload).
    Void,
    /// One bit.
    Bool,
    /// Signed 8-bit integer.
    Int8,
    /// Signed 16-bit integer.
    Int16,
    /// Signed 32-bit integer.
    Int32,
    /// Signed 64-bit integer.
    Int64,
    /// Unsigned 8-bit integer.
    Uint8,
    /// Unsigned 16-bit integer.
    Uint16,
    /// Unsigned 32-bit integer.
    Uint32,
    /// Unsigned 64-bit integer.
    Uint64,
    /// IEEE-754 binary32.
    Float32,
    /// IEEE-754 binary64.
    Float64,
    /// NUL-terminated UTF-8 byte list.
    Text,
    /// Raw byte list.
    Data,
    /// Homogeneous list of `element`.
    List {
        /// Element type.
        element: Box<WireType>,
    },
    /// 16-bit enum ordinal of the named enum.
    Enum {
        /// Schema-qualified enum name.
        name: String,
    },
    /// Pointer to a struct of the named type.
    Struct {
        /// Schema-qualified struct name.
        name: String,
    },
    /// Capability pointer to the named interface.
    Interface {
        /// Schema-qualified interface name.
        name: String,
    },
    /// Untyped pointer.
    AnyPointer,
}

/// One struct field with its compiler-assigned slot.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldLayout {
    /// Field name as written in the schema.
    name: String,
    /// Declaration order (the `@N` ordinal).
    code_order: u16,
    /// Union discriminant value, or `None` for ordinary fields.
    discriminant_value: Option<u16>,
    /// Wire type.
    #[serde(rename = "type")]
    ty: WireType,
    /// Slot offset in units of the field's own size (bits for `Bool`, 16-bit
    /// units for `UInt16`/enums, pointer index for pointer types).
    offset: u32,
}

/// Layout of one struct (including implicit method parameter/result structs).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StructLayout {
    /// Schema-qualified name (`Outer.Inner` for nested scopes; method
    /// envelopes are `Interface.method$Params` / `Interface.method$Results`).
    name: String,
    /// Schema file the struct is declared in.
    file: String,
    /// Data section size in 64-bit words.
    data_words: u16,
    /// Pointer section size in pointers.
    pointer_count: u16,
    /// Number of union members (`0` when the struct has no union).
    discriminant_count: u16,
    /// Discriminant slot offset in 16-bit units.
    discriminant_offset: u32,
    /// Fields in declaration order.
    fields: Vec<FieldLayout>,
}

/// Layout of one enum.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnumLayout {
    /// Schema-qualified name.
    name: String,
    /// Schema file the enum is declared in.
    file: String,
    /// Enumerants in ordinal order (index = wire value).
    enumerants: Vec<String>,
}

/// One RPC method with its envelope struct names.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MethodLayout {
    /// Method name.
    name: String,
    /// Method ordinal.
    ordinal: u16,
    /// Name of the implicit parameter struct in `structs`.
    params: String,
    /// Name of the implicit result struct in `structs`.
    results: String,
}

/// Layout of one interface.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InterfaceLayout {
    /// Schema-qualified name.
    name: String,
    /// Schema file the interface is declared in.
    file: String,
    /// Methods in ordinal order.
    methods: Vec<MethodLayout>,
}

/// One file-scope constant.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConstLayout {
    /// Constant name.
    name: String,
    /// Schema file the constant is declared in.
    file: String,
    /// Constant value (`UInt32` or `Text` in the plugin schema).
    value: serde_json::Value,
}

/// The whole layout document.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayoutDoc {
    /// Provenance marker for readers.
    generator: &'static str,
    /// Compiler version string from `capnp --version`.
    capnp_version: String,
    /// Compiled schema files in order.
    files: Vec<String>,
    /// Structs keyed by schema-qualified name.
    structs: BTreeMap<String, StructLayout>,
    /// Enums keyed by schema-qualified name.
    enums: BTreeMap<String, EnumLayout>,
    /// Interfaces keyed by schema-qualified name.
    interfaces: BTreeMap<String, InterfaceLayout>,
    /// Constants keyed by name.
    consts: BTreeMap<String, ConstLayout>,
}

/// Parsed command line.
struct Args {
    /// Directory holding the schema files.
    schema_dir: PathBuf,
    /// Output path, or `None` for stdout.
    out: Option<PathBuf>,
}

/// Parses argv; unknown flags are errors.
fn parse_args() -> Result<Args> {
    let mut schema_dir = None;
    let mut out = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--schema-dir" => {
                schema_dir = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow!("--schema-dir needs a value"))?,
                ));
            }
            "--out" => {
                out = Some(PathBuf::from(
                    it.next().ok_or_else(|| anyhow!("--out needs a value"))?,
                ));
            }
            other => bail!("unknown argument {other}"),
        }
    }
    let schema_dir = schema_dir.unwrap_or_else(default_schema_dir);
    Ok(Args { schema_dir, out })
}

/// `crates/bookclerk-plugin-abi/schema` relative to the workspace root.
fn default_schema_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("crates/bookclerk-plugin-abi/schema"))
        .unwrap_or_else(|| PathBuf::from("crates/bookclerk-plugin-abi/schema"))
}

/// Runs `capnp compile -o-` and returns the raw `CodeGeneratorRequest` bytes.
fn compile(schema_dir: &Path) -> Result<Vec<u8>> {
    let mut cmd = Command::new("capnp");
    cmd.arg("compile")
        .arg("-o-")
        .arg(format!("--src-prefix={}", schema_dir.display()));
    for file in SCHEMA_FILES {
        cmd.arg(schema_dir.join(file));
    }
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .context("spawn `capnp compile` (is capnproto installed?)")?;
    if !output.status.success() {
        bail!(
            "capnp compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

/// `capnp --version` text, trimmed.
fn capnp_version() -> Result<String> {
    let output = Command::new("capnp")
        .arg("--version")
        .output()
        .context("run `capnp --version`")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Node lookup with file attribution and display names.
struct Nodes<'a> {
    /// Every node by id.
    by_id: BTreeMap<u64, node::Reader<'a>>,
    /// Requested file id -> basename.
    files: BTreeMap<u64, String>,
    /// Implicit method parameter/result struct id -> qualified name and
    /// owning file. These nodes have `scopeId == 0` in the compiler output,
    /// so the interface that declares the method is recorded here instead.
    envelopes: BTreeMap<u64, String>,
    /// Envelope struct id -> owning file (mirrors `envelopes`).
    envelope_files: BTreeMap<u64, String>,
}

impl<'a> Nodes<'a> {
    /// Schema-qualified name: the node's own short name prefixed by every
    /// enclosing non-file scope (`Interface.method$Params`, `Outer.Inner`).
    fn name_of(&self, id: u64) -> Result<String> {
        if let Some(name) = self.envelopes.get(&id) {
            return Ok(name.clone());
        }
        let mut parts = Vec::new();
        let mut cur = id;
        for _ in 0..64 {
            if cur == 0 {
                break;
            }
            let node = self
                .by_id
                .get(&cur)
                .ok_or_else(|| anyhow!("dangling node id {cur:#x}"))?;
            if self.files.contains_key(&cur) || matches!(node.which()?, node::File(())) {
                break;
            }
            let display = node.get_display_name()?.to_str()?;
            let prefix = node.get_display_name_prefix_length() as usize;
            parts.push(display[prefix.min(display.len())..].to_string());
            cur = node.get_scope_id();
        }
        parts.reverse();
        Ok(parts.join("."))
    }

    /// Walks scope ids up to the owning file node.
    fn file_of(&self, id: u64) -> Result<String> {
        if let Some(file) = self.envelope_files.get(&id) {
            return Ok(file.clone());
        }
        let mut cur = id;
        for _ in 0..64 {
            if cur == 0 {
                bail!("node {id:#x} has no owning file");
            }
            let node = self
                .by_id
                .get(&cur)
                .ok_or_else(|| anyhow!("dangling scope id {cur:#x}"))?;
            if let Some(file) = self.files.get(&cur) {
                return Ok(file.clone());
            }
            if matches!(node.which()?, node::File(())) {
                let display = node.get_display_name()?.to_str()?;
                return Ok(Path::new(display)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| display.to_string()));
            }
            cur = node.get_scope_id();
        }
        bail!("scope chain too deep for node {id:#x}")
    }

    /// Projects a compiler `Type` into [`WireType`].
    fn wire_type(&self, ty: type_::Reader<'a>) -> Result<WireType> {
        Ok(match ty.which()? {
            type_::Void(()) => WireType::Void,
            type_::Bool(()) => WireType::Bool,
            type_::Int8(()) => WireType::Int8,
            type_::Int16(()) => WireType::Int16,
            type_::Int32(()) => WireType::Int32,
            type_::Int64(()) => WireType::Int64,
            type_::Uint8(()) => WireType::Uint8,
            type_::Uint16(()) => WireType::Uint16,
            type_::Uint32(()) => WireType::Uint32,
            type_::Uint64(()) => WireType::Uint64,
            type_::Float32(()) => WireType::Float32,
            type_::Float64(()) => WireType::Float64,
            type_::Text(()) => WireType::Text,
            type_::Data(()) => WireType::Data,
            type_::List(list) => WireType::List {
                element: Box::new(self.wire_type(list.get_element_type()?)?),
            },
            type_::Enum(e) => WireType::Enum {
                name: self.name_of(e.get_type_id())?,
            },
            type_::Struct(s) => WireType::Struct {
                name: self.name_of(s.get_type_id())?,
            },
            type_::Interface(i) => WireType::Interface {
                name: self.name_of(i.get_type_id())?,
            },
            type_::AnyPointer(_) => WireType::AnyPointer,
        })
    }
}

/// Builds the layout document from a decoded `CodeGeneratorRequest`.
fn build(req: code_generator_request::Reader<'_>) -> Result<LayoutDoc> {
    let mut nodes = Nodes {
        by_id: BTreeMap::new(),
        files: BTreeMap::new(),
        envelopes: BTreeMap::new(),
        envelope_files: BTreeMap::new(),
    };
    for node in req.get_nodes()? {
        nodes.by_id.insert(node.get_id(), node);
    }
    let mut files = Vec::new();
    for requested in req.get_requested_files()? {
        let filename = requested.get_filename()?.to_str()?;
        let base = Path::new(filename)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| filename.to_string());
        nodes.files.insert(requested.get_id(), base.clone());
        files.push(base);
    }
    // Method envelopes are scope-less; name them after their interface first
    // so every later lookup (including struct-typed fields) resolves.
    let interface_ids: Vec<u64> = nodes
        .by_id
        .iter()
        .filter(|(_, n)| matches!(n.which(), Ok(node::Interface(_))))
        .map(|(id, _)| *id)
        .collect();
    for id in interface_ids {
        let Ok(file) = nodes.file_of(id) else {
            continue;
        };
        let iface_name = nodes.name_of(id)?;
        let node::Interface(i) = nodes.by_id[&id].which()? else {
            continue;
        };
        for m in i.get_methods()? {
            let method = m.get_name()?.to_str()?;
            nodes.envelopes.insert(
                m.get_param_struct_type(),
                format!("{iface_name}.{method}$Params"),
            );
            nodes.envelopes.insert(
                m.get_result_struct_type(),
                format!("{iface_name}.{method}$Results"),
            );
            nodes
                .envelope_files
                .insert(m.get_param_struct_type(), file.clone());
            nodes
                .envelope_files
                .insert(m.get_result_struct_type(), file.clone());
        }
    }

    let mut doc = LayoutDoc {
        generator: "tools/abi-layout",
        capnp_version: capnp_version()?,
        files,
        structs: BTreeMap::new(),
        enums: BTreeMap::new(),
        interfaces: BTreeMap::new(),
        consts: BTreeMap::new(),
    };

    for (&id, node) in &nodes.by_id {
        let file = match nodes.file_of(id) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if !nodes.files.values().any(|f| *f == file) {
            // Imported builtins (e.g. `/capnp/c++.capnp`) are not ours.
            continue;
        }
        let name = nodes.name_of(id)?;
        match node.which()? {
            node::Struct(s) => {
                let mut fields = Vec::new();
                for f in s.get_fields()? {
                    let disc = f.get_discriminant_value();
                    let (ty, offset) = match f.which()? {
                        field::Slot(slot) => (nodes.wire_type(slot.get_type()?)?, slot.get_offset()),
                        field::Group(g) => bail!(
                            "struct {name}: field {} is a group ({:#x}); groups are not part of the plugin ABI",
                            f.get_name()?.to_str()?,
                            g.get_type_id()
                        ),
                    };
                    fields.push(FieldLayout {
                        name: f.get_name()?.to_str()?.to_string(),
                        code_order: f.get_code_order(),
                        discriminant_value: (disc != field::NO_DISCRIMINANT).then_some(disc),
                        ty,
                        offset,
                    });
                }
                fields.sort_by_key(|f| f.code_order);
                doc.structs.insert(
                    name.clone(),
                    StructLayout {
                        name,
                        file,
                        data_words: s.get_data_word_count(),
                        pointer_count: s.get_pointer_count(),
                        discriminant_count: s.get_discriminant_count(),
                        discriminant_offset: s.get_discriminant_offset(),
                        fields,
                    },
                );
            }
            node::Enum(e) => {
                let mut enumerants: Vec<(u16, String)> = Vec::new();
                for (ordinal, en) in e.get_enumerants()?.iter().enumerate() {
                    enumerants.push((
                        u16::try_from(ordinal).context("enum too large")?,
                        en.get_name()?.to_str()?.to_string(),
                    ));
                }
                enumerants.sort_by_key(|(o, _)| *o);
                doc.enums.insert(
                    name.clone(),
                    EnumLayout {
                        name,
                        file,
                        enumerants: enumerants.into_iter().map(|(_, n)| n).collect(),
                    },
                );
            }
            node::Interface(i) => {
                let mut methods = Vec::new();
                for (ordinal, m) in i.get_methods()?.iter().enumerate() {
                    methods.push(MethodLayout {
                        name: m.get_name()?.to_str()?.to_string(),
                        ordinal: u16::try_from(ordinal).context("interface too large")?,
                        params: nodes.name_of(m.get_param_struct_type())?,
                        results: nodes.name_of(m.get_result_struct_type())?,
                    });
                }
                doc.interfaces.insert(
                    name.clone(),
                    InterfaceLayout {
                        name,
                        file,
                        methods,
                    },
                );
            }
            node::Const(c) => {
                let value = match c.get_value()?.which()? {
                    value::Uint32(v) => serde_json::Value::from(v),
                    value::Uint64(v) => serde_json::Value::from(v),
                    value::Int32(v) => serde_json::Value::from(v),
                    value::Int64(v) => serde_json::Value::from(v),
                    value::Bool(v) => serde_json::Value::from(v),
                    value::Text(t) => serde_json::Value::from(t?.to_str()?),
                    _ => bail!("const {name}: unsupported constant type"),
                };
                doc.consts
                    .insert(name.clone(), ConstLayout { name, file, value });
            }
            node::File(()) | node::Annotation(_) => {}
        }
    }
    Ok(doc)
}

/// Entry point.
fn main() -> Result<()> {
    let args = parse_args()?;
    let bytes = compile(&args.schema_dir)?;
    let message = serialize::read_message(
        &mut bytes.as_slice(),
        capnp::message::ReaderOptions {
            traversal_limit_in_words: Some(64 * 1024 * 1024),
            nesting_limit: 128,
        },
    )
    .context("decode CodeGeneratorRequest")?;
    let req = message.get_root::<code_generator_request::Reader<'_>>()?;
    let doc = build(req)?;
    let mut json = serde_json::to_string_pretty(&doc)?;
    json.push('\n');
    match args.out {
        Some(path) => {
            std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
        }
        None => {
            std::io::stdout().write_all(json.as_bytes())?;
        }
    }
    Ok(())
}
