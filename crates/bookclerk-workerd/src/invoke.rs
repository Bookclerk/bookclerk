//! `POST /invoke`: the Cap'n Proto data plane between the launcher and the
//! adapter isolate.
//!
//! [`InvokeClient`] is a Cap'n Proto `ClientHook`. Every method call made
//! through a typed client built from it (`ContentSourceClient`,
//! `EventConsumerClient`, `JobRunnerClient`, `DatabaseClient`, …) becomes one
//! HTTP request whose body is the `$Params` struct as an unpacked
//! single-segment message and whose `200` reply body is the `$Results` struct
//! — the same envelopes the generated `generated-wire.ts` codecs read and
//! write in the isolate. No JSON projection of any ABI struct exists on this
//! path; see `docs/workerd-bridge.md`.
//!
//! Unpacked messages carry no capability table, so interface-typed fields are
//! written as capability indexes and the table travels beside the message as
//! the `X-Bookclerk-Caps` JSON header. Host-served capabilities are
//! [`GrantCap`]s — grant tokens the isolate presents on the granted channel —
//! and the only isolate-served kind is an `AdapterDatabaseSession`, which the
//! reply names by object id and later calls address with `X-Bookclerk-Target`.

#![allow(clippy::missing_docs_in_private_items)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use bookclerk_plugin_abi::plugin_capnp;
use bookclerk_plugin_abi::MAX_SCALAR_BYTES;
use capnp::any_pointer;
use capnp::capability::{self, FromClientHook, Promise, RemotePromise, Request};
use capnp::private::capability::{
    ClientHook, ParamsHook, PipelineHook, PipelineOp, RequestHook, ResponseHook, ResultsHook,
};
use capnp::traits::{HasTypeId, Imbue, ImbueMut};
use capnp::{message, serialize, Error};
use serde::{Deserialize, Serialize};

use crate::bridge_http::BridgeHttp;

/// Largest `/invoke` request or reply body.
///
/// Twice the scalar ceiling so an `EventConsumer.event` batch or a
/// `JobRunner.job` invocation with a checkpoint still fits; equal to the
/// isolate codec's traversal budget (`MAX_TRAVERSAL_WORDS` words in
/// `db-capnp.ts`), so anything larger would be rejected there anyway.
pub const MAX_INVOKE_BODY_BYTES: u32 = MAX_SCALAR_BYTES * 2;

/// Cap'n Proto nesting limit for `/invoke` replies (the ABI schema is shallow).
const REPLY_NESTING_LIMIT: i32 = 32;

/// One entry of the `X-Bookclerk-Caps` table. Entry `i` describes capability
/// index `i` of the message beside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CapDescriptor {
    /// `Source` → granted `GET /open`.
    Source {
        /// Grant token.
        token: String,
    },
    /// `Destination` → granted `PUT /put`.
    Destination {
        /// Grant token.
        token: String,
    },
    /// `ProgressSink` → granted `POST /progress`.
    Progress {
        /// Grant token.
        token: String,
    },
    /// `Cancellation` → granted `GET /cancel` long poll.
    Cancellation {
        /// Grant token.
        token: String,
    },
    /// `EventPublisher` → granted `POST /events/publish`.
    EventPublisher {
        /// Grant token.
        token: String,
    },
    /// `GuestDatabase` → granted `POST /db/execute`.
    GuestDatabase {
        /// Grant token.
        token: String,
        /// `[[databases]]` binding name.
        binding: String,
    },
    /// Isolate-served `AdapterDatabaseSession`; later calls carry `id` in
    /// `X-Bookclerk-Target`.
    AdapterSession {
        /// Isolate-side object id.
        id: String,
    },
}

/// Static method-name table: `(interface type id, method ordinal)` → the
/// `X-Bookclerk-Interface` / `X-Bookclerk-Method` header values.
struct InterfaceEntry {
    id: u64,
    name: &'static str,
    /// Method names indexed by Cap'n ordinal.
    methods: &'static [&'static str],
}

