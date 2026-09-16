"""GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.

Author-facing Python projection of every struct, union, enum, and
interface in ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.
TypedDict keys are the schema names (camelCase). Cap'n Proto unions
become ``{"kind": ..., "value": ...}`` TypedDict unions; ``Void``
members carry no ``value``. 64-bit integers are Python ``int``; ``Data``
is ``bytes``. Interfaces are ``Protocol`` classes with ``async`` methods.
"""

from __future__ import annotations

from typing import Any, Literal, NotRequired, Protocol, TypedDict, Union

PluginErrorCode = Literal[
    "invalid_params",
    "unauthorized",
    "forbidden",
    "not_found",
    "unavailable",
    "unsupported",
    "internal",
    "payload_too_large",
    "deadline_exceeded",
    "invalid_cursor",
    "cancelled",
    "conflict",
]
"""Stable `PluginError.code` strings. Unknown future codes are forwarded
as-is; SDKs surface them as a local `unknown` while keeping the raw wire
code.
"""

Entrypoint = Literal[
    "storefront",
    "storage",
    "databaseAdapter",
    "remoteLibrary",
    "cli",
    "oidc",
]
"""Named entrypoint a plugin exports (Cloudflare Workers named-entrypoint
analogue). Each value is a capability the host calls over RPC; triggers on
the default entrypoint (`event`, `job`) are declared separately.
"""

PortalAuthMode = Literal["unspecified", "password", "oauth"]
"""Portal Accounts connect mode for storefronts."""

CliArgKind = Literal["string", "bool", "int", "path"]
"""Value kind for a `CliArgSpec`."""

CatalogSort = Literal["relevance", "popularity", "rating", "title", "author"]
"""Catalog search ordering."""

CatalogField = Literal["any", "author", "narrator", "series", "genre"]
"""Catalog search facet restricting which field the query matches."""

Abridgement = Literal["unknown", "unabridged", "abridged"]
"""Whether an edition is abridged."""

DbType = Literal["unspecified", "bool", "int64", "float64", "text", "bytes"]
"""Universal database cell/parameter domain. Engine-native arrays, enums,
unsigned integers, and JSON text sentinels are not baseline ABI values.
"""

DbStatementKind = Literal["execute", "select", "returning"]
"""How the host classifies a statement for result handling."""

DbResultSelection = Literal["discard", "affectedRows", "rows"]
"""Which part of a statement's outcome the caller wants back."""

IsolationReq = Literal["atomicBatch", "nestedSavepoint", "consistentSnapshot"]
"""Host → adapter execute. GuestDatabase stays ExecuteRequest-only."""

ResolvedSqlType = Literal["integer", "real", "text", "blob", "boolean", "null"]
"""Resolved BookclerkSQL column type after type checking."""

IntegerArithKind = Literal["add", "sub", "mul", "abs"]
"""INTEGER `+` `-` `*` `abs` site lowered to overflow -> NULL."""


class ScalarLimits(TypedDict):
    """Guest-advertised caps for `rpc.scalarLimits`; never above the file constants.

    Attributes:
        maxScalarBytes: Largest scalar the guest accepts; at most `maxScalarBytes`.
        maxStreamWindowBytes: Largest `ByteSource.pull` window; at most
            `maxStreamWindowBytes`.
        maxListPage: Largest list page; at most `maxListPage`.
    """

    maxScalarBytes: int
    maxStreamWindowBytes: int
    maxListPage: int


class PluginError(TypedDict):
    """`code` is a snake_case string (`not_found`, `invalid_cursor`, …). Unknown
    codes are forwarded as-is; SDKs surface them as PluginErrorCode.unknown
    while retaining the raw wire code.

    Attributes:
        code: Stable snake_case error code (see `PluginErrorCode`).
        message: Human-readable detail; never contains secrets.
    """

    code: str
    message: str


class ObjectMetadata(TypedDict):
    """Full metadata of one stored object.

    Attributes:
        key: Object key.
        size: Object size in bytes.
        contentType: MIME type; empty when unknown.
        etag: Backend entity tag; empty when unsupported.
        sha256: Raw SHA-256 digest (32 bytes) or empty when unknown.
    """

    key: str
    size: int
    contentType: str
    etag: str
    sha256: bytes


class ObjectInfo(TypedDict):
    """Compact object entry in a list page.

    Attributes:
        key: Object key.
        size: Object size in bytes.
    """

    key: str
    size: int


class ListOptions(TypedDict):
    """Paging options for `Destination.list`.

    Attributes:
        prefix: Only keys starting with this prefix; empty lists everything.
        cursor: Opaque cursor from a previous `ListPage.nextCursor`; empty starts over.
        limit: Requested page size; clamped to `maxListPage`. `0` means guest default.
    """

    prefix: str
    cursor: str
    limit: int


class ListPage(TypedDict):
    """One page of `Destination.list` results.

    Attributes:
        objects: Objects in this page, in backend order.
        nextCursor: Cursor for the next page; empty when exhausted.
    """

    objects: list[ObjectInfo]
    nextCursor: str


class ByteRange(TypedDict):
    """Half-open byte window `[offset, offset + length)`.

    Attributes:
        offset: First byte offset.
        length: Number of bytes; `0` reads to the end.
    """

    offset: int
    length: int


class ReadOptions(TypedDict):
    """Options for `Destination.get`.

    Attributes:
        range: Byte range to read; an all-zero range reads the whole object.
    """

    range: ByteRange


class WriteOptions(TypedDict):
    """Options for `Destination.put`.

    Attributes:
        contentType: MIME type to record; empty when unknown.
        contentLength: Expected body length in bytes; `0` when unknown (chunked).
        sha256: Expected raw SHA-256 digest; empty skips verification.
        commitToken: Destination-side stage-and-publish. Empty means a one-shot put.
        stageOnly: When true, `put` stages remotely and does not publish until `commit`.
    """

    contentType: str
    contentLength: int
    sha256: bytes
    commitToken: str
    stageOnly: bool


class PutResult(TypedDict):
    """Summary of a stored (or committed) object.

    Attributes:
        key: Object key written.
        bytesWritten: Bytes persisted.
        etag: Backend entity tag; empty when unsupported.
        sha256: Raw SHA-256 digest of the stored bytes; empty when not computed.
    """

    key: str
    bytesWritten: int
    etag: str
    sha256: bytes


class CopyResult(TypedDict):
    """Summary of a server-side copy.

    Attributes:
        bytesCopied: Bytes copied.
    """

    bytesCopied: int


class PluginDescribe(TypedDict):
    """Guest identity and negotiation surface returned by `describe()`.

    Attributes:
        apiVersion: ABI version the guest speaks; must equal `apiVersion`.
        id: Stable plugin id (`[a-z][a-z0-9_]{0,63}`).
        displayName: Human-readable name for UI lists.
        rpcFeatures: Negotiable feature names the guest supports (see `feature*`
            constants).
        scalarLimits: Guest caps when `rpc.scalarLimits` is advertised.
        capabilities: Exported entrypoints, triggers, and bindings the guest implements.
            The host rejects anything wider than the manifest and the operator grant.
        portalAuthMode: Portal Accounts connect mode for storefronts.
        passwordEnvVar: Env var name operators may set for password helpers; never
            required for Accounts UI connect. Empty when the guest accepts none. Omitted
            when absent (wire zero value).
        aliases: Alternate ids accepted for config / CLI targeting.
        sortKey: UI sort weight among peers of the same family; lower sorts first.
        brand: Portal brand colors and icon URL; `brand.id` is empty when the guest has
            no brand and the host renders a neutral fallback.
        configOptions: Discoverable config option groups for source UIs.
        cli: Embedded CLI schema (same shape as `PluginCli.describe`); empty when
            unused.
    """

    apiVersion: int
    id: str
    displayName: str
    rpcFeatures: list[str]
    scalarLimits: ScalarLimits
    capabilities: PluginCapabilities
    portalAuthMode: PortalAuthMode
    passwordEnvVar: NotRequired[str]
    aliases: list[str]
    sortKey: int
    brand: Brand
    configOptions: list[ConfigOption]
    cli: CliSchema


class OidcClientTemplate(TypedDict):
    """Bookclerk-as-IdP relying-party template. Plugins declare callback path and
    client id; the host materializes `oidc_clients` rows and remains the AS.
    `originConfigKey` is a dotted config path (e.g. integrations.audiobookshelf.base_url).

    Attributes:
        clientId: OIDC client id the host materializes.
        displayName: Name shown on consent / admin screens.
        callbackPath: Redirect URI path relative to the integration origin.
        publicClient: When true, the client uses PKCE without a client secret.
        defaultScopes: Scopes granted by default.
        issueRefreshToken: When true, the AS issues refresh tokens to this client.
        originConfigKey: Dotted config path holding the client's origin URL.
    """

    clientId: str
    displayName: str
    callbackPath: str
    publicClient: bool
    defaultScopes: list[str]
    issueRefreshToken: bool
    originConfigKey: str


class OidcClientsOk(TypedDict):
    """Success payload of `Oidc.clients`.

    Attributes:
        clients: Client templates; empty when the plugin is not a relying party.
    """

    clients: list[OidcClientTemplate]


class OidcClientsReplyOk(TypedDict):
    """``OidcClientsReply`` member ``ok``.

    Success: OIDC client templates.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: OIDC client templates.
    """

    kind: Literal["ok"]
    value: OidcClientsOk


class OidcClientsReplyErr(TypedDict):
    """``OidcClientsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


OidcClientsReply = Union[
    OidcClientsReplyOk,
    OidcClientsReplyErr,
]
"""Result union of `Oidc.clients`."""


class ExtensibleConfig(TypedDict):
    """Plugin-specific extensible config. Not a substitute for typed ABI fields.

    Attributes:
        schemaVersion: Version of `payload`'s schema, owned by the plugin.
        mediaType: Media type of `payload` (e.g. `application/json`).
        payload: Bounded encoded payload; at most `maxConfigPayloadBytes`.
    """

    schemaVersion: int
    mediaType: str
    payload: bytes


class Bindings(TypedDict):
    """Host-granted bindings for one `PluginWorker.open` (`env` in the Workers
    idiom). OS paths, FDs, and sockets are transport-private. Capability fields
    are null when the manifest does not declare (or the operator did not grant)
    the binding.

    Attributes:
        config: `CONFIG`: granted plugin settings (operator `[vars]` plus the family
            settings table) as `application/json`; at most `maxConfigPayloadBytes`.
        secrets: `SECRETS`: granted secret values as `application/json`; empty payload
            when the manifest declares no `[secrets]`.
        adapter: Database-adapter bootstrap for the `databaseAdapter` entrypoint. First-
            party host-managed adapters receive host-private connect params in `config`;
            third-party adapters receive this typed bootstrap (and an empty `config`).
        events: `EVENTS`: outbox publisher; null unless `[[events.producers]]` is
            granted.
        databases: Named plugin-owned `[[databases]]` bindings: each entry is an
            isolated database provisioned by the active adapter, separate from the
            Bookclerk library and from every other plugin. Empty when the manifest
            declares none.
        cancel: Host cancellation for the whole invocation (fence / lease loss).
        storage: `WORK_FS`: host-granted object storage for durable plugin files (work
            filesystem); null unless `[work_fs]` is granted. Job input/output travel on
            `JobRunner.job(controller)`, never here.
    """

    config: ExtensibleConfig
    secrets: ExtensibleConfig
    adapter: DatabaseAdapterConfig
    events: EventPublisher
    databases: list[NamedDatabase]
    cancel: Cancellation
    storage: Destination


class Entrypoints(TypedDict):
    """Exported entrypoints returned by `PluginWorker.open`. One capability per
    `plugin.toml` `entrypoints` entry / trigger; null when not exported. The host
    refuses an entrypoint the manifest or operator grant did not allow.

    Attributes:
        eventConsumer: `[[events.consumers]]` trigger: `event(batch)` handler.
        jobRunner: `[triggers] jobs` trigger: `job(controller)` handler.
        storefront: `storefront` entrypoint.
        storage: `storage` entrypoint.
        databaseAdapter: `databaseAdapter` entrypoint.
        remoteLibrary: `remoteLibrary` entrypoint.
        cli: `cli` entrypoint.
        oidc: `oidc` entrypoint.
    """

    eventConsumer: EventConsumer
    jobRunner: JobRunner
    storefront: ContentSource
    storage: Destination
    databaseAdapter: Database
    remoteLibrary: RemoteLibrary
    cli: PluginCli
    oidc: Oidc


class EntrypointsReplyOk(TypedDict):
    """``EntrypointsReply`` member ``ok``.

    Success: exported entrypoint capabilities.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: exported entrypoint capabilities.
    """

    kind: Literal["ok"]
    value: Entrypoints


class EntrypointsReplyErr(TypedDict):
    """``EntrypointsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


EntrypointsReply = Union[
    EntrypointsReplyOk,
    EntrypointsReplyErr,
]
"""Result union of `PluginWorker.open`."""


class JobInvocation(TypedDict):
    """Durable command envelope (not a domain event). Command payload schema
    version and checkpoint schema versions are independent of the plugin ABI.
    The envelope itself is not persisted as opaque bytes across ABI majors.
    Idempotency keys are scoped to (account, plugin, commandType) until a
    terminal fenced outcome is committed.

    Attributes:
        payloadSchemaVersion: Schema version of `payloadJson`, owned by the command
            type.
        invocationId: Unique id of this invocation attempt.
        commandType: Command type the handler dispatches on.
        payloadJson: Command payload (JSON); at most `maxScalarBytes`.
        idempotencyKey: Caller idempotency key scoped to (account, plugin, commandType).
        attempt: Failure retry counter, starting at 1.
        correlationId: Trace correlation id; empty when none.
        causationId: Id of the event or command that caused this one; empty when none.
        deadlineUnixMs: UTC Unix milliseconds. Host fence/lease is authoritative; this
            hint must not outlive the fence (clock skew across VPS nodes).
        checkpointJson: Checkpoint persisted by a prior `SuspendedOutcome`; empty on
            first run.
        checkpointSchemaVersion: Schema version of `checkpointJson`.
        invocationSequence: Resume ordinal; distinct from failure `attempt`.
        stepId: Optional step identifier for multi-step commands.
    """

    payloadSchemaVersion: int
    invocationId: str
    commandType: str
    payloadJson: str
    idempotencyKey: str
    attempt: int
    correlationId: str
    causationId: str
    deadlineUnixMs: int
    checkpointJson: str
    checkpointSchemaVersion: int
    invocationSequence: int
    stepId: str


class CompletedOutcome(TypedDict):
    """Job finished successfully.

    Attributes:
        message: Short human summary.
        bytesCopied: Bytes produced, when meaningful.
    """

    message: str
    bytesCopied: int


class RetryableOutcome(TypedDict):
    """Job failed transiently; the host reschedules it.

    Attributes:
        message: Short human reason.
        retryAfterUnixMs: Earliest retry time; `0` lets the host choose.
    """

    message: str
    retryAfterUnixMs: int


class RejectedOutcome(TypedDict):
    """Job failed permanently; no retry.

    Attributes:
        message: Short human reason.
    """

    message: str


class CancelledOutcome(TypedDict):
    """Job observed cancellation and stopped.

    Attributes:
        message: Short human note.
    """

    message: str


class SuspendedOutcome(TypedDict):
    """Job released the process and asks to be resumed later.

    Attributes:
        checkpointJson: Bounded checkpoint to replay on resume; at most
            `maxCheckpointBytes`.
        checkpointSchemaVersion: Schema version of `checkpointJson`.
        wakeAtUnixMs: Earliest resume time.
    """

    checkpointJson: str
    checkpointSchemaVersion: int
    wakeAtUnixMs: int


class JobOutcomeCompleted(TypedDict):
    """``JobOutcome`` member ``completed``.

    Finished successfully.

    Attributes:
        kind: Always ``"completed"``.
        value: Finished successfully.
    """

    kind: Literal["completed"]
    value: CompletedOutcome


class JobOutcomeRetryable(TypedDict):
    """``JobOutcome`` member ``retryable``.

    Transient failure; retry later.

    Attributes:
        kind: Always ``"retryable"``.
        value: Transient failure; retry later.
    """

    kind: Literal["retryable"]
    value: RetryableOutcome


class JobOutcomeRejected(TypedDict):
    """``JobOutcome`` member ``rejected``.

    Permanent failure.

    Attributes:
        kind: Always ``"rejected"``.
        value: Permanent failure.
    """

    kind: Literal["rejected"]
    value: RejectedOutcome


class JobOutcomeCancelled(TypedDict):
    """``JobOutcome`` member ``cancelled``.

    Stopped on cancellation.

    Attributes:
        kind: Always ``"cancelled"``.
        value: Stopped on cancellation.
    """

    kind: Literal["cancelled"]
    value: CancelledOutcome


class JobOutcomeSuspended(TypedDict):
    """``JobOutcome`` member ``suspended``.

    Released with a checkpoint.

    Attributes:
        kind: Always ``"suspended"``.
        value: Released with a checkpoint.
    """

    kind: Literal["suspended"]
    value: SuspendedOutcome


JobOutcome = Union[
    JobOutcomeCompleted,
    JobOutcomeRetryable,
    JobOutcomeRejected,
    JobOutcomeCancelled,
    JobOutcomeSuspended,
]
"""Terminal or suspended result of `JobRunner.job`."""


class DomainEvent(TypedDict):
    """Domain event (not a job). Outbox-produced, at-least-once, idempotent consume.

    Attributes:
        eventId: Unique event id (outbox row identity).
        eventType: Dotted event type (e.g. `library.title.added`).
        schemaVersion: Schema version of `payload`, owned by the event type.
        occurredAtUnixMs: When the producer observed the fact.
        accountId: Account scope; empty for host-wide events.
        correlationId: Trace correlation id; empty when none.
        causationId: Id of the command or event that caused this one; empty when none.
        deduplicationKey: Consumer-side idempotency key; stable across redeliveries.
        deliveryAttempt: Delivery counter, starting at 1.
        payload: Encoded event payload; at most `maxEventPayloadBytes`.
        checkpointJson: Append-only. Resume a prior EventResult.suspended.
        checkpointSchemaVersion: Schema version of `checkpointJson`.
        invocationSequence: Resume ordinal; distinct from `deliveryAttempt`.
        resumePending: True when this delivery resumes a prior suspension.
        source: Append-only. Producer plugin id; empty when unknown.
    """

    eventId: str
    eventType: str
    schemaVersion: int
    occurredAtUnixMs: int
    accountId: str
    correlationId: str
    causationId: str
    deduplicationKey: str
    deliveryAttempt: int
    payload: bytes
    checkpointJson: str
    checkpointSchemaVersion: int
    invocationSequence: int
    resumePending: bool
    source: str


class EventAck(TypedDict):
    """Event handled; the host marks it delivered.

    Attributes:
        dummy: Placeholder; the struct carries no data.
    """

    dummy: None


class EventRetry(TypedDict):
    """Redeliver later.

    Attributes:
        retryAtUnixMs: Earliest redelivery time; `0` lets the host choose.
        reason: Short human reason.
    """

    retryAtUnixMs: int
    reason: str


class EventReject(TypedDict):
    """Event rejected; the host records the reason and stops delivering.

    Attributes:
        reason: Short human reason.
    """

    reason: str


class EventDeadLetter(TypedDict):
    """Event moved to the dead-letter queue for operator review.

    Attributes:
        reason: Short human reason.
    """

    reason: str


class EventSuspended(TypedDict):
    """Append-only. Mirrors job SuspendedOutcome; event handlers persist a
    bounded checkpoint and release the process until wakeAtUnixMs. Optional
    wake-on-matching-event fields (empty = timestamp-only).

    Attributes:
        checkpointJson: Bounded checkpoint to replay on resume; at most
            `maxCheckpointBytes`.
        checkpointSchemaVersion: Schema version of `checkpointJson`.
        wakeAtUnixMs: Earliest resume time.
        wakeOnEventType: Also wake when an event of this type arrives; empty disables.
        wakeOnFilterJson: JSON filter applied to matching wake events; empty matches
            all.
    """

    checkpointJson: str
    checkpointSchemaVersion: int
    wakeAtUnixMs: int
    wakeOnEventType: str
    wakeOnFilterJson: str


class EventResultAck(TypedDict):
    """``EventResult`` member ``ack``.

    Handled.

    Attributes:
        kind: Always ``"ack"``.
        value: Handled.
    """

    kind: Literal["ack"]
    value: EventAck


class EventResultRetry(TypedDict):
    """``EventResult`` member ``retry``.

    Redeliver later.

    Attributes:
        kind: Always ``"retry"``.
        value: Redeliver later.
    """

    kind: Literal["retry"]
    value: EventRetry


class EventResultReject(TypedDict):
    """``EventResult`` member ``reject``.

    Stop delivering.

    Attributes:
        kind: Always ``"reject"``.
        value: Stop delivering.
    """

    kind: Literal["reject"]
    value: EventReject


class EventResultDeadLetter(TypedDict):
    """``EventResult`` member ``deadLetter``.

    Park for operator review.

    Attributes:
        kind: Always ``"deadLetter"``.
        value: Park for operator review.
    """

    kind: Literal["deadLetter"]
    value: EventDeadLetter


class EventResultSuspended(TypedDict):
    """``EventResult`` member ``suspended``.

    Released with a checkpoint.

    Attributes:
        kind: Always ``"suspended"``.
        value: Released with a checkpoint.
    """

    kind: Literal["suspended"]
    value: EventSuspended


EventResult = Union[
    EventResultAck,
    EventResultRetry,
    EventResultReject,
    EventResultDeadLetter,
    EventResultSuspended,
]
"""Per-event outcome inside an `EventConsumer.event` batch reply."""


class EventBatch(TypedDict):
    """One `EventConsumer.event` delivery: at most `maxListPage` events, each
    bounded by `maxEventPayloadBytes` / `maxCheckpointBytes`.

    Attributes:
        events: Events in delivery order; the reply carries one `EventResult` per entry.
    """

    events: list[DomainEvent]


class EventBatchReplyOk(TypedDict):
    """``EventBatchReply`` member ``ok``.

    Success: per-event outcomes.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: per-event outcomes.
    """

    kind: Literal["ok"]
    value: list[EventResult]


class EventBatchReplyErr(TypedDict):
    """``EventBatchReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


EventBatchReply = Union[
    EventBatchReplyOk,
    EventBatchReplyErr,
]
"""Result union of `EventConsumer.event`. `ok` has exactly one entry per
`EventBatch.events` entry, in order; a short list is a host-side error and
the missing tail is retried.
"""


class JobController(TypedDict):
    """Everything one `JobRunner.job` invocation may touch. Capabilities are
    host-served; the guest never sees OS paths or sockets.

    Attributes:
        invocation: Durable command envelope.
        input: Job input objects.
        output: Job output object store.
        progress: Progress reporter (host coalesces frequent updates).
        cancel: Host cancellation probe for this job (fence / lease).
    """

    invocation: JobInvocation
    input: Source
    output: Destination
    progress: ProgressSink
    cancel: Cancellation


class HeadOk(TypedDict):
    """Success payload of `Destination.head`.

    Attributes:
        found: Whether the object exists.
        meta: Object metadata; meaningful only when `found`.
    """

    found: bool
    meta: ObjectMetadata


class HeadReplyOk(TypedDict):
    """``HeadReply`` member ``ok``.

    Success: head probe outcome.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: head probe outcome.
    """

    kind: Literal["ok"]
    value: HeadOk


class HeadReplyErr(TypedDict):
    """``HeadReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


HeadReply = Union[
    HeadReplyOk,
    HeadReplyErr,
]
"""Result union of `Destination.head`."""


class ListReplyOk(TypedDict):
    """``ListReply`` member ``ok``.

    Success: one list page.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one list page.
    """

    kind: Literal["ok"]
    value: ListPage


class ListReplyErr(TypedDict):
    """``ListReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


ListReply = Union[
    ListReplyOk,
    ListReplyErr,
]
"""Result union of `Destination.list`."""


class GetOk(TypedDict):
    """Success payload of `Destination.get`.

    Attributes:
        meta: Object metadata.
        body: Streamed object bytes.
    """

    meta: ObjectMetadata
    body: ByteSource


class GetReplyOk(TypedDict):
    """``GetReply`` member ``ok``.

    Success: object metadata and body stream.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: object metadata and body stream.
    """

    kind: Literal["ok"]
    value: GetOk


class GetReplyErr(TypedDict):
    """``GetReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


GetReply = Union[
    GetReplyOk,
    GetReplyErr,
]
"""Result union of `Destination.get`."""


class PutReplyOk(TypedDict):
    """``PutReply`` member ``ok``.

    Success: stored object summary.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: stored object summary.
    """

    kind: Literal["ok"]
    value: PutResult


class PutReplyErr(TypedDict):
    """``PutReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


PutReply = Union[
    PutReplyOk,
    PutReplyErr,
]
"""Result union of `Destination.put` / `Destination.commit`."""


class CopyReplyOk(TypedDict):
    """``CopyReply`` member ``ok``.

    Success: copy summary.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: copy summary.
    """

    kind: Literal["ok"]
    value: CopyResult


class CopyReplyErr(TypedDict):
    """``CopyReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


CopyReply = Union[
    CopyReplyOk,
    CopyReplyErr,
]
"""Result union of `Destination.copy`."""


class EmptyReplyOk(TypedDict):
    """``EmptyReply`` member ``ok``.

    Success: no payload.

    Attributes:
        kind: Always ``"ok"``.
    """

    kind: Literal["ok"]


class EmptyReplyErr(TypedDict):
    """``EmptyReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


EmptyReply = Union[
    EmptyReplyOk,
    EmptyReplyErr,
]
"""Result union of `methods without a success payload`."""


class PullOk(TypedDict):
    """Success payload of `ByteSource.pull`.

    Attributes:
        chunk: Next bytes; may be shorter than requested.
        done: True when the stream is exhausted after `chunk`.
    """

    chunk: bytes
    done: bool


class PullReplyOk(TypedDict):
    """``PullReply`` member ``ok``.

    Success: one stream window.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one stream window.
    """

    kind: Literal["ok"]
    value: PullOk


class PullReplyErr(TypedDict):
    """``PullReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


PullReply = Union[
    PullReplyOk,
    PullReplyErr,
]
"""Result union of `ByteSource.pull`."""


class OpenOk(TypedDict):
    """Success payload of `Source.open`.

    Attributes:
        meta: Object metadata.
        body: Streamed object bytes.
    """

    meta: ObjectMetadata
    body: ByteSource


class OpenReplyOk(TypedDict):
    """``OpenReply`` member ``ok``.

    Success: object metadata and body stream.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: object metadata and body stream.
    """

    kind: Literal["ok"]
    value: OpenOk


class OpenReplyErr(TypedDict):
    """``OpenReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


OpenReply = Union[
    OpenReplyOk,
    OpenReplyErr,
]
"""Result union of `Source.open`."""


class DescribeReplyOk(TypedDict):
    """``DescribeReply`` member ``ok``.

    Success: plugin identity.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: plugin identity.
    """

    kind: Literal["ok"]
    value: PluginDescribe


class DescribeReplyErr(TypedDict):
    """``DescribeReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


DescribeReply = Union[
    DescribeReplyOk,
    DescribeReplyErr,
]
"""Result union of `PluginWorker.describe`."""


class HandleReplyOk(TypedDict):
    """``HandleReply`` member ``ok``.

    Success: job outcome.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: job outcome.
    """

    kind: Literal["ok"]
    value: JobOutcome