/// Every interface reachable on `/invoke`, mirroring `plugin.layout.json`
/// (`interface_table_matches_schema_layout` guards the mirror).
const INTERFACES: &[InterfaceEntry] = &[
    InterfaceEntry {
        id: <plugin_capnp::plugin_worker::Client as HasTypeId>::TYPE_ID,
        name: "PluginWorker",
        methods: &["describe", "open", "shutdown", "databaseMigrations"],
    },
    InterfaceEntry {
        id: <plugin_capnp::content_source::Client as HasTypeId>::TYPE_ID,
        name: "ContentSource",
        methods: &[
            "login",
            "scan",
            "fetchTitle",
            "listAccounts",
            "loginStart",
            "loginComplete",
            "searchCatalog",
            "expandCandidates",
            "purchaseHint",
            "listDeals",
            "health",
            "diagnose",
            "catalogDetail",
        ],
    },
    InterfaceEntry {
        id: <plugin_capnp::destination::Client as HasTypeId>::TYPE_ID,
        name: "Destination",
        methods: &[
            "head",
            "list",
            "get",
            "put",
            "copy",
            "delete",
            "commit",
            "abortStage",
        ],
    },
    InterfaceEntry {
        id: <plugin_capnp::remote_library::Client as HasTypeId>::TYPE_ID,
        name: "RemoteLibrary",
        methods: &[
            "health",
            "start",
            "stop",
            "diagnose",
            "scanLibrary",
            "syncListening",
            "pollEvents",
        ],
    },
    InterfaceEntry {
        id: <plugin_capnp::event_consumer::Client as HasTypeId>::TYPE_ID,
        name: "EventConsumer",
        methods: &["event"],
    },
    InterfaceEntry {
        id: <plugin_capnp::job_runner::Client as HasTypeId>::TYPE_ID,
        name: "JobRunner",
        methods: &["job"],
    },
    InterfaceEntry {
        id: <plugin_capnp::plugin_cli::Client as HasTypeId>::TYPE_ID,
        name: "PluginCli",
        methods: &["describe", "invoke"],
    },
    InterfaceEntry {
        id: <plugin_capnp::oidc::Client as HasTypeId>::TYPE_ID,
        name: "Oidc",
        methods: &["clients", "authenticateUser"],
    },
    InterfaceEntry {
        id: <plugin_capnp::database::Client as HasTypeId>::TYPE_ID,
        name: "Database",
        methods: &["openSession"],
    },
    InterfaceEntry {
        id: <plugin_capnp::adapter_database_session::Client as HasTypeId>::TYPE_ID,
        name: "AdapterDatabaseSession",
        methods: &[
            "capabilities",
            "execute",
            "close",
            "bootstrap",
            "exportIdentity",
            "importIdentity",
            "listUserRelations",
            "prepareUnitRestore",
            "dropUserRelations",
            "assertRestoreConstraints",
        ],
    },
];

/// Header values for one `(interface, method)` pair.
///
/// Returns `None` for interfaces that are not routed over `/invoke`
/// (`Source`, `ByteSource`, `EventPublisher`, `GuestDatabase`, …) and for
/// ordinals outside the schema.
#[must_use]
pub fn method_headers(interface_id: u64, method_id: u16) -> Option<(&'static str, &'static str)> {
    let entry = INTERFACES.iter().find(|e| e.id == interface_id)?;
    let method = entry.methods.get(usize::from(method_id))?;
    Some((entry.name, method))
}

// ---------------------------------------------------------------------------
// Grant capabilities (host-served, described by token)
// ---------------------------------------------------------------------------

/// Anchor whose address is the `ClientHook::get_brand` value of every
/// [`GrantCap`]; a process-unique brand no RPC connection can collide with.
static GRANT_BRAND_ANCHOR: u8 = 0;

fn grant_brand() -> usize {
    std::ptr::from_ref(&GRANT_BRAND_ANCHOR) as usize
}

thread_local! {
    /// Live grant capabilities by `get_ptr` id (vat thread only).
    static GRANT_CAPS: RefCell<HashMap<usize, CapDescriptor>> = RefCell::new(HashMap::new());
    static NEXT_GRANT_ID: Cell<usize> = const { Cell::new(1) };
}

struct GrantCapInner {
    id: usize,
    descriptor: CapDescriptor,
}

impl Drop for GrantCapInner {
    fn drop(&mut self) {
        GRANT_CAPS.with(|caps| {
            caps.borrow_mut().remove(&self.id);
        });
    }
}

/// A capability that exists only to be described: it is never callable from
/// this process. Placed in a `$Params` capability table it is serialized as
/// its [`CapDescriptor`] in `X-Bookclerk-Caps`, and the isolate reaches the
/// host object behind it through the granted channel with the token.
#[derive(Clone)]
pub struct GrantCap {
    inner: Rc<GrantCapInner>,
}

impl GrantCap {
    /// Registers a new grant capability.
    #[must_use]
    pub fn new(descriptor: CapDescriptor) -> Self {
        let id = NEXT_GRANT_ID.with(|next| {
            let id = next.get();
            next.set(id.wrapping_add(1).max(1));
            id
        });
        GRANT_CAPS.with(|caps| {
            caps.borrow_mut().insert(id, descriptor.clone());
        });
        Self {
            inner: Rc::new(GrantCapInner { id, descriptor }),
        }
    }

    /// Builds a typed Cap'n client (e.g. `source::Client`) over a new grant
    /// capability.
    #[must_use]
    pub fn client<C: FromClientHook>(descriptor: CapDescriptor) -> C {
        C::new(Box::new(Self::new(descriptor)))
    }

    /// The descriptor this capability serializes as.
    #[must_use]
    pub fn descriptor(&self) -> &CapDescriptor {
        &self.inner.descriptor
    }

    fn not_callable() -> Error {
        Error::failed(
            "grant capabilities are served to the isolate over the granted channel, not callable here"
                .into(),
        )
    }
}

impl ClientHook for GrantCap {
    fn add_ref(&self) -> Box<dyn ClientHook> {
        Box::new(self.clone())
    }

    fn new_call(
        &self,
        interface_id: u64,
        method_id: u16,
        size_hint: Option<capnp::MessageSize>,
    ) -> Request<any_pointer::Owned, any_pointer::Owned> {
        capnp_rpc::new_broken_cap(Self::not_callable())
            .hook
            .new_call(interface_id, method_id, size_hint)
    }

    fn call(
        &self,
        _interface_id: u64,
        _method_id: u16,
        _params: Box<dyn ParamsHook>,
        _results: Box<dyn ResultsHook>,
    ) -> Promise<(), Error> {
        Promise::err(Self::not_callable())
    }

    fn get_brand(&self) -> usize {
        grant_brand()
    }

    fn get_ptr(&self) -> usize {
        self.inner.id
    }

    fn get_resolved(&self) -> Option<Box<dyn ClientHook>> {
        None
    }

    fn when_more_resolved(&self) -> Option<Promise<Box<dyn ClientHook>, Error>> {
        None
    }

    fn when_resolved(&self) -> Promise<(), Error> {
        Promise::ok(())
    }
}

/// Descriptor of a capability found in a request table, or an error when the
/// hook is not a [`GrantCap`] (nothing else can be handed to the isolate).
fn describe_hook(hook: &dyn ClientHook) -> capnp::Result<CapDescriptor> {
    if hook.get_brand() != grant_brand() {
        return Err(Error::failed(
            "only grant capabilities can travel on /invoke".into(),
        ));
    }
    GRANT_CAPS
        .with(|caps| caps.borrow().get(&hook.get_ptr()).cloned())
        .ok_or_else(|| Error::failed("grant capability was released before the call".into()))
}

// ---------------------------------------------------------------------------
// Invoke client
// ---------------------------------------------------------------------------

struct InvokeInner {
    http: BridgeHttp,
    /// `X-Bookclerk-Context` (bridge JSON); absent for `PluginWorker.*`.
    context: Option<Rc<str>>,
    /// `X-Bookclerk-Target` for calls on an isolate-returned capability.
    target: Option<Rc<str>>,
    /// Anything that must outlive every client derived from this hook (for
    /// example the `RevokeGrant` of the `EVENTS` token in `context`).
    keepalive: Option<Rc<dyn std::any::Any>>,
}

/// Cap'n Proto client hook whose calls are `POST /invoke` requests.
#[derive(Clone)]
pub struct InvokeClient {
    inner: Rc<InvokeInner>,
}

impl InvokeClient {
    /// A hook for calls carrying `context` (the bridge JSON of one `open`), or
    /// none for `PluginWorker.*`.
    #[must_use]
    pub fn new(http: BridgeHttp, context: Option<String>) -> Self {
        Self::with_keepalive(http, context, None)
    }

    /// Like [`new`](Self::new), also pinning `keepalive` for as long as any
    /// client derived from this hook (including reply capabilities) lives.
    #[must_use]
    pub fn with_keepalive(
        http: BridgeHttp,
        context: Option<String>,
        keepalive: Option<Rc<dyn std::any::Any>>,
    ) -> Self {
        Self {
            inner: Rc::new(InvokeInner {
                http,
                context: context.map(Rc::from),
                target: None,
                keepalive,
            }),
        }
    }