class HandleReplyErr(TypedDict):
    """``HandleReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


HandleReply = Union[
    HandleReplyOk,
    HandleReplyErr,
]
"""Result union of `JobRunner.job`."""


class HealthOk(TypedDict):
    """Typed liveness report.

    Attributes:
        ok: True when the guest is healthy enough for traffic.
        detail: Short human status line; empty when none.
    """

    ok: bool
    detail: str


class HealthReplyOk(TypedDict):
    """``HealthReply`` member ``ok``.

    Success: health status.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: health status.
    """

    kind: Literal["ok"]
    value: HealthOk


class HealthReplyErr(TypedDict):
    """``HealthReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


HealthReply = Union[
    HealthReplyOk,
    HealthReplyErr,
]
"""Result union of `health`."""


class AdapterSessionReplyOk(TypedDict):
    """``AdapterSessionReply`` member ``ok``.

    Success: opened `AdapterDatabaseSession` capability.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: opened `AdapterDatabaseSession` capability.
    """

    kind: Literal["ok"]
    value: AdapterDatabaseSession


class AdapterSessionReplyErr(TypedDict):
    """``AdapterSessionReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


AdapterSessionReply = Union[
    AdapterSessionReplyOk,
    AdapterSessionReplyErr,
]
"""Result union of `Database.openSession`."""


class GuestDatabaseReplyOk(TypedDict):
    """``GuestDatabaseReply`` member ``ok``.

    Success: opened `GuestDatabase` capability.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: opened `GuestDatabase` capability.
    """

    kind: Literal["ok"]
    value: GuestDatabase


class GuestDatabaseReplyErr(TypedDict):
    """``GuestDatabaseReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


GuestDatabaseReply = Union[
    GuestDatabaseReplyOk,
    GuestDatabaseReplyErr,
]
"""Result union of `guest database opens`."""


class ByteSource(Protocol):
    """Transferred readable byte stream. The capability *is* the stream; callers
    pull bounded windows. Abort is capability drop / RPC cancel. A failed pull
    MUST set `err` — never a successful empty EOF.
    """

    async def pull(self, max_bytes: int) -> PullReply:
        """Pull the next window of bytes. `done = true` on the final chunk.

        Args:
            max_bytes: Upper bound for this window; at most `maxStreamWindowBytes`.

        Returns:
            ``PullReply``
        """
        ...


class Destination(Protocol):
    """Object store the host writes acquired media into (`output.*` plugins).
    Keys are relative object paths; the plugin owns the physical layout.
    """

    async def head(self, key: str) -> HeadReply:
        """Metadata probe. `found = false` is a success, not `not_found`.

        Args:
            key: Object key to probe.

        Returns:
            ``HeadReply``
        """
        ...

    async def list(self, options: ListOptions) -> ListReply:
        """Page through objects under a prefix; at most `maxListPage` per page.

        Args:
            options: Prefix, cursor, and page size.

        Returns:
            ``ListReply``
        """
        ...

    async def get(self, key: str, options: ReadOptions) -> GetReply:
        """Open an object (or a byte range of it) for reading.

        Args:
            key: Object key to read.
            options: Optional byte range.

        Returns:
            ``GetReply``
        """
        ...

    async def put(self, key: str, body: ByteSource, options: WriteOptions) -> PutReply:
        """Store an object from a transferred byte stream.

        Args:
            key: Object key to write.
            body: Streamed object bytes.
            options: Content type, length, digest, staging.

        Returns:
            ``PutReply``
        """
        ...

    async def copy(self, from_: str, to: str) -> CopyReply:
        """Server-side copy (requires `storage.copy`).

        Args:
            from_: Source object key.
            to: Destination object key.

        Returns:
            ``CopyReply``
        """
        ...

    async def delete(self, key: str) -> EmptyReply:
        """Remove an object; deleting a missing key is a success.

        Args:
            key: Object key to remove.

        Returns:
            ``EmptyReply``
        """
        ...

    async def commit(self, key: str, commit_token: str) -> PutReply:
        """Finalize a destination-side staged object. Staging itself is `put` with
        `stageOnly = true`; bytes must stream into destination-managed temp/multipart
        storage, never a complete local spool on host/adapter/broker/guest.

        Args:
            key: Object key that was staged.
            commit_token: Token from `WriteOptions.commitToken`.

        Returns:
            ``PutReply``
        """
        ...

    async def abort_stage(self, key: str, commit_token: str) -> EmptyReply:
        """Discard a staged object without publishing it.

        Args:
            key: Object key that was staged.
            commit_token: Token from `WriteOptions.commitToken`.

        Returns:
            ``EmptyReply``
        """
        ...


class Source(Protocol):
    """Read-only byte source for job inputs (not a storefront)."""

    async def open(self, key: str) -> OpenReply:
        """Open an object for streaming reads.

        Args:
            key: Object key to open.

        Returns:
            ``OpenReply``
        """
        ...


class ProgressSink(Protocol):
    """Host-side progress reporter handed to job handlers."""

    async def report(self, percent: float, message: str) -> EmptyReply:
        """Report progress; the host coalesces frequent updates.

        Args:
            percent: Completion in `[0, 100]`.
            message: Short human-readable status line.

        Returns:
            ``EmptyReply``
        """
        ...


class Cancellation(Protocol):
    """Transport cancellation. SDKs project this into a locally created AbortSignal
    (AbortSignal is not a serializable Workers RPC value).
    """

    async def poll(self) -> bool:
        """Non-blocking check; `true` once the host has fenced the invocation.

        Returns:
            ``bool``
        """
        ...


class JobRunner(Protocol):
    """`[triggers] jobs` handler (`Entrypoints.jobRunner`); runs one durable command."""

    async def job(self, controller: JobController) -> HandleReply:
        """Run one command invocation to a terminal or suspended outcome. Named
        database bindings come from `Bindings.databases` at `open`, not per job.

        Args:
            controller: Envelope plus host-served capabilities.

        Returns:
            ``HandleReply``
        """
        ...


class EventConsumer(Protocol):
    """`[[events.consumers]]` handler (`Entrypoints.eventConsumer`). Delivery is
    at-least-once; consumers must be idempotent on `deduplicationKey`.
    """

    async def event(self, batch: EventBatch) -> EventBatchReply:
        """Deliver one ordered batch of domain events.

        Args:
            batch: Events in delivery order.

        Returns:
            ``EventBatchReply``
        """
        ...


class EventPublisher(Protocol):
    """`EVENTS` binding: host-served outbox publisher granted through
    `Bindings.events`. Event types must be listed in `[[events.producers]]`.
    """

    async def publish(self, event: PluginEvent) -> PublishReply:
        """Append one event to the outbox (idempotent on `deduplicationKey`).

        Args:
            event: Event to publish.

        Returns:
            ``PublishReply``
        """
        ...


class NamedDatabase(TypedDict):
    """One named plugin-owned database binding delivered on `Bindings.databases`.

    Attributes:
        name: Binding name from `plugin.toml` `[[databases]]`.
        database: Isolated typed SQL session for this binding (plugin-owned schema).
    """

    name: str
    database: GuestDatabase


class ContentSource(Protocol):
    """Storefront content source (not byte Source). Every method takes and
    returns typed structs from the "Typed method payloads" section.
    """

    async def login(self, params: LoginParams) -> LoginReply:
        """Connect an account (password or one-shot OAuth).

        Args:
            params: Credentials, marketplace, callback wiring.

        Returns:
            ``LoginReply``
        """
        ...

    async def scan(self, params: ScanParams) -> ScanReply:
        """Sync library rows for one or more accounts.

        Args:
            params: Accounts, paging, and host-sealed credentials.

        Returns:
            ``ScanReply``
        """
        ...

    async def fetch_title(self, params: FetchTitleParams) -> FetchTitleReply:
        """Download and decrypt one title into `cacheDir`.

        Args:
            params: Title, credentials, and fetch options.

        Returns:
            ``FetchTitleReply``
        """
        ...

    async def list_accounts(self) -> SourceAccountsReply:
        """Enumerate accounts the guest knows about.

        Returns:
            ``SourceAccountsReply``
        """
        ...

    async def login_start(self, params: LoginParams) -> LoginStartReply:
        """Begin an interactive OAuth login; returns a session id.

        Args:
            params: Same shape as `login`; host fills callback IPC.

        Returns:
            ``LoginStartReply``
        """
        ...

    async def login_complete(self, params: LoginCompleteParams) -> LoginReply:
        """Finish an interactive OAuth login started by `loginStart`.

        Args:
            params: Session id from `loginStart`.

        Returns:
            ``LoginReply``
        """
        ...

    async def search_catalog(self, params: SearchCatalogParams) -> CatalogHitsReply:
        """Free-text storefront catalog search.

        Args:
            params: Query, region, paging, sort, facet.

        Returns:
            ``CatalogHitsReply``
        """
        ...

    async def expand_candidates(self, params: ExpandCandidatesParams) -> CatalogHitsReply:
        """Related-title expansion from a seed title.

        Args:
            params: Seed identity fields and limit.

        Returns:
            ``CatalogHitsReply``
        """
        ...

    async def purchase_hint(self, params: PurchaseHintParams) -> PurchaseHintReply:
        """Purchase link / price hint for one title.

        Args:
            params: Identity fields and price flag.

        Returns:
            ``PurchaseHintReply``
        """
        ...

    async def list_deals(self, params: ListDealsParams) -> CatalogHitsReply:
        """Current storefront deals.

        Args:
            params: Optional result cap.

        Returns:
            ``CatalogHitsReply``
        """
        ...

    async def health(self) -> HealthReply:
        """Liveness / readiness probe.

        Returns:
            ``HealthReply``
        """
        ...

    async def diagnose(self) -> DiagnoseReply:
        """Human-readable diagnostic lines.

        Returns:
            ``DiagnoseReply``
        """
        ...

    async def catalog_detail(self, params: CatalogDetailParams) -> CatalogDetailReply:
        """Full catalog record for one product.

        Args:
            params: Product id and optional ISBN.

        Returns:
            ``CatalogDetailReply``
        """
        ...


class RemoteLibrary(Protocol):
    """`remoteLibrary` entrypoint: long-running remote-library lifecycle (start /
    stop, library rescan, listening sync, external-user polling). Event delivery
    is `EventConsumer`; credential verification is `Oidc`.
    """

    async def health(self) -> HealthReply:
        """Liveness / readiness probe.

        Returns:
            ``HealthReply``
        """
        ...

    async def start(self) -> EmptyReply:
        """Start background work after the host has granted bindings.

        Returns:
            ``EmptyReply``
        """
        ...

    async def stop(self) -> EmptyReply:
        """Stop background work; the host may drop the capability afterwards.

        Returns:
            ``EmptyReply``
        """
        ...

    async def diagnose(self) -> DiagnoseReply:
        """Human-readable diagnostic lines.

        Returns:
            ``DiagnoseReply``
        """
        ...

    async def scan_library(self, params: ScanLibraryParams) -> EmptyReply:
        """Re-sync the remote library.

        Args:
            params: Full-rescan flag.

        Returns:
            ``EmptyReply``
        """
        ...

    async def sync_listening(self) -> SyncListeningReply:
        """Push / pull listening progress.

        Returns:
            ``SyncListeningReply``
        """
        ...

    async def poll_events(self) -> EventPollReply:
        """Drain external users the remote side observed since the last poll.

        Returns:
            ``EventPollReply``
        """
        ...


class PluginCli(Protocol):
    """`cli` entrypoint: guest commands under `bookclerk plugins <id> <command>`."""

    async def describe(self) -> CliSchemaReply:
        """Declared CLI surface.

        Returns:
            ``CliSchemaReply``
        """
        ...

    async def invoke(self, params: CliInvokeParams) -> CliInvokeReply:
        """Run one plugin CLI command.

        Args:
            params: Command name and argument values.

        Returns:
            ``CliInvokeReply``
        """
        ...


class Oidc(Protocol):
    """`oidc` entrypoint: relying-party client templates and credential
    verification on behalf of the host authorization server.
    """

    async def clients(self) -> OidcClientsReply:
        """Plugin-provided OIDC AS client templates. Empty list when unused.

        Returns:
            ``OidcClientsReply``
        """
        ...

    async def authenticate_user(self, params: AuthenticateUserParams) -> ExternalUserReply:
        """Verify remote credentials on behalf of the host.

        Args:
            params: Username and password.

        Returns:
            ``ExternalUserReply``
        """
        ...


class EventConsumerSpec(TypedDict):
    """One declared event consumer: a `[[events.consumers]]` row the default
    entrypoint's `event(batch)` handler accepts.

    Attributes:
        eventType: Versioned event type (snake_case, e.g. `book_acquired`).
        schemaVersions: Schema versions the guest can consume; never empty.
        supportsSuspend: Whether `EventResult.suspended` is supported for this type.
    """

    eventType: str
    schemaVersions: list[int]
    supportsSuspend: bool


class PluginCapabilities(TypedDict):
    """Typed capability declaration returned by `describe()`. The host compares
    it with `plugin.toml` and the operator grant; widening is rejected at spawn.

    Attributes:
        entrypoints: Named entrypoints the guest exports.
        consumes: Event types the default entrypoint consumes (`event(batch)` trigger).
        produces: Event types the guest may publish through its `EVENTS` binding.
        jobs: Command types the default entrypoint runs (`job(controller)` trigger).
        databases: Plugin-owned database binding names (`[[databases]]`).
        bindings: Other named bindings the guest expects on `env` (`CONFIG`, `SECRETS`,
            `WORK_FS`, `OAUTH`, `KV`, `EVENTS`, ...).
    """

    entrypoints: list[Entrypoint]
    consumes: list[EventConsumerSpec]
    produces: list[str]
    jobs: list[str]
    databases: list[str]
    bindings: list[str]