    /// Typed client (`content_source::Client`, `job_runner::Client`, …) over
    /// this hook.
    #[must_use]
    pub fn typed<C: FromClientHook>(&self) -> C {
        C::new(Box::new(self.clone()))
    }

    /// Same isolate and context, addressing the object `id` the isolate
    /// returned.
    fn with_target(&self, id: &str) -> Self {
        Self {
            inner: Rc::new(InvokeInner {
                http: self.inner.http.clone(),
                context: self.inner.context.clone(),
                target: Some(Rc::from(id)),
                keepalive: self.inner.keepalive.clone(),
            }),
        }
    }

    /// Serializes the call, POSTs it, and decodes the reply envelope.
    async fn dispatch(
        &self,
        interface_id: u64,
        method_id: u16,
        message: message::Builder<message::HeapAllocator>,
        cap_table: Vec<Option<Box<dyn ClientHook>>>,
    ) -> capnp::Result<InvokeResponse> {
        let (interface, method) = method_headers(interface_id, method_id).ok_or_else(|| {
            Error::unimplemented(format!(
                "interface {interface_id:#x} method {method_id} is not routed over /invoke"
            ))
        })?;
        let (body, caps) = single_segment_bytes(&message, &cap_table)?;
        if body.len() > MAX_INVOKE_BODY_BYTES as usize {
            return Err(Error::failed(format!(
                "payload_too_large: {interface}.{method} params of {} bytes exceed {MAX_INVOKE_BODY_BYTES}",
                body.len()
            )));
        }
        let caps_json = if caps.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&caps).map_err(|err| Error::failed(err.to_string()))?)
        };
        let mut headers: Vec<(&str, &str)> = vec![
            ("x-bookclerk-interface", interface),
            ("x-bookclerk-method", method),
        ];
        if let Some(context) = self.inner.context.as_deref() {
            headers.push(("x-bookclerk-context", context));
        }
        if let Some(caps) = caps_json.as_deref() {
            headers.push(("x-bookclerk-caps", caps));
        }
        if let Some(target) = self.inner.target.as_deref() {
            headers.push(("x-bookclerk-target", target));
        }
        let response = self
            .inner
            .http
            .capnp_post("/invoke", &headers, &body, MAX_INVOKE_BODY_BYTES)
            .await
            .map_err(|err| {
                Error::disconnected(format!("{interface}.{method} transport failed: {err}"))
            })?;
        if response.status != 200 {
            return Err(transport_error(interface, method, &response));
        }
        let reply_caps: Vec<CapDescriptor> = match response.header("x-bookclerk-caps") {
            Some(raw) => serde_json::from_str(&raw).map_err(|err| {
                Error::failed(format!(
                    "{interface}.{method} reply capability table is malformed: {err}"
                ))
            })?,
            None => Vec::new(),
        };
        let cap_table = reply_caps
            .into_iter()
            .map(|descriptor| Some(self.reply_cap(descriptor)))
            .collect();
        let reader = read_single_segment(&response.body)?;
        Ok(InvokeResponse { reader, cap_table })
    }

    /// Hook for one entry of a reply capability table.
    fn reply_cap(&self, descriptor: CapDescriptor) -> Box<dyn ClientHook> {
        match descriptor {
            CapDescriptor::AdapterSession { id } => Box::new(self.with_target(&id)),
            other => {
                capnp_rpc::new_broken_cap(Error::failed(format!(
                    "isolate replies may only carry adapterSession capabilities, not {other:?}"
                )))
                .hook
            }
        }
    }
}

/// Non-200 `/invoke` reply → RPC error whose text is `code: message` so the
/// typed clients' `from_capnp` surfaces the bridge's wire code verbatim.
fn transport_error(
    interface: &str,
    method: &str,
    response: &crate::bridge_http::CapnpResponse,
) -> Error {
    if response.status == 401 {
        return Error::failed(format!(
            "unauthorized: bridge rejected {interface}.{method}"
        ));
    }
    let (code, message) = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|v| {
            let err = v.get("error")?;
            Some((
                err.get("code")?.as_str()?.to_string(),
                err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("bridge error")
                    .to_string(),
            ))
        })
        .unwrap_or_else(|| {
            (
                "internal".to_string(),
                format!(
                    "bridge HTTP {} on {interface}.{method}: {}",
                    response.status,
                    String::from_utf8_lossy(&response.body)
                ),
            )
        });
    if response.status == 404 || code == "unsupported" {
        Error::unimplemented(format!("{code}: {message}"))
    } else {
        Error::failed(format!("{code}: {message}"))
    }
}

/// Copies the request root into a fresh single-segment message (the isolate
/// codecs read exactly one segment) and returns its bytes with the
/// capability table the copy produced, in index order.
fn single_segment_bytes(
    message: &message::Builder<message::HeapAllocator>,
    cap_table: &[Option<Box<dyn ClientHook>>],
) -> capnp::Result<(Vec<u8>, Vec<Option<CapDescriptor>>)> {
    let mut root: any_pointer::Reader = message.get_root_as_reader()?;
    let caps: Vec<Option<Box<dyn ClientHook>>> = cap_table
        .iter()
        .map(|c| c.as_ref().map(|h| h.add_ref()))
        .collect();
    root.imbue(&caps);
    let size = root.target_size()?;
    let words = u32::try_from(size.word_count)
        .map_err(|_| Error::failed("params exceed the addressable segment size".into()))?;
    let mut single = message::Builder::new(
        message::HeapAllocator::new().first_segment_words(words.saturating_add(1)),
    );
    let mut copied_caps: Vec<Option<Box<dyn ClientHook>>> = Vec::new();
    {
        let mut dst: any_pointer::Builder = single.init_root();
        dst.imbue_mut(&mut copied_caps);
        dst.set_as(root)?;
    }
    if single.get_segments_for_output().len() != 1 {
        return Err(Error::failed(
            "params did not fit a single Cap'n segment".into(),
        ));
    }
    let descriptors = copied_caps
        .iter()
        .map(|cap| cap.as_deref().map(describe_hook).transpose())
        .collect::<capnp::Result<Vec<_>>>()?;
    Ok((serialize::write_message_to_words(&single), descriptors))
}

/// Reads an unpacked single-segment reply with a traversal budget derived
/// from its own length.
fn read_single_segment(bytes: &[u8]) -> capnp::Result<message::Reader<serialize::OwnedSegments>> {
    if bytes.len() < 8 {
        return Err(Error::failed("truncated Cap'n reply".into()));
    }
    let mut nseg_minus = [0u8; 4];
    nseg_minus.copy_from_slice(&bytes[..4]);
    if u32::from_le_bytes(nseg_minus) != 0 {
        return Err(Error::failed(
            "multi-segment Cap'n replies are not supported".into(),
        ));
    }
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(REPLY_NESTING_LIMIT);
    let mut cursor = std::io::Cursor::new(bytes);
    serialize::read_message(&mut cursor, opts)
}

impl ClientHook for InvokeClient {
    fn add_ref(&self) -> Box<dyn ClientHook> {
        Box::new(self.clone())
    }

    fn new_call(
        &self,
        interface_id: u64,
        method_id: u16,
        _size_hint: Option<capnp::MessageSize>,
    ) -> Request<any_pointer::Owned, any_pointer::Owned> {
        Request::new(Box::new(InvokeRequest {
            message: message::Builder::new_default(),
            cap_table: Vec::new(),
            interface_id,
            method_id,
            client: self.clone(),
        }))
    }

    /// Server-side entry (a vat forwarding a call to this hook): copy the
    /// params into a fresh request, send it, copy the results back.
    fn call(
        &self,
        interface_id: u64,
        method_id: u16,
        params: Box<dyn ParamsHook>,
        mut results: Box<dyn ResultsHook>,
    ) -> Promise<(), Error> {
        let client = self.clone();
        Promise::from_future(async move {
            let mut request = client.new_call(interface_id, method_id, None);
            request.get().set_as(params.get()?)?;
            let response = request.send().promise.await?;
            results.get()?.set_as(response.get()?)?;
            Ok(())
        })
    }

    fn get_brand(&self) -> usize {
        0
    }

    fn get_ptr(&self) -> usize {
        Rc::as_ptr(&self.inner) as usize
    }

    fn get_resolved(&self) -> Option<Box<dyn ClientHook>> {
        None
    }

    fn when_more_resolved(&self) -> Option<Promise<Box<dyn ClientHook>, Error>> {
        None
    }