class Brand(TypedDict):
    """Portal brand crossing the RPC boundary. Distinct from `plugin.toml`
    `logo`: `iconUrl` is the live URL or data URI the SPA renders.

    Attributes:
        id: Brand id (often matches the plugin id); empty means "no brand".
        name: Display name shown next to the brand swatch.
        bg: Background CSS color (hex or named).
        fg: Foreground CSS color for text on `bg`.
        accent: Accent CSS color for highlights / CTAs.
        iconUrl: Icon URL or data URI for the portal. Omitted when absent (wire zero
            value).
    """

    id: str
    name: str
    bg: str
    fg: str
    accent: str
    iconUrl: NotRequired[str]


class ConfigOption(TypedDict):
    """One discoverable config option group advertised for sources.

    Attributes:
        key: Config key under the plugin's `config.toml` table.
        label: Operator-facing label for the option group.
        values: Allowed selectable values for this key.
    """

    key: str
    label: str
    values: list[ConfigOptionValue]


class ConfigOptionValue(TypedDict):
    """One selectable value under a `ConfigOption`.

    Attributes:
        id: Value written to config when selected.
        label: Operator-facing label for this value.
    """

    id: str
    label: str


class CliSchema(TypedDict):
    """Declared plugin CLI surface (`PluginCli.describe` / `describe().cli`).

    Attributes:
        commands: Commands exposed as `bookclerk plugins <id> <command> ...`.
    """

    commands: list[CliCommandSpec]


class CliCommandSpec(TypedDict):
    """One plugin CLI command under `CliSchema`.

    Attributes:
        name: Command verb after the plugin id (for example "ping").
        about: Short help text for `--help`. Omitted when absent (wire zero value).
        args: Argument / flag specs for this command.
    """

    name: str
    about: NotRequired[str]
    args: list[CliArgSpec]


class CliArgSpec(TypedDict):
    """One CLI argument or flag under a `CliCommandSpec`.

    Attributes:
        name: Internal arg name used as `CliArg.name` on invoke.
        long: Long flag without leading dashes (e.g. "message" -> `--message`). Omitted
            when absent (wire zero value).
        short: Short flag character (e.g. "m" -> `-m`). Omitted when absent (wire zero
            value).
        kind: Parsed value kind.
        required: When true, the host rejects invoke if the arg is missing.
        default: Default string form when the operator omits the arg. Omitted when
            absent (wire zero value).
        about: Help text for this arg. Omitted when absent (wire zero value).
        positional: When true, the arg is positional rather than a flagged option.
    """

    name: str
    long: NotRequired[str]
    short: NotRequired[str]
    kind: CliArgKind
    required: bool
    default: NotRequired[str]
    about: NotRequired[str]
    positional: bool


class CliArg(TypedDict):
    """One named argument value passed to `PluginCli.invoke`.

    Attributes:
        name: Arg name matching a `CliArgSpec.name`.
        value: String form of the value (the guest parses per `CliArgSpec.kind`).
    """

    name: str
    value: str


class CliInvokeParams(TypedDict):
    """Params of `PluginCli.invoke`.

    Attributes:
        command: Command name matching a `CliCommandSpec.name`.
        args: Named argument values.
    """

    command: str
    args: list[CliArg]


class CliInvokeResult(TypedDict):
    """Result of `PluginCli.invoke`.

    Attributes:
        exitCode: Process-style exit code (0 = success).
        stdout: Captured standard output text.
        stderr: Captured standard error text.
        payload: Structured payload for machine consumers; empty `mediaType` when
            absent.
    """

    exitCode: int
    stdout: str
    stderr: str
    payload: ExtensibleConfig


class CliSchemaReplyOk(TypedDict):
    """``CliSchemaReply`` member ``ok``.

    Success: declared CLI surface.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: declared CLI surface.
    """

    kind: Literal["ok"]
    value: CliSchema


class CliSchemaReplyErr(TypedDict):
    """``CliSchemaReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


CliSchemaReply = Union[
    CliSchemaReplyOk,
    CliSchemaReplyErr,
]
"""Result union of `PluginCli.describe`."""


class CliInvokeReplyOk(TypedDict):
    """``CliInvokeReply`` member ``ok``.

    Success: command output.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: command output.
    """

    kind: Literal["ok"]
    value: CliInvokeResult


class CliInvokeReplyErr(TypedDict):
    """``CliInvokeReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


CliInvokeReply = Union[
    CliInvokeReplyOk,
    CliInvokeReplyErr,
]
"""Result union of `PluginCli.invoke`."""


class DatabaseAdapterConfig(TypedDict):
    """Author-facing database adapter configuration carried in
    `DatabaseContext.adapter`. This is the generic bootstrap mechanism for
    third-party adapters: the operator's granted `[database.<id>]` table plus
    the scoped writable data dir. First-party host-managed adapters receive
    host-private connect params in `DatabaseContext.config` instead.

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin
            (`.../plugins/<id>/data`).
        settings: Granted plugin settings (operator `[database.<id>]` table) as
            `application/json`; `{}` when the operator configured nothing.
        binding: Named plugin database binding this open serves; empty for the primary
            library open. Adapters advertising `DbCapabilities.pluginDatabases` must
            serve each binding from its own isolated database. Omitted when absent (wire
            zero value).
        instanceId: Host-issued opaque instance id for this (owner plugin, binding)
            pair. Collision-resistant and stable across re-opens. Empty for the primary
            library open. Third-party adapters must key isolated databases on this value
            rather than `binding` alone (two plugins may both declare `DB`). Omitted
            when absent (wire zero value).
        openExisting: When true, open an existing binding unit and do not provision a
            missing one (read-only backup capture). False lets the adapter create the
            unit.
    """

    pluginDataDir: str
    settings: ExtensibleConfig
    binding: NotRequired[str]
    instanceId: NotRequired[str]
    openExisting: bool


class DiagnoseResult(TypedDict):
    """Operator-facing diagnostic lines printed by `bookclerk plugins diagnose`.

    Attributes:
        lines: Human-readable probe lines.
    """

    lines: list[str]


class DiagnoseReplyOk(TypedDict):
    """``DiagnoseReply`` member ``ok``.

    Success: diagnostic lines.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: diagnostic lines.
    """

    kind: Literal["ok"]
    value: DiagnoseResult


class DiagnoseReplyErr(TypedDict):
    """``DiagnoseReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


DiagnoseReply = Union[
    DiagnoseReplyOk,
    DiagnoseReplyErr,
]
"""Result union of `diagnose`."""


class SourceAccount(TypedDict):
    """Source account metadata returned from login and stored by the host.

    Attributes:
        accountId: Stable account id within this source plugin.
        source: Source plugin id (the host forces this to the guest's install id).
        marketplace: Storefront marketplace / region code (for example `us`, `uk`).
        label: Operator-facing label. Omitted when absent (wire zero value).
        scanEnabled: When true, bare / scheduled scans include this account. Explicit
            CLI `--account` bypasses this flag.
    """

    accountId: str
    source: str
    marketplace: str
    label: NotRequired[str]
    scanEnabled: bool


class LoginParams(TypedDict):
    """Params of `ContentSource.login` and `ContentSource.loginStart`. Password
    sources fill email/password; OAuth sources use callback / external fields.
    There is no files-dir root or library DB path -- only `pluginDataDir`.

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin only
            (`.../plugins/<id>/data`).
        marketplace: Marketplace / locale for the storefront; empty means the guest
            default.
        label: Operator label stored on the account row. Omitted when absent (wire zero
            value).
        email: Account email / username for password logins; empty for pure OAuth.
            Omitted when absent (wire zero value).
        password: Account password for password logins; never logged; empty for OAuth.
            Omitted when absent (wire zero value).
        force: When true, overwrite an existing sealed credential for this account.
        callbackBind: Bind address for OAuth callback servers (`host:port`). Ignored
            when `callbackIpc` is set (host owns the TCP listener). Omitted when absent
            (wire zero value).
        callbackIpc: Host-owned callback IPC endpoint the guest must connect to. When
            set (with `callbackPublicBase`), the guest must not bind a TCP listener.
            Omitted when absent (wire zero value).
        callbackPublicBase: Public base URL for the host TCP listener, e.g.
            `http://127.0.0.1:12345`. Omitted when absent (wire zero value).
        external: When true, use external / paste-redirect OAuth instead of a local
            callback server.
        responseUrl: Pre-supplied OAuth redirect URL (paste flow). Omitted when absent
            (wire zero value).
        showQr: Prefer QR output when the guest supports it.
        timeoutSecs: Seconds to wait for OAuth callback capture; guest default when 0.
            Omitted when absent (wire zero value).
        extra: Store-specific knobs as `application/json`; guests may ignore unknowns.
    """

    pluginDataDir: str
    marketplace: str
    label: NotRequired[str]
    email: NotRequired[str]
    password: NotRequired[str]
    force: bool
    callbackBind: NotRequired[str]
    callbackIpc: NotRequired[str]
    callbackPublicBase: NotRequired[str]
    external: bool
    responseUrl: NotRequired[str]
    showQr: bool
    timeoutSecs: NotRequired[int]
    extra: ExtensibleConfig


class LoginResult(TypedDict):
    """Result of `ContentSource.login` / `loginComplete`: account metadata plus
    opaque credentials for the host to seal into `encrypted_secrets`
    (`provider = plugin id`). Guests never write secrets into the library DB.

    Attributes:
        account: Account row fields for the host to upsert.
        credentials: Opaque credential blob the host seals; empty when login only
            refreshed metadata. Guests choose the encoding (typically JSON bytes).
            Omitted when absent (wire zero value).
    """

    account: SourceAccount
    credentials: NotRequired[bytes]


class LoginStartResult(TypedDict):
    """Result of `ContentSource.loginStart` (interactive OAuth). The operator
    opens `url`; `loginComplete` later uses `sessionId`.

    Attributes:
        sessionId: Opaque session id for `loginComplete`.
        url: Browser URL the operator should open to complete OAuth.
    """

    sessionId: str
    url: str


class LoginCompleteParams(TypedDict):
    """Params of `ContentSource.loginComplete`.

    Attributes:
        sessionId: Session id previously returned by `loginStart`.
    """

    sessionId: str


class LoginReplyOk(TypedDict):
    """``LoginReply`` member ``ok``.

    Success: account plus credentials to seal.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: account plus credentials to seal.
    """

    kind: Literal["ok"]
    value: LoginResult


class LoginReplyErr(TypedDict):
    """``LoginReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


LoginReply = Union[
    LoginReplyOk,
    LoginReplyErr,
]
"""Result union of `ContentSource.login` / `loginComplete`."""


class LoginStartReplyOk(TypedDict):
    """``LoginStartReply`` member ``ok``.

    Success: session id and browser URL.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: session id and browser URL.
    """

    kind: Literal["ok"]
    value: LoginStartResult


class LoginStartReplyErr(TypedDict):
    """``LoginStartReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


LoginStartReply = Union[
    LoginStartReplyOk,
    LoginStartReplyErr,
]
"""Result union of `ContentSource.loginStart`."""


class AccountCredential(TypedDict):
    """Host-sealed credentials for one account, delivered on `scan`.

    Attributes:
        accountId: Account id the blob belongs to.
        credentials: Opaque credential bytes exactly as the guest returned them at
            login.
    """

    accountId: str
    credentials: bytes


class ScanParams(TypedDict):
    """Params of `ContentSource.scan`. The host injects sealed credentials so the
    plugin does not need a private credential store under `pluginDataDir`.

    Attributes:
        pluginDataDir: Scoped plugin data directory.
        accounts: Account ids to scan; empty means all scan-enabled accounts.
        pageSize: Storefront page size; the host always sends an explicit value.
        importEpisodes: When true, import podcast/episode-style rows.
        importPlusTitles: When true, import Plus/catalog entitlement titles.
        credentials: Host-loaded credential blobs for the requested accounts.
    """

    pluginDataDir: str
    accounts: list[str]
    pageSize: int
    importEpisodes: bool
    importPlusTitles: bool
    credentials: list[AccountCredential]


class ScanBook(TypedDict):
    """One library title returned by `ContentSource.scan`. The host upserts these
    rows and forces `source` to the plugin id.

    Attributes:
        accountId: Account that owns this library entry.
        productId: Storefront product / SKU id.
        title: Primary title string.
        marketplace: Marketplace / region when known. Omitted when absent (wire zero
            value).
        asin: Amazon ASIN when the storefront exposes one. Omitted when absent (wire
            zero value).
        isbn: ISBN when the storefront exposes one. Omitted when absent (wire zero
            value).
        authors: Comma- or guest-formatted author list. Omitted when absent (wire zero
            value).
        narrators: Comma- or guest-formatted narrator list. Omitted when absent (wire
            zero value).
        series: Series name when applicable. Omitted when absent (wire zero value).
        seriesIndex: Series index / sequence label. Omitted when absent (wire zero
            value).
        contentKind: Content classification (e.g. `book` vs `episode`). Omitted when
            absent (wire zero value).
        publisher: Publisher name when known. Omitted when absent (wire zero value).
        lengthMinutes: Runtime in whole minutes when known. Omitted when absent (wire
            zero value).
        subtitle: Subtitle when distinct from `title`. Omitted when absent (wire zero
            value).
    """

    accountId: str
    productId: str
    title: str
    marketplace: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    authors: NotRequired[str]
    narrators: NotRequired[str]
    series: NotRequired[str]
    seriesIndex: NotRequired[str]
    contentKind: NotRequired[str]
    publisher: NotRequired[str]
    lengthMinutes: NotRequired[int]
    subtitle: NotRequired[str]


class ScanSummary(TypedDict):
    """Summary result of `ContentSource.scan`.

    Attributes:
        accounts: Number of accounts touched during the scan.
        booksUpserted: Count of titles the guest expects the host to upsert; may mirror
            `books.length`.
        pages: Number of storefront pages fetched.
        skippedDisabled: Accounts skipped because `scanEnabled` was false.
        books: Titles for the host to upsert. Prefer this over plugin-side DB writes.
    """

    accounts: int
    booksUpserted: int
    pages: int
    skippedDisabled: int
    books: list[ScanBook]


class ScanReplyOk(TypedDict):
    """``ScanReply`` member ``ok``.

    Success: scan summary and titles to upsert.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: scan summary and titles to upsert.
    """

    kind: Literal["ok"]
    value: ScanSummary


class ScanReplyErr(TypedDict):
    """``ScanReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


ScanReply = Union[
    ScanReplyOk,
    ScanReplyErr,
]
"""Result union of `ContentSource.scan`."""


class FetchOptions(TypedDict):
    """Fetch-relevant acquire knobs the host forwards so external load matches
    in-process. Packaging / naming knobs stay host-side.

    Attributes:
        widevine: Prefer Widevine/CENC download when the store offers it.
        xheAac: Prefer xHE-AAC on the Widevine path when offered.
        widevineCdmPath: Local Widevine `.wvd` path granted to the guest. Omitted when
            absent (wire zero value).
        widevineCdmProvider: Remote L3 CDM provider URL; empty means the classic
            default, `off` disables remote provisioning. Omitted when absent (wire zero
            value).
        downloadCover: When true, download a cover image alongside audio.
        downloadPdf: When true, download a companion PDF when the store exposes one.
        coverSize: Cover image size request (`500`, `1215`, or `native`).
        chapterLayout: Preferred chapter API layout when fetching (`tree` or `flat`).
        stripAudibleBrandAudio: When true, trim Audible brand intro/outro from the remux
            window.
        downloadClipsBookmarks: When true, download clips/bookmarks sidecars when
            offered.
        retainAaxFile: When true, keep the encrypted download in storage.
        downloadSpeedLimitKbps: Fetch speed cap in KB/s (`0` = unlimited).
        saveMetadataJson: When true, persist raw catalog API JSON as `metadata.json`.
    """

    widevine: bool
    xheAac: bool
    widevineCdmPath: NotRequired[str]
    widevineCdmProvider: NotRequired[str]
    downloadCover: bool
    downloadPdf: bool
    coverSize: str
    chapterLayout: str
    stripAudibleBrandAudio: bool
    downloadClipsBookmarks: bool
    retainAaxFile: bool
    downloadSpeedLimitKbps: int
    saveMetadataJson: bool


class FetchTitleParams(TypedDict):
    """Params of `ContentSource.fetchTitle`. The plugin writes media under
    `cacheDir` and returns plain (DRM-free) paths. The host injects
    credentials; guests must not open `library.db` or `master.key`.

    Attributes:
        pluginDataDir: Scoped plugin data directory.
        accountId: Account whose credentials apply.
        titleId: Library / storefront title id to download.
        cacheDir: Absolute path the guest should write media into (jail-granted TMPDIR).
        credentials: Host-loaded credential blob for this account; empty when
            unavailable. Omitted when absent (wire zero value).
        sourceConfig: Granted `[sources.<id>]` table as `application/json`.
        fetch: Fetch-relevant acquire options.
    """

    pluginDataDir: str
    accountId: str
    titleId: str
    cacheDir: str
    credentials: NotRequired[bytes]
    sourceConfig: ExtensibleConfig
    fetch: FetchOptions


class PlainPart(TypedDict):
    """One plain audio part written under the cache directory.

    Attributes:
        path: Absolute path to the part file under `cacheDir`.
        title: Part title (disc / chapter label). Omitted when absent (wire zero value).
        durationMs: Duration of this part in milliseconds when known. Omitted when
            absent (wire zero value).
    """

    path: str
    title: NotRequired[str]
    durationMs: NotRequired[int]


class ChapterMarker(TypedDict):
    """One chapter marker of a fetched title.

    Attributes:
        title: Chapter title.
        startMs: Chapter start offset in milliseconds from the beginning of the title.
    """

    title: str
    startMs: int


class PlainFetch(TypedDict):
    """Plain (DRM-free) fetch result. Sources always return decrypted media; DRM
    guests decrypt before responding.

    Attributes:
        parts: Ordered audio part files written under the cache directory.
        m4bPath: Single M4B path when the guest assembled one. Omitted when absent (wire
            zero value).
        coverPath: Cover image path under the cache directory. Omitted when absent (wire
            zero value).
        chapters: Chapter markers; empty when unknown.
        pdfUrl: Companion PDF download URL when the store exposes one. Omitted when
            absent (wire zero value).
    """

    parts: list[PlainPart]
    m4bPath: NotRequired[str]
    coverPath: NotRequired[str]
    chapters: list[ChapterMarker]
    pdfUrl: NotRequired[str]


class FetchTitleReplyOk(TypedDict):
    """``FetchTitleReply`` member ``ok``.

    Success: plain media paths.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: plain media paths.
    """

    kind: Literal["ok"]
    value: PlainFetch


class FetchTitleReplyErr(TypedDict):
    """``FetchTitleReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


FetchTitleReply = Union[
    FetchTitleReplyOk,
    FetchTitleReplyErr,
]
"""Result union of `ContentSource.fetchTitle`."""


class SourceAccounts(TypedDict):
    """Success payload of `ContentSource.listAccounts`.

    Attributes:
        accounts: Accounts the guest knows about.
    """

    accounts: list[SourceAccount]


class SourceAccountsReplyOk(TypedDict):
    """``SourceAccountsReply`` member ``ok``.

    Success: account list.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: account list.
    """

    kind: Literal["ok"]
    value: SourceAccounts


class SourceAccountsReplyErr(TypedDict):
    """``SourceAccountsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


SourceAccountsReply = Union[
    SourceAccountsReplyOk,
    SourceAccountsReplyErr,
]
"""Result union of `ContentSource.listAccounts`."""


class SearchCatalogParams(TypedDict):
    """Params of `ContentSource.searchCatalog`.

    Attributes:
        query: Free-text search query.
        region: Storefront region / marketplace code; empty means the guest default.
        limit: Maximum hits to return; the host always sends an explicit value.
        page: 1-based page for storefronts that page.
        sort: Sort order.
        field: Facet restriction.
        language: Preferred content language (soft-prioritize; e.g. `en`). Omitted when
            absent (wire zero value).
    """

    query: str
    region: str
    limit: int
    page: int
    sort: CatalogSort
    field: CatalogField
    language: NotRequired[str]


class ExpandCandidatesParams(TypedDict):
    """Params of `ContentSource.expandCandidates`. Seed fields identify a known
    title; the guest returns related catalog hits.

    Attributes:
        source: Source plugin id hint when expanding across storefronts.
        productId: Seed storefront product id.
        title: Seed title text.
        authors: Seed authors string. Omitted when absent (wire zero value).
        narrators: Seed narrators string. Omitted when absent (wire zero value).
        series: Seed series name. Omitted when absent (wire zero value).
        seriesAsin: Seed series ASIN when known. Omitted when absent (wire zero value).
        asin: Seed Amazon ASIN. Omitted when absent (wire zero value).
        isbn: Seed ISBN. Omitted when absent (wire zero value).
        region: Storefront region / marketplace code.
        limit: Maximum candidates to return; the host always sends an explicit value.
    """

    source: str
    productId: str
    title: str
    authors: NotRequired[str]
    narrators: NotRequired[str]
    series: NotRequired[str]
    seriesAsin: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    region: str
    limit: int


class PurchaseHintParams(TypedDict):
    """Params of `ContentSource.purchaseHint`. At least one identity field
    (`productId` / `asin` / `isbn` / title+authors) should be set; guests may
    return `invalid_params` when none are usable.

    Attributes:
        productId: Storefront product id when known. Omitted when absent (wire zero
            value).
        title: Title text for fuzzy lookup. Omitted when absent (wire zero value).
        authors: Authors string for fuzzy lookup. Omitted when absent (wire zero value).
        asin: Amazon ASIN when known. Omitted when absent (wire zero value).
        isbn: ISBN when known. Omitted when absent (wire zero value).
        region: Storefront region / marketplace code.
        withPrice: When true, guests should include live price fields when available.
    """

    productId: NotRequired[str]
    title: NotRequired[str]
    authors: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    region: str
    withPrice: bool


class ListDealsParams(TypedDict):
    """Params of `ContentSource.listDeals`.

    Attributes:
        limit: Maximum number of deals to return; guest default when 0. Omitted when
            absent (wire zero value).
    """

    limit: NotRequired[int]


class CatalogDetailParams(TypedDict):
    """Params of `ContentSource.catalogDetail`.

    Attributes:
        productId: Store product id (Libro ISBN or ISBN-slug).
        isbn: ISBN when it differs from `productId`. Omitted when absent (wire zero
            value).
    """

    productId: str
    isbn: NotRequired[str]


class CatalogHit(TypedDict):
    """One catalog / candidate hit returned by `searchCatalog`,
    `expandCandidates`, `listDeals`, and `catalogDetail`.

    Attributes:
        productId: Storefront product / SKU id.
        title: Primary title.
        authors: Authors string when known. Omitted when absent (wire zero value).
        narrators: Narrators string when known. Omitted when absent (wire zero value).
        series: Series name when applicable. Omitted when absent (wire zero value).
        seriesIndex: Series index / sequence label. Omitted when absent (wire zero
            value).
        asin: Amazon ASIN when known. Omitted when absent (wire zero value).
        isbn: ISBN when known. Omitted when absent (wire zero value).
        url: Storefront product page URL. Omitted when absent (wire zero value).
        coverUrl: Cover image URL. Omitted when absent (wire zero value).
        origin: Hit origin label (plugin id or storefront name).
        subtitle: Subtitle when distinct from `title`. Omitted when absent (wire zero
            value).
        description: Long description / blurb when fetched. Omitted when absent (wire
            zero value).
        publisher: Publisher name when known. Omitted when absent (wire zero value).
        lengthMinutes: Runtime in whole minutes. Omitted when absent (wire zero value).
        publishedAt: Publication date string as provided by the storefront. Omitted when
            absent (wire zero value).
        categories: Category / genre labels as a single string when known. Omitted when
            absent (wire zero value).
        language: Content language code when known. Omitted when absent (wire zero
            value).
        priceCents: Current price in minor units (cents). Omitted when absent (wire zero
            value).
        currency: ISO currency code for `priceCents`. Omitted when absent (wire zero
            value).
        priceLabel: Pre-formatted price for display. Omitted when absent (wire zero
            value).
        ratingOverall: Aggregate rating when known. Omitted when absent (wire zero
            value).
        ratingCount: Number of ratings when known. Omitted when absent (wire zero
            value).
        abridgement: Whether the edition is abridged when the storefront says so.
    """

    productId: str
    title: str
    authors: NotRequired[str]
    narrators: NotRequired[str]
    series: NotRequired[str]
    seriesIndex: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    url: NotRequired[str]
    coverUrl: NotRequired[str]
    origin: str
    subtitle: NotRequired[str]
    description: NotRequired[str]
    publisher: NotRequired[str]
    lengthMinutes: NotRequired[int]
    publishedAt: NotRequired[str]
    categories: NotRequired[str]
    language: NotRequired[str]
    priceCents: NotRequired[int]
    currency: NotRequired[str]
    priceLabel: NotRequired[str]
    ratingOverall: NotRequired[float]
    ratingCount: NotRequired[int]
    abridgement: Abridgement


class CatalogHits(TypedDict):
    """Success payload of the catalog list methods.

    Attributes:
        hits: Hits in storefront order.
    """

    hits: list[CatalogHit]


class CatalogHitsReplyOk(TypedDict):
    """``CatalogHitsReply`` member ``ok``.

    Success: catalog hits.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: catalog hits.
    """

    kind: Literal["ok"]
    value: CatalogHits


class CatalogHitsReplyErr(TypedDict):
    """``CatalogHitsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


CatalogHitsReply = Union[
    CatalogHitsReplyOk,
    CatalogHitsReplyErr,
]
"""Result union of `searchCatalog` / `expandCandidates` / `listDeals`."""


class CatalogDetail(TypedDict):
    """Success payload of `ContentSource.catalogDetail`.

    Attributes:
        found: False when the product is unknown to the storefront (`hit` is empty).
        hit: Full catalog record when `found`.
    """

    found: bool
    hit: CatalogHit


class CatalogDetailReplyOk(TypedDict):
    """``CatalogDetailReply`` member ``ok``.

    Success: detail record or not-found marker.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: detail record or not-found marker.
    """

    kind: Literal["ok"]
    value: CatalogDetail


class CatalogDetailReplyErr(TypedDict):
    """``CatalogDetailReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


CatalogDetailReply = Union[
    CatalogDetailReplyOk,
    CatalogDetailReplyErr,
]
"""Result union of `ContentSource.catalogDetail`."""


class PurchaseHint(TypedDict):
    """Purchase hint for SPA / CLI purchase deep-links.

    Attributes:
        productId: Storefront product id.
        title: Title when resolved. Omitted when absent (wire zero value).
        url: Purchase or product-page URL. Omitted when absent (wire zero value).
        priceCents: Current price in minor units. Omitted when absent (wire zero value).
        currency: ISO currency code for price fields. Omitted when absent (wire zero
            value).
        priceLabel: Pre-formatted current price. Omitted when absent (wire zero value).
        listPriceCents: List / MSRP price in minor units. Omitted when absent (wire zero
            value).
        listPriceLabel: Pre-formatted list price. Omitted when absent (wire zero value).
        memberPriceCents: Member / Plus price in minor units. Omitted when absent (wire
            zero value).
        memberPriceLabel: Pre-formatted member price. Omitted when absent (wire zero
            value).
    """

    productId: str
    title: NotRequired[str]
    url: NotRequired[str]
    priceCents: NotRequired[int]
    currency: NotRequired[str]
    priceLabel: NotRequired[str]
    listPriceCents: NotRequired[int]
    listPriceLabel: NotRequired[str]
    memberPriceCents: NotRequired[int]
    memberPriceLabel: NotRequired[str]


class PurchaseHintResult(TypedDict):
    """Success payload of `ContentSource.purchaseHint`.

    Attributes:
        found: False when the guest could not resolve the title (`hint` is empty).
        hint: Purchase hint when `found`.
    """

    found: bool
    hint: PurchaseHint


class PurchaseHintReplyOk(TypedDict):
    """``PurchaseHintReply`` member ``ok``.

    Success: hint or not-found marker.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: hint or not-found marker.
    """

    kind: Literal["ok"]
    value: PurchaseHintResult


class PurchaseHintReplyErr(TypedDict):
    """``PurchaseHintReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


PurchaseHintReply = Union[
    PurchaseHintReplyOk,
    PurchaseHintReplyErr,
]
"""Result union of `ContentSource.purchaseHint`."""


class ScanLibraryParams(TypedDict):
    """Params of `RemoteLibrary.scanLibrary` (remote library sync).

    Attributes:
        force: When true, force a full rescan even if the guest would otherwise
            incremental-sync.
    """

    force: bool


class AuthenticateUserParams(TypedDict):
    """Params of `Oidc.authenticateUser`.

    Attributes:
        username: Integration username / login id.
        password: Integration password; never logged by the host.
    """

    username: str
    password: str


class ExternalUser(TypedDict):
    """One external user observed by an integration. The host may mint claim
    tickets without exposing portal details to the guest.

    Attributes:
        provider: Integration provider id (often the plugin id).
        externalUserId: Provider-scoped user id.
        displayName: Display name for UI. Omitted when absent (wire zero value).
        accessToken: Ephemeral remote token (e.g. ABS JWT). Guest-to-host only; never
            persisted. Omitted when absent (wire zero value).
    """

    provider: str
    externalUserId: str
    displayName: NotRequired[str]
    accessToken: NotRequired[str]


class ExternalUserReplyOk(TypedDict):
    """``ExternalUserReply`` member ``ok``.

    Success: verified external user.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: verified external user.
    """

    kind: Literal["ok"]
    value: ExternalUser


class ExternalUserReplyErr(TypedDict):
    """``ExternalUserReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


ExternalUserReply = Union[
    ExternalUserReplyOk,
    ExternalUserReplyErr,
]
"""Result union of `Oidc.authenticateUser`."""


class EventPollResult(TypedDict):
    """Success payload of `RemoteLibrary.pollEvents`: signals for the host to kick
    off workflows.

    Attributes:
        users: Newly observed external users since the last poll.
    """

    users: list[ExternalUser]


class EventPollReplyOk(TypedDict):
    """``EventPollReply`` member ``ok``.

    Success: observed users.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: observed users.
    """

    kind: Literal["ok"]
    value: EventPollResult


class EventPollReplyErr(TypedDict):
    """``EventPollReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


EventPollReply = Union[
    EventPollReplyOk,
    EventPollReplyErr,
]
"""Result union of `RemoteLibrary.pollEvents`."""


class ListeningProgress(TypedDict):
    """One listening-progress row. The host upserts into `listening_progress`
    tagged with the plugin id; plugins never open the library DB.

    Attributes:
        externalUserId: Provider-scoped user id.
        externalItemId: Provider-scoped item / library id.
        identityId: Bookclerk identity row id when already linked. Omitted when absent
            (wire zero value).
        title: Title text when known. Omitted when absent (wire zero value).
        authors: Authors string when known. Omitted when absent (wire zero value).
        asin: Amazon ASIN when known. Omitted when absent (wire zero value).
        isbn: ISBN when known. Omitted when absent (wire zero value).
        progress: Fractional progress in `0.0..=1.0` when the provider reports it.
            Omitted when absent (wire zero value).
        currentTimeSeconds: Current playback position in seconds. Omitted when absent
            (wire zero value).
        durationSeconds: Total duration in seconds when known. Omitted when absent (wire
            zero value).
        isFinished: When true, the provider marks the item finished.
        lastListenedAtUnixMs: Last listen timestamp as unix milliseconds (UTC). Omitted
            when absent (wire zero value).
    """

    externalUserId: str
    externalItemId: str
    identityId: NotRequired[int]
    title: NotRequired[str]
    authors: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    progress: NotRequired[float]
    currentTimeSeconds: NotRequired[float]
    durationSeconds: NotRequired[float]
    isFinished: bool
    lastListenedAtUnixMs: NotRequired[int]


class SyncListeningResult(TypedDict):
    """Success payload of `RemoteLibrary.syncListening`.

    Attributes:
        items: Progress snapshots to upsert.
    """

    items: list[ListeningProgress]


class SyncListeningReplyOk(TypedDict):
    """``SyncListeningReply`` member ``ok``.

    Success: progress snapshots.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: progress snapshots.
    """

    kind: Literal["ok"]
    value: SyncListeningResult


class SyncListeningReplyErr(TypedDict):
    """``SyncListeningReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


SyncListeningReply = Union[
    SyncListeningReplyOk,
    SyncListeningReplyErr,
]
"""Result union of `RemoteLibrary.syncListening`."""


class PluginEvent(TypedDict):
    """Guest-published domain event (`EventPublisher.publish`). The host stamps
    `eventId`, `source` (the plugin id), and `accountId` from the invocation;
    guests cannot forge either.

    Attributes:
        eventType: Snake_case event type; must be listed in `[[events.producers]]`.
        schemaVersion: Schema version of `payload`, owned by the event type.
        deduplicationKey: Producer idempotency key, unique per (account, source,
            eventType) in the outbox; a repeat returns `PublishOk.duplicate = true` with
            the earlier id. Empty publishes unconditionally.
        payload: Encoded event payload; at most `maxEventPayloadBytes`.
        occurredAtUnixMs: When the producer observed the fact; zero means "now" on the
            host clock.
        correlationId: Trace correlation id; empty inherits `Invocation.correlationId`.
        causationId: Id of the event or command that caused this one; empty inherits
            `Invocation.causationId`.
    """

    eventType: str
    schemaVersion: int
    deduplicationKey: str
    payload: bytes
    occurredAtUnixMs: int
    correlationId: str
    causationId: str


class PublishOk(TypedDict):
    """Success payload of `EventPublisher.publish`.

    Attributes:
        eventId: Outbox event id (new or the earlier row when `duplicate`).
        duplicate: True when `deduplicationKey` matched an existing outbox row.
    """

    eventId: str
    duplicate: bool


class PublishReplyOk(TypedDict):
    """``PublishReply`` member ``ok``.

    Success: outbox row identity.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: outbox row identity.
    """

    kind: Literal["ok"]
    value: PublishOk


class PublishReplyErr(TypedDict):
    """``PublishReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


PublishReply = Union[
    PublishReplyOk,
    PublishReplyErr,
]
"""Result union of `EventPublisher.publish`."""


class Invocation(TypedDict):
    """Identity of one `PluginWorker.open` invocation. The host issues ids; guests
    echo `correlationId` / `causationId` onto published events.

    Attributes:
        id: Unique host-issued invocation id.
        accountId: Account scope; empty for operator / host-wide invocations.
        deadlineUnixMs: UTC Unix milliseconds; zero when the invocation has no deadline.
            The host fence is authoritative.
        correlationId: Trace correlation id; empty when none.
        causationId: Id of the event or command that caused this invocation; empty when
            none.
    """

    id: str
    accountId: str
    deadlineUnixMs: int
    correlationId: str
    causationId: str


class DbValueNull(TypedDict):
    """``DbValue`` member ``null``.

    SQL NULL with its declared type.

    Attributes:
        kind: Always ``"null"``.
        value: SQL NULL with its declared type.
    """

    kind: Literal["null"]
    value: DbType


class DbValueBoolean(TypedDict):
    """``DbValue`` member ``boolean``.

    Boolean.

    Attributes:
        kind: Always ``"boolean"``.
        value: Boolean.
    """

    kind: Literal["boolean"]
    value: bool


class DbValueInt64(TypedDict):
    """``DbValue`` member ``int64``.

    Signed 64-bit integer.

    Attributes:
        kind: Always ``"int64"``.
        value: Signed 64-bit integer.
    """

    kind: Literal["int64"]
    value: int


class DbValueFloat64(TypedDict):
    """``DbValue`` member ``float64``.

    IEEE-754 double.

    Attributes:
        kind: Always ``"float64"``.
        value: IEEE-754 double.
    """

    kind: Literal["float64"]
    value: float


class DbValueText(TypedDict):
    """``DbValue`` member ``text``.

    UTF-8 text.

    Attributes:
        kind: Always ``"text"``.
        value: UTF-8 text.
    """

    kind: Literal["text"]
    value: str


class DbValueBytes(TypedDict):
    """``DbValue`` member ``bytes``.

    Raw bytes.

    Attributes:
        kind: Always ``"bytes"``.
        value: Raw bytes.
    """

    kind: Literal["bytes"]
    value: bytes


DbValue = Union[
    DbValueNull,
    DbValueBoolean,
    DbValueInt64,
    DbValueFloat64,
    DbValueText,
    DbValueBytes,
]
"""One typed SQL cell or bind parameter."""


class DbColumn(TypedDict):
    """Result column descriptor.

    Attributes:
        name: Column name as projected.
        dbType: Declared or inferred column type.
    """

    name: str
    dbType: DbType


class DbRow(TypedDict):
    """One result row.

    Attributes:
        values: Cells in `columns` order.
    """

    values: list[DbValue]


class DbStatement(TypedDict):
    """One guest statement in an `ExecuteRequest`.

    Attributes:
        sql: BookclerkSQL text; at most `maxScalarBytes`.
        parameters: Positional bind values.
        kind: Statement classification.
        maxRows: Row cap for queries; `0` means adapter default.
        resultSelection: Which outcome parts to return.
    """

    sql: str
    parameters: list[DbValue]
    kind: DbStatementKind
    maxRows: int
    resultSelection: DbResultSelection


class ExecuteRequest(TypedDict):
    """Guest statement batch for `GuestDatabase.execute`; runs atomically.

    Attributes:
        operationId: Caller-chosen idempotency key.
        requestHash: SHA-256 hex of the idempotency-relevant request; empty when
            omitted.
        statements: Statements in execution order; at most `maxStatements`.
        deadlineUnixMs: Deadline hint; `0` means none.
    """

    operationId: str
    requestHash: str
    statements: list[DbStatement]
    deadlineUnixMs: int


class SqlSpan(TypedDict):
    """Byte span in the exact canonical SQL string a proof is bound to.

    Attributes:
        start: Inclusive start byte offset.
        end: Exclusive end byte offset.
    """

    start: int
    end: int


class TextCollateSite(TypedDict):
    """One TEXT expression the adapter must collate bytewise (`COLLATE "C"`).

    Attributes:
        span: Identifier or string-literal span in canonical SQL.
    """

    span: SqlSpan


class IntegerArithSite(TypedDict):
    """One INTEGER arithmetic expression that must not wrap or error on overflow.

    Attributes:
        full: Full expression span (`a + b` or `abs(n)`).
        lhs: Left operand (or `abs` argument).
        rhs: Right operand (`abs` repeats `lhs`).
        kind: Operator.
    """

    full: SqlSpan
    lhs: SqlSpan
    rhs: SqlSpan
    kind: IntegerArithKind


class PhysicalAccess(TypedDict):
    """Physical table/column access used for authorization.

    Attributes:
        table: Physical table name.
        column: Empty = table presence only; "*" = projection wildcard.
    """

    table: str
    column: str


class ResolvedAssignment(TypedDict):
    """Destination assignment `lhs = rhs` (INSERT, UPDATE, ...).

    Attributes:
        table: Target table.
        column: Target column.
        dest: Declared column type.
        source: Resolved type of the assigned expression.
    """

    table: str
    column: str
    dest: ResolvedSqlType
    source: ResolvedSqlType


class NamedSqlType(TypedDict):
    """Column name paired with its resolved type.

    Attributes:
        name: Column name.
        sqlType: Resolved type.
    """

    name: str
    sqlType: ResolvedSqlType


class ColumnReference(TypedDict):
    """`REFERENCES` target of one column.

    Attributes:
        refTable: Referenced table.
        refColumns: Referenced columns.
    """

    refTable: str
    refColumns: list[str]


class OptionalColumnReferenceNone(TypedDict):
    """``OptionalColumnReference`` member ``none``.

    Column has no reference.

    Attributes:
        kind: Always ``"none"``.
    """

    kind: Literal["none"]


class OptionalColumnReferenceSome(TypedDict):
    """``OptionalColumnReference`` member ``some``.

    Column references another table.

    Attributes:
        kind: Always ``"some"``.
        value: Column references another table.
    """

    kind: Literal["some"]
    value: ColumnReference


OptionalColumnReference = Union[
    OptionalColumnReferenceNone,
    OptionalColumnReferenceSome,
]
"""Per-column `REFERENCES` slot in `CreateTableSchema`."""


class ForeignKeyConstraint(TypedDict):
    """Table-level `FOREIGN KEY` constraint.

    Attributes:
        columns: Local columns.
        refTable: Referenced table.
        refColumns: Referenced columns.
    """

    columns: list[str]
    refTable: str
    refColumns: list[str]


class TableConstraintPrimaryKey(TypedDict):
    """``TableConstraint`` member ``primaryKey``.

    `PRIMARY KEY (...)` columns.

    Attributes:
        kind: Always ``"primaryKey"``.
        value: `PRIMARY KEY (...)` columns.
    """

    kind: Literal["primaryKey"]
    value: list[str]


class TableConstraintUnique(TypedDict):
    """``TableConstraint`` member ``unique``.

    `UNIQUE (...)` columns.

    Attributes:
        kind: Always ``"unique"``.
        value: `UNIQUE (...)` columns.
    """

    kind: Literal["unique"]
    value: list[str]


class TableConstraintCheck(TypedDict):
    """``TableConstraint`` member ``check``.

    `CHECK (...)` expression text.

    Attributes:
        kind: Always ``"check"``.
        value: `CHECK (...)` expression text.
    """

    kind: Literal["check"]
    value: str


class TableConstraintForeignKey(TypedDict):
    """``TableConstraint`` member ``foreignKey``.

    `FOREIGN KEY (...) REFERENCES ...`.

    Attributes:
        kind: Always ``"foreignKey"``.
        value: `FOREIGN KEY (...) REFERENCES ...`.
    """

    kind: Literal["foreignKey"]
    value: ForeignKeyConstraint


TableConstraint = Union[
    TableConstraintPrimaryKey,
    TableConstraintUnique,
    TableConstraintCheck,
    TableConstraintForeignKey,
]
"""One table-level constraint."""


class CreateTableSchema(TypedDict):
    """Parsed `CREATE TABLE` (canonical SQL v1). Per-column lists align with `columns`.

    Attributes:
        table: Table name.
        columns: Columns with resolved types, in order.
        identityColumn: `INTEGER PRIMARY KEY AUTOINCREMENT` column; empty when none.
        columnNotNull: Per-column NOT NULL flags.
        columnUnique: Per-column UNIQUE flags.
        columnPrimaryKey: Per-column PRIMARY KEY flags.
        columnDefaults: Per-column DEFAULT expression text; empty when none.
        columnChecks: Per-column CHECK expression text; empty when none.
        columnReferences: Per-column REFERENCES targets.
        tableConstraints: Table-level constraints, in order.
    """

    table: str
    columns: list[NamedSqlType]
    identityColumn: str
    columnNotNull: list[bool]
    columnUnique: list[bool]
    columnPrimaryKey: list[bool]
    columnDefaults: list[str]
    columnChecks: list[str]
    columnReferences: list[OptionalColumnReference]
    tableConstraints: list[TableConstraint]


class SchemaCreate(TypedDict):
    """`CREATE TABLE` action with its durable fingerprint.

    Attributes:
        schema: Parsed table schema.
        fingerprint: Structured schema fingerprint (hex SHA-256).
        noop: True when the catalog already holds this exact fingerprint.
    """

    schema: CreateTableSchema
    fingerprint: str
    noop: bool


class SchemaActionNone(TypedDict):
    """``SchemaAction`` member ``none``.

    Not DDL.

    Attributes:
        kind: Always ``"none"``.
    """

    kind: Literal["none"]


class SchemaActionCreate(TypedDict):
    """``SchemaAction`` member ``create``.

    `CREATE TABLE`.

    Attributes:
        kind: Always ``"create"``.
        value: `CREATE TABLE`.
    """

    kind: Literal["create"]
    value: SchemaCreate


class SchemaActionDrop(TypedDict):
    """``SchemaAction`` member ``drop``.

    `DROP TABLE` of the named table.

    Attributes:
        kind: Always ``"drop"``.
        value: `DROP TABLE` of the named table.
    """

    kind: Literal["drop"]
    value: str


SchemaAction = Union[
    SchemaActionNone,
    SchemaActionCreate,
    SchemaActionDrop,
]
"""CREATE/DROP action recorded on a proof."""


class ResolvedStatement(TypedDict):
    """Host-produced typed proof bound to one exact canonical statement.

    Attributes:
        statementHash: SHA-256 hex of the exact canonical SQL this proof claims.
        outputColumns: SELECT / RETURNING / VALUES output columns in order.
        physicalAccesses: Physical tables/columns referenced (authorization).
        assignments: Mutation destination assignments.
        textCollateSites: TEXT expression spans needing bytewise collation.
        integerArithSites: INTEGER overflow sites (`+` `-` `*` `abs`).
        functions: Function names invoked (folded), for authorization.
        schemaAction: DDL action + fingerprint.
    """

    statementHash: str
    outputColumns: list[NamedSqlType]
    physicalAccesses: list[PhysicalAccess]
    assignments: list[ResolvedAssignment]
    textCollateSites: list[TextCollateSite]
    integerArithSites: list[IntegerArithSite]
    functions: list[str]
    schemaAction: SchemaAction


class AdapterReceipt(TypedDict):
    """Replay receipt the host asks the adapter to persist with the batch.

    Attributes:
        guestLen: Guest statement count inside the receipt wrap (excluding host
            prune/select).
        guestHash: Guest `requestHash` compared on replay.
    """

    guestLen: int
    guestHash: str


class AdapterStatement(TypedDict):
    """One host-lowered statement with its proof.

    Attributes:
        sql: Canonical SQL text.
        parameters: Positional bind values.
        kind: Statement classification.
        maxRows: Row cap for queries; `0` means adapter default.
        resultSelection: Which outcome parts to return.
        proof: Typed proof bound to `sql`.
    """

    sql: str
    parameters: list[DbValue]
    kind: DbStatementKind
    maxRows: int
    resultSelection: DbResultSelection
    proof: ResolvedStatement


class AdapterExecuteRequest(TypedDict):
    """Host -> adapter batch: proven statements plus isolation and receipt.

    Attributes:
        operationId: Host-chosen idempotency key.
        requestHash: SHA-256 hex of the idempotency-relevant request.
        statements: Statements in execution order.
        deadlineUnixMs: Deadline hint; `0` means none.
        isolation: Transaction isolation the adapter must realize.
        receipt: Replay receipt to persist; zero/empty when not required.
    """

    operationId: str
    requestHash: str
    statements: list[AdapterStatement]
    deadlineUnixMs: int
    isolation: IsolationReq
    receipt: AdapterReceipt


class StatementResult(TypedDict):
    """Outcome of one statement.

    Attributes:
        rows: Result rows (empty unless `rows` selected).
        columns: Result column descriptors.
        rowsAffected: Rows changed by a mutation.
    """

    rows: list[DbRow]
    columns: list[DbColumn]
    rowsAffected: int


class DbTiming(TypedDict):
    """Engine timing on `ExecuteReply`.

    Attributes:
        attemptElapsedUs: Monotonic duration of this handler attempt (microseconds).
        dbExecutionUs: Engine-reported SQL/transaction time when available (`0` =
            omitted).
        dbTimingSource: How `dbExecutionUs` was measured.
    """

    attemptElapsedUs: int
    dbExecutionUs: int
    dbTimingSource: str


class ExecuteReply(TypedDict):
    """Success payload of `execute`.

    Attributes:
        operationId: Echo of the request `operationId`.
        statements: Per-statement results in request order.
        timing: Engine timing.
    """

    operationId: str
    statements: list[StatementResult]
    timing: DbTiming


class ExecuteResultReplyOk(TypedDict):
    """``ExecuteResultReply`` member ``ok``.

    Success: statement results.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: statement results.
    """

    kind: Literal["ok"]
    value: ExecuteReply


class ExecuteResultReplyErr(TypedDict):
    """``ExecuteResultReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


ExecuteResultReply = Union[
    ExecuteResultReplyOk,
    ExecuteResultReplyErr,
]
"""Result union of `execute`."""


class DbCapabilities(TypedDict):
    """Semantic SQL-contract advertisement. Diagnostic engine identity is not
    part of the capability plane — see `DbBootstrap`.

    Attributes:
        sqlContractVersion: Bookclerk SQL contract version.
        atomicBatch: Guest can run a bounded statement list as one SQL transaction.
        returning: Guest SQL supports `RETURNING`.
        affectedRows: Guest reports `rowsAffected`.
        schemaMigrations: Guest versions schema with a `bookclerk_schema_migrations`
            table.
        cancellation: Guest honors RPC/session cancellation.
        timing: Guest can fill `DbTiming.dbExecutionUs`. Not a host connect minimum.
        maxBinds: Maximum bound parameters per statement.
        maxStatements: Maximum statements in one atomic batch.
        maxResultRows: Maximum rows a query statement may return.
        maxPayloadBytes: Maximum UTF-8 bytes of SQL plus binds per statement.
        maxResultBytes: Maximum encoded bytes of one statement's result rows.
        maxCellBytes: Maximum UTF-8 / blob bytes of one result cell.
        maxRequestBytes: Maximum encoded bytes of one `ExecuteRequest`.
        maxAtomicResultBytes: Maximum encoded bytes of one `ExecuteReply`.
        pluginDatabases: Append-only. Adapter can open additional isolated sessions for
            plugin-owned database bindings (per-binding file / schema / database).
        maxFunctionArgs: Maximum arguments in one physical function call after adapter
            hiding (nested min / max / coalesce). Portable json_object is already ≤ 32
            arguments (16 pairs), matching D1. `0` is unspecified.
        maxSchemaColumns: Maximum columns in one CREATE TABLE / result row. `0` is
            unspecified.
        maxPatternBytes: Maximum UTF-8 bytes of a BookclerkSQL LIKE pattern value
            (literals and TEXT binds). Adapters that expand LIKE into GLOB must
            advertise a conservative value that still fits the physical pattern cap. `0`
            is unspecified.
        maxLoweredStatementBytes: Maximum UTF-8 bytes of sqlite-family lowered SQL the
            adapter can realize for one statement (INTEGER overflow wraps, LIKE→GLOB,
            NULLIF, INSERT OR IGNORE, query LIMIT wrap, bytes-placeholder expansion).
            `0` is unspecified: the host does not enforce a lowered-size ceiling. First-
            party D1 advertises 100000. Hosts compare a standardized Bookclerk lowering
            upper bound against this number and must not branch on engine identity.
        consistentBackupRead: Adapter can expose one stable logical database state while
            the host reads schema, rows, and identity.
        atomicUnitRestore: Adapter can destructively replace one logical database unit
            so an ordinary restore failure does not leave that unit partially replaced.
    """

    sqlContractVersion: int
    atomicBatch: bool
    returning: bool
    affectedRows: bool
    schemaMigrations: bool
    cancellation: bool
    timing: bool
    maxBinds: int
    maxStatements: int
    maxResultRows: int
    maxPayloadBytes: int
    maxResultBytes: int
    maxCellBytes: int
    maxRequestBytes: int
    maxAtomicResultBytes: int
    pluginDatabases: bool
    maxFunctionArgs: int
    maxSchemaColumns: int
    maxPatternBytes: int
    maxLoweredStatementBytes: int
    consistentBackupRead: bool
    atomicUnitRestore: bool


class DbBootstrapReplyOk(TypedDict):
    """``DbBootstrapReply`` member ``ok``.

    Success: bootstrap metadata.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: bootstrap metadata.
    """

    kind: Literal["ok"]
    value: DbBootstrap


class DbBootstrapReplyErr(TypedDict):
    """``DbBootstrapReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


DbBootstrapReply = Union[
    DbBootstrapReplyOk,
    DbBootstrapReplyErr,
]
"""Result union of `AdapterDatabaseSession.bootstrap`."""


class DbBootstrap(TypedDict):
    """Bootstrap-only diagnostic metadata (not a capability).

    Attributes:
        engine: Diagnostic physical engine name. Hosts must not admit or generate SQL
            from this value. Any string is valid.
    """

    engine: str


class DbCapabilitiesReplyOk(TypedDict):
    """``DbCapabilitiesReply`` member ``ok``.

    Success: capability advertisement.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: capability advertisement.
    """

    kind: Literal["ok"]
    value: DbCapabilities


class DbCapabilitiesReplyErr(TypedDict):
    """``DbCapabilitiesReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


DbCapabilitiesReply = Union[
    DbCapabilitiesReplyOk,
    DbCapabilitiesReplyErr,
]
"""Result union of `AdapterDatabaseSession.capabilities`."""


class Database(Protocol):
    """`databaseAdapter` entrypoint (`Entrypoints.databaseAdapter`)."""

    async def open_session(self) -> AdapterSessionReply:
        """Open one adapter session (capability negotiation + typed execute).

        Returns:
            ``AdapterSessionReply``
        """
        ...


class IdentityHighWater(TypedDict):
    """Adapter-private identity high-water (sqlite_sequence / bookclerk_identity).
    Column names live in the canonical backup schema, not this catalog.

    Attributes:
        table: Table whose identity column the mark belongs to.
        last: Highest generated or stored value that must not be reused.
    """

    table: str
    last: int


class IdentityExportReplyOk(TypedDict):
    """``IdentityExportReply`` member ``ok``.

    Success: identity high-water rows.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: identity high-water rows.
    """

    kind: Literal["ok"]
    value: list[IdentityHighWater]


class IdentityExportReplyErr(TypedDict):
    """``IdentityExportReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


IdentityExportReply = Union[
    IdentityExportReplyOk,
    IdentityExportReplyErr,
]
"""Result union of `AdapterDatabaseSession.exportIdentity`."""


class UserRelationsReplyOk(TypedDict):
    """``UserRelationsReply`` member ``ok``.

    Success: user relation names.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: user relation names.
    """

    kind: Literal["ok"]
    value: list[str]


class UserRelationsReplyErr(TypedDict):
    """``UserRelationsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


UserRelationsReply = Union[
    UserRelationsReplyOk,
    UserRelationsReplyErr,
]
"""Result union of `AdapterDatabaseSession.listUserRelations`."""


class AdapterDatabaseSession(Protocol):
    """Host ↔ database adapter plugin. Capability negotiation + typed execute only."""

    async def capabilities(self) -> DbCapabilitiesReply:
        """Semantic SQL-contract advertisement for this session.

        Returns:
            ``DbCapabilitiesReply``
        """
        ...

    async def execute(self, request: AdapterExecuteRequest) -> ExecuteResultReply:
        """Canonical SQL + required structured proofs (not JSON).

        Args:
            request: Proven statements plus isolation and receipt.

        Returns:
            ``ExecuteResultReply``
        """
        ...

    async def close(self) -> EmptyReply:
        """Release the session; further calls fail.

        Returns:
            ``EmptyReply``
        """
        ...

    async def bootstrap(self) -> DbBootstrapReply:
        """Bootstrap-only SeaORM proxy metadata (not part of DbCapabilities).

        Returns:
            ``DbBootstrapReply``
        """
        ...

    async def export_identity(self) -> IdentityExportReply:
        """Snapshot/identity/restore primitives (not a SQL dialect API).

        Returns:
            ``IdentityExportReply``
        """
        ...

    async def import_identity(self, rows: list[IdentityHighWater]) -> EmptyReply:
        """Restore identity high-water marks captured by `exportIdentity`.

        Args:
            rows: High-water rows to apply.

        Returns:
            ``EmptyReply``
        """
        ...

    async def list_user_relations(self) -> UserRelationsReply:
        """Names of user relations present in the logical database unit.

        Returns:
            ``UserRelationsReply``
        """
        ...

    async def prepare_unit_restore(self) -> EmptyReply:
        """Enter restore mode for one logical unit (`atomicUnitRestore`).

        Returns:
            ``EmptyReply``
        """
        ...

    async def drop_user_relations(self, names: list[str]) -> EmptyReply:
        """Drop the named user relations during restore.

        Args:
            names: Relation names from `listUserRelations`.

        Returns:
            ``EmptyReply``
        """
        ...

    async def assert_restore_constraints(self) -> EmptyReply:
        """Verify constraints hold after restore rows were written.

        Returns:
            ``EmptyReply``
        """
        ...


class GuestDatabase(Protocol):
    """Host-granted SQL for job plugin authors. SDK `DatabaseBinding` mirrors the
    Cloudflare Workers D1 surface (`prepare`/`bind`/`run`/`first`/`all`/`raw`,
    `batch`, `exec`) over this typed `execute` transport; wire types stay Cap'n
    `ExecuteRequest`/`ExecuteReply`.
    """

    async def execute(self, request: ExecuteRequest) -> ExecuteResultReply:
        """Run one typed statement batch.

        Args:
            request: Statements, binds, and deadline.

        Returns:
            ``ExecuteResultReply``
        """
        ...

    async def close(self) -> EmptyReply:
        """Release the session; further calls fail.

        Returns:
            ``EmptyReply``
        """
        ...


class PluginMigrationOpSchema(TypedDict):
    """``PluginMigrationOp`` member ``schema``.

    Admitted DDL statement text.

    Attributes:
        kind: Always ``"schema"``.
        value: Admitted DDL statement text.
    """

    kind: Literal["schema"]
    value: str


class PluginMigrationOpData(TypedDict):
    """``PluginMigrationOp`` member ``data``.

    Admitted DML statement text.

    Attributes:
        kind: Always ``"data"``.
        value: Admitted DML statement text.
    """

    kind: Literal["data"]
    value: str


PluginMigrationOp = Union[
    PluginMigrationOpSchema,
    PluginMigrationOpData,
]
"""One already-separated BookclerkSQL operation in a plugin-owned migration.
`schema` is admitted DDL; `data` is admitted DML. There is no native SQL
escape hatch.
"""


class PluginMigration(TypedDict):
    """One plugin-owned migration application. `id` is an opaque plugin-chosen
    stable identity (name, UUID, timestamp-like string, or digits-as-text).
    Bookclerk assigns no order, version, or predecessor meaning to `id`.
    Registration order is the forward sequence.

    Attributes:
        id: Opaque plugin-chosen stable identity.
        operations: Operations in forward order; at most `maxPluginMigrationOps`.
    """

    id: str
    operations: list[PluginMigrationOp]


class PluginMigrationsOk(TypedDict):
    """Success payload of `PluginWorker.databaseMigrations`.

    Attributes:
        migrations: At most `maxListPage` entries; aggregate id+SQL bytes at most
            `maxPluginMigrationRegistrationBytes`; total operations at most
            `maxPluginMigrationTotalOps`.
    """

    migrations: list[PluginMigration]


class PluginMigrationsReplyOk(TypedDict):
    """``PluginMigrationsReply`` member ``ok``.

    Success: ordered migration sequence.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: ordered migration sequence.
    """

    kind: Literal["ok"]
    value: PluginMigrationsOk


class PluginMigrationsReplyErr(TypedDict):
    """``PluginMigrationsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


PluginMigrationsReply = Union[
    PluginMigrationsReplyOk,
    PluginMigrationsReplyErr,
]
"""Result union of `PluginWorker.databaseMigrations`."""


class PluginWorker(Protocol):
    """Plugin bootstrap capability: the guest's root object (Workers `default`
    export analogue). `describe()` first; `open()` once per invocation.
    """

    async def describe(self) -> DescribeReply:
        """Identity, ABI version, negotiated features, and typed capabilities.

        Returns:
            ``DescribeReply``
        """
        ...

    async def open(self, invocation: Invocation, bindings: Bindings) -> EntrypointsReply:
        """Open the exported entrypoints for one invocation with host-granted
        bindings. Entrypoints the manifest does not export are null.

        Args:
            invocation: Host-issued invocation identity.
            bindings: Granted `env` bindings.

        Returns:
            ``EntrypointsReply``
        """
        ...

    async def shutdown(self) -> EmptyReply:
        """Flush and release resources before the process exits.

        Returns:
            ``EmptyReply``
        """
        ...

    async def database_migrations(self, binding: str) -> PluginMigrationsReply:
        """Complete ordered plugin-owned migration sequence for one named binding.
        The host calls this before `open` while provisioning `[[databases]]`
        (a binding must be migrated before any `Bindings.databases` session is
        handed out). Empty list means the binding has no plugin-owned migrations.
        Bounded by `maxListPage` / `maxPluginMigrationOps` /
        `maxPluginMigrationTotalOps` / `maxScalarBytes` /
        `maxPluginMigrationRegistrationBytes`.

        Args:
            binding: Binding name from `plugin.toml`.

        Returns:
            ``PluginMigrationsReply``
        """
        ...


__all__ = [
    "PluginErrorCode",
    "Entrypoint",
    "PortalAuthMode",
    "CliArgKind",
    "CatalogSort",
    "CatalogField",
    "Abridgement",
    "DbType",
    "DbStatementKind",
    "DbResultSelection",
    "IsolationReq",
    "ResolvedSqlType",
    "IntegerArithKind",
    "ScalarLimits",
    "PluginError",
    "ObjectMetadata",
    "ObjectInfo",
    "ListOptions",
    "ListPage",
    "ByteRange",
    "ReadOptions",
    "WriteOptions",
    "PutResult",
    "CopyResult",
    "PluginDescribe",
    "OidcClientTemplate",
    "OidcClientsOk",
    "OidcClientsReplyOk",
    "OidcClientsReplyErr",
    "OidcClientsReply",
    "ExtensibleConfig",
    "Bindings",
    "Entrypoints",
    "EntrypointsReplyOk",
    "EntrypointsReplyErr",
    "EntrypointsReply",
    "JobInvocation",
    "CompletedOutcome",
    "RetryableOutcome",
    "RejectedOutcome",
    "CancelledOutcome",
    "SuspendedOutcome",
    "JobOutcomeCompleted",
    "JobOutcomeRetryable",
    "JobOutcomeRejected",
    "JobOutcomeCancelled",
    "JobOutcomeSuspended",
    "JobOutcome",
    "DomainEvent",
    "EventAck",
    "EventRetry",
    "EventReject",
    "EventDeadLetter",
    "EventSuspended",
    "EventResultAck",
    "EventResultRetry",
    "EventResultReject",
    "EventResultDeadLetter",
    "EventResultSuspended",
    "EventResult",
    "EventBatch",
    "EventBatchReplyOk",
    "EventBatchReplyErr",
    "EventBatchReply",
    "JobController",
    "HeadOk",
    "HeadReplyOk",
    "HeadReplyErr",
    "HeadReply",
    "ListReplyOk",
    "ListReplyErr",
    "ListReply",
    "GetOk",
    "GetReplyOk",
    "GetReplyErr",
    "GetReply",
    "PutReplyOk",
    "PutReplyErr",
    "PutReply",
    "CopyReplyOk",
    "CopyReplyErr",
    "CopyReply",
    "EmptyReplyOk",
    "EmptyReplyErr",
    "EmptyReply",
    "PullOk",
    "PullReplyOk",
    "PullReplyErr",
    "PullReply",
    "OpenOk",
    "OpenReplyOk",
    "OpenReplyErr",
    "OpenReply",
    "DescribeReplyOk",
    "DescribeReplyErr",
    "DescribeReply",
    "HandleReplyOk",
    "HandleReplyErr",
    "HandleReply",
    "HealthOk",
    "HealthReplyOk",
    "HealthReplyErr",
    "HealthReply",
    "AdapterSessionReplyOk",
    "AdapterSessionReplyErr",
    "AdapterSessionReply",
    "GuestDatabaseReplyOk",
    "GuestDatabaseReplyErr",
    "GuestDatabaseReply",
    "ByteSource",
    "Destination",
    "Source",
    "ProgressSink",
    "Cancellation",
    "JobRunner",
    "EventConsumer",
    "EventPublisher",
    "NamedDatabase",
    "ContentSource",
    "RemoteLibrary",
    "PluginCli",
    "Oidc",
    "EventConsumerSpec",
    "PluginCapabilities",
    "Brand",
    "ConfigOption",
    "ConfigOptionValue",
    "CliSchema",
    "CliCommandSpec",
    "CliArgSpec",
    "CliArg",
    "CliInvokeParams",
    "CliInvokeResult",
    "CliSchemaReplyOk",
    "CliSchemaReplyErr",
    "CliSchemaReply",
    "CliInvokeReplyOk",
    "CliInvokeReplyErr",
    "CliInvokeReply",
    "DatabaseAdapterConfig",
    "DiagnoseResult",
    "DiagnoseReplyOk",
    "DiagnoseReplyErr",
    "DiagnoseReply",
    "SourceAccount",
    "LoginParams",
    "LoginResult",
    "LoginStartResult",
    "LoginCompleteParams",
    "LoginReplyOk",
    "LoginReplyErr",
    "LoginReply",
    "LoginStartReplyOk",
    "LoginStartReplyErr",
    "LoginStartReply",
    "AccountCredential",
    "ScanParams",
    "ScanBook",
    "ScanSummary",
    "ScanReplyOk",
    "ScanReplyErr",
    "ScanReply",
    "FetchOptions",
    "FetchTitleParams",
    "PlainPart",
    "ChapterMarker",
    "PlainFetch",
    "FetchTitleReplyOk",
    "FetchTitleReplyErr",
    "FetchTitleReply",
    "SourceAccounts",
    "SourceAccountsReplyOk",
    "SourceAccountsReplyErr",
    "SourceAccountsReply",
    "SearchCatalogParams",
    "ExpandCandidatesParams",
    "PurchaseHintParams",
    "ListDealsParams",
    "CatalogDetailParams",
    "CatalogHit",
    "CatalogHits",
    "CatalogHitsReplyOk",
    "CatalogHitsReplyErr",
    "CatalogHitsReply",
    "CatalogDetail",
    "CatalogDetailReplyOk",
    "CatalogDetailReplyErr",
    "CatalogDetailReply",
    "PurchaseHint",
    "PurchaseHintResult",
    "PurchaseHintReplyOk",
    "PurchaseHintReplyErr",
    "PurchaseHintReply",
    "ScanLibraryParams",
    "AuthenticateUserParams",
    "ExternalUser",
    "ExternalUserReplyOk",
    "ExternalUserReplyErr",
    "ExternalUserReply",
    "EventPollResult",
    "EventPollReplyOk",
    "EventPollReplyErr",
    "EventPollReply",
    "ListeningProgress",
    "SyncListeningResult",
    "SyncListeningReplyOk",
    "SyncListeningReplyErr",
    "SyncListeningReply",
    "PluginEvent",
    "PublishOk",
    "PublishReplyOk",
    "PublishReplyErr",
    "PublishReply",
    "Invocation",
    "DbValueNull",
    "DbValueBoolean",
    "DbValueInt64",
    "DbValueFloat64",
    "DbValueText",
    "DbValueBytes",
    "DbValue",
    "DbColumn",
    "DbRow",
    "DbStatement",
    "ExecuteRequest",
    "SqlSpan",
    "TextCollateSite",
    "IntegerArithSite",
    "PhysicalAccess",
    "ResolvedAssignment",
    "NamedSqlType",
    "ColumnReference",
    "OptionalColumnReferenceNone",
    "OptionalColumnReferenceSome",
    "OptionalColumnReference",
    "ForeignKeyConstraint",
    "TableConstraintPrimaryKey",
    "TableConstraintUnique",
    "TableConstraintCheck",
    "TableConstraintForeignKey",
    "TableConstraint",
    "CreateTableSchema",
    "SchemaCreate",
    "SchemaActionNone",
    "SchemaActionCreate",
    "SchemaActionDrop",
    "SchemaAction",
    "ResolvedStatement",
    "AdapterReceipt",
    "AdapterStatement",
    "AdapterExecuteRequest",
    "StatementResult",
    "DbTiming",
    "ExecuteReply",
    "ExecuteResultReplyOk",
    "ExecuteResultReplyErr",
    "ExecuteResultReply",
    "DbCapabilities",
    "DbBootstrapReplyOk",
    "DbBootstrapReplyErr",
    "DbBootstrapReply",
    "DbBootstrap",
    "DbCapabilitiesReplyOk",
    "DbCapabilitiesReplyErr",
    "DbCapabilitiesReply",
    "Database",
    "IdentityHighWater",
    "IdentityExportReplyOk",
    "IdentityExportReplyErr",
    "IdentityExportReply",
    "UserRelationsReplyOk",
    "UserRelationsReplyErr",
    "UserRelationsReply",
    "AdapterDatabaseSession",
    "GuestDatabase",
    "PluginMigrationOpSchema",
    "PluginMigrationOpData",
    "PluginMigrationOp",
    "PluginMigration",
    "PluginMigrationsOk",
    "PluginMigrationsReplyOk",
    "PluginMigrationsReplyErr",
    "PluginMigrationsReply",
    "PluginWorker",
]