    fn when_resolved(&self) -> Promise<(), Error> {
        Promise::ok(())
    }
}

struct InvokeRequest {
    message: message::Builder<message::HeapAllocator>,
    cap_table: Vec<Option<Box<dyn ClientHook>>>,
    interface_id: u64,
    method_id: u16,
    client: InvokeClient,
}

impl RequestHook for InvokeRequest {
    fn get(&mut self) -> any_pointer::Builder<'_> {
        let mut root: any_pointer::Builder = self
            .message
            .get_root()
            .expect("any_pointer root of a heap message is always readable");
        root.imbue_mut(&mut self.cap_table);
        root
    }

    fn get_brand(&self) -> usize {
        0
    }

    fn send(self: Box<Self>) -> RemotePromise<any_pointer::Owned> {
        let InvokeRequest {
            message,
            cap_table,
            interface_id,
            method_id,
            client,
        } = *self;
        let promise = Promise::from_future(async move {
            client
                .dispatch(interface_id, method_id, message, cap_table)
                .await
                .map(|response| capability::Response::new(Box::new(response)))
        });
        RemotePromise {
            promise,
            pipeline: any_pointer::Pipeline::new(Box::new(NoPipeline)),
        }
    }

    fn send_streaming(self: Box<Self>) -> Promise<(), Error> {
        Promise::from_future(async move {
            let _ = self.send().promise.await?;
            Ok(())
        })
    }

    fn tail_send(self: Box<Self>) -> Option<(u32, Promise<(), Error>, Box<dyn PipelineHook>)> {
        None
    }
}

/// `/invoke` is request/response; promise pipelining is not offered.
struct NoPipeline;

impl PipelineHook for NoPipeline {
    fn add_ref(&self) -> Box<dyn PipelineHook> {
        Box::new(NoPipeline)
    }

    fn get_pipelined_cap(&self, _ops: &[PipelineOp]) -> Box<dyn ClientHook> {
        capnp_rpc::new_broken_cap(Error::unimplemented(
            "promise pipelining is not supported on the /invoke transport".into(),
        ))
        .hook
    }
}

struct InvokeResponse {
    reader: message::Reader<serialize::OwnedSegments>,
    cap_table: Vec<Option<Box<dyn ClientHook>>>,
}

impl ResponseHook for InvokeResponse {
    fn get(&self) -> capnp::Result<any_pointer::Reader<'_>> {
        let mut root: any_pointer::Reader = self.reader.get_root()?;
        root.imbue(&self.cap_table);
        Ok(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Rust table is a hand mirror of the Cap'n compiler's layout output;
    /// this keeps the two in lockstep when `plugin.capnp` changes.
    #[test]
    fn interface_table_matches_schema_layout() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bookclerk-plugin-abi/schema/plugin.layout.json"
        );
        let raw = std::fs::read_to_string(path).expect("read plugin.layout.json");
        let layout: serde_json::Value = serde_json::from_str(&raw).expect("parse layout");
        let interfaces = layout["interfaces"].as_object().expect("interfaces object");
        for entry in INTERFACES {
            let iface = interfaces
                .get(entry.name)
                .unwrap_or_else(|| panic!("{} missing from plugin.layout.json", entry.name));
            let methods = iface["methods"].as_array().expect("methods array");
            assert_eq!(
                methods.len(),
                entry.methods.len(),
                "{}: method count",
                entry.name
            );
            for method in methods {
                let ordinal =
                    usize::try_from(method["ordinal"].as_u64().expect("ordinal")).expect("usize");
                assert_eq!(
                    entry.methods[ordinal],
                    method["name"].as_str().expect("name"),
                    "{}: ordinal {ordinal}",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn method_headers_resolve_known_methods_only() {
        let content_source = <plugin_capnp::content_source::Client as HasTypeId>::TYPE_ID;
        assert_eq!(
            method_headers(content_source, 12),
            Some(("ContentSource", "catalogDetail"))
        );
        assert_eq!(method_headers(content_source, 13), None);
        let source = <plugin_capnp::source::Client as HasTypeId>::TYPE_ID;
        assert_eq!(
            method_headers(source, 0),
            None,
            "streams are not /invoke methods"
        );
    }

    #[test]
    fn descriptors_round_trip_as_tagged_json() {
        let caps = vec![
            CapDescriptor::Source { token: "a".into() },
            CapDescriptor::GuestDatabase {
                token: "b".into(),
                binding: "DB".into(),
            },
            CapDescriptor::AdapterSession { id: "s1".into() },
        ];
        let json = serde_json::to_string(&caps).expect("encode");
        assert_eq!(
            json,
            r#"[{"kind":"source","token":"a"},{"kind":"guestDatabase","token":"b","binding":"DB"},{"kind":"adapterSession","id":"s1"}]"#
        );
        let back: Vec<CapDescriptor> = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, caps);
    }

    #[test]
    fn grant_caps_are_described_while_alive_only() {
        let cap = GrantCap::new(CapDescriptor::Progress { token: "t".into() });
        let hook: Box<dyn ClientHook> = Box::new(cap.clone());
        assert_eq!(
            describe_hook(hook.as_ref()).expect("described"),
            CapDescriptor::Progress { token: "t".into() }
        );
        let ptr = hook.get_ptr();
        drop(hook);
        drop(cap);
        let released = GRANT_CAPS.with(|caps| caps.borrow().contains_key(&ptr));
        assert!(!released, "dropping the last reference releases the entry");
    }

    #[test]
    fn foreign_hooks_are_rejected() {
        let broken = capnp_rpc::new_broken_cap(Error::failed("x".into())).hook;
        let err = describe_hook(broken.as_ref()).expect_err("not a grant cap");
        assert!(err.to_string().contains("only grant capabilities"), "{err}");
    }

    /// A `JobController` with grant capabilities serializes to one segment
    /// whose capability indexes line up with the descriptor table.
    #[test]
    fn job_params_serialize_with_capability_table() {
        use plugin_capnp::job_runner::job_params;
        let mut message = message::Builder::new_default();
        let mut cap_table: Vec<Option<Box<dyn ClientHook>>> = Vec::new();
        {
            let mut root: any_pointer::Builder = message.init_root();
            root.imbue_mut(&mut cap_table);
            let mut params = root.init_as::<job_params::Builder<'_>>();
            let mut controller = params.reborrow().init_controller();
            controller
                .reborrow()
                .init_invocation()
                .set_invocation_id("job-1");
            controller.set_input(GrantCap::client(CapDescriptor::Source {
                token: "in".into(),
            }));
            controller.set_output(GrantCap::client(CapDescriptor::Destination {
                token: "out".into(),
            }));
            controller.set_progress(GrantCap::client(CapDescriptor::Progress {
                token: "p".into(),
            }));
            controller.set_cancel(GrantCap::client(CapDescriptor::Cancellation {
                token: "c".into(),
            }));
        }
        let (bytes, caps) = single_segment_bytes(&message, &cap_table).expect("single segment");
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            0,
            "one segment"
        );
        assert_eq!(
            caps,
            vec![
                Some(CapDescriptor::Source { token: "in".into() }),
                Some(CapDescriptor::Destination {
                    token: "out".into()
                }),
                Some(CapDescriptor::Progress { token: "p".into() }),
                Some(CapDescriptor::Cancellation { token: "c".into() }),
            ]
        );
        let reader = read_single_segment(&bytes).expect("readable");
        let params = reader
            .get_root::<job_params::Reader<'_>>()
            .expect("params root");
        let id = params
            .get_controller()
            .expect("controller")
            .get_invocation()
            .expect("invocation")
            .get_invocation_id()
            .expect("id");
        assert_eq!(id.to_str().expect("utf8"), "job-1");
    }

    /// A large request (bigger than the default first heap segment) is still
    /// emitted as one segment.
    #[test]
    fn large_params_collapse_to_one_segment() {
        use plugin_capnp::content_source::login_params;
        let mut message = message::Builder::new_default();
        {
            let params = message.init_root::<login_params::Builder<'_>>();
            params.init_params().set_plugin_data_dir("x".repeat(40_000));
        }
        assert!(
            message.get_segments_for_output().len() > 1,
            "precondition: heap builder spilled into more segments"
        );
        let (bytes, caps) = single_segment_bytes(&message, &[]).expect("single segment");
        assert!(caps.is_empty());
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 0);
        let reader = read_single_segment(&bytes).expect("readable");
        let params = reader.get_root::<login_params::Reader<'_>>().expect("root");
        assert_eq!(
            params
                .get_params()
                .expect("params")
                .get_plugin_data_dir()
                .expect("dir")
                .len(),
            40_000
        );
    }
}
