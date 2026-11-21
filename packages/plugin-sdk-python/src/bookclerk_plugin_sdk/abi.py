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

ObjectChangeKind = Literal["changed", "upserted", "removed"]
"""What happened to an object in a change feed. Unknown codes are kept and
treated as `changed`.
"""

SettingNetworkKind = Literal["fetch", "tcp"]
"""How a setting's destination is reached."""

SettingGrantState = Literal["approved", "pending", "unset", "invalid"]
"""Where a network setting's destination stands."""

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
    "limit_exceeded",
    "rate_limited",
    "network_not_granted",
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
    "metadata",
    "covers",
    "metadataFiles",
    "notifier",
]
"""Named entrypoint a plugin exports (Cloudflare Workers named-entrypoint
analogue). Each value is a capability the host calls over RPC; triggers on
the default entrypoint (`event`, `job`) are declared separately.
"""

PortalAuthMode = Literal["unspecified", "password", "oauth"]
"""Portal Accounts connect mode for storefronts."""

CliArgKind = Literal["string", "bool", "int", "path"]
"""Value kind for a `CliArgSpec`."""

WorkKind = Literal["unspecified", "book", "podcast"]
"""What family of creative work a record describes (FRBR work). Every record
names one: `unspecified` is refused by the host, so a guest that forgets
the field fails instead of silently describing a book. Whether a podcast
work is a show or an episode is structural, not a kind: an episode names
its show through `parentProductId`, a show names no parent.
"""

ExpressionKind = Literal[
    "unspecified",
    "text",
    "narration",
    "dramatisation",
    "graphic",
    "originalAudio",
]
"""How a work is realised (FRBR expression): the kind of content you get,
independent of the carrier. A translation is not a kind: the expression's
language and its translator credits say it. `unspecified` is refused.
"""

MediaKind = Literal["unspecified", "audio", "ebook", "print"]
"""Carrier of an edition (FRBR manifestation): what form you receive.
Whether the host acquires an edition depends on the storefront's
`MediaSupport.fetches` and what the host supports, not on this value
alone. `unspecified` is refused.
"""

IdentifierScheme = Literal["isbn", "asin", "openLibraryEdition", "openLibraryWork"]
"""Scheme of an external identifier. The scheme fixes whether the id names a
work or an edition, and whether it is strong (unique) or weak (shared).
"""

CreditRole = Literal[
    "author",
    "narrator",
    "translator",
    "editor",
    "illustrator",
    "publisher",
    "host",
    "cast",
    "adaptor",
]
"""A contributor's responsibility for a work, expression or edition."""

CreditLevel = Literal["roleDefault", "work", "expression", "edition"]
"""Which level a credit attaches to. Ordinal 0 is the role's usual level, so
a guest sets this only when a credit departs from it.
"""

FetchedFileRole = Literal["unspecified", "part", "whole", "cover"]
"""What one fetched file is to the edition it belongs to."""

CatalogSort = Literal["relevance", "popularity", "rating", "title", "author"]
"""Catalog search ordering."""

CatalogField = Literal["any", "author", "narrator", "series", "genre"]
"""Catalog search facet restricting which field the query matches."""

PriceLookup = Literal["unspecified", "product", "isbn", "asin", "title"]
"""How a purchase-hint query names the title to price."""

Abridgement = Literal["unknown", "unabridged", "abridged"]
"""Whether an edition is abridged."""

MetadataField = Literal[
    "title",
    "subtitle",
    "description",
    "publisher",
    "publishedAt",
    "language",
    "genres",
    "tags",
    "abridgement",
    "expressionKind",
    "identifiers",
    "credits",
    "series",
    "chapters",
    "duration",
]
"""A field of a `MetadataFileRecord`. Dialects declare in the manifest which
fields their format carries; a parsed record lists the ones its file
states, and a render request lists the ones to write.
"""

NotificationSeverity = Literal["info", "warning", "critical"]
"""How urgent a notification is. Routing rules carry a minimum severity."""

NotificationAudience = Literal["operational", "patron", "account"]
"""Who a notification type is for. The account audience is reserved to the
host; a plugin may declare only operational and patron types.
"""

NotificationAlertAction = Literal["none", "trigger", "resolve"]
"""Lifecycle step of an alert, for services such as PagerDuty that open and
close incidents.
"""

NotifyDestinationKind = Literal["channel", "personal"]
"""Kind of destination a `Notifier` sends to."""

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
        details: Structured extras as one JSON object (`limitExceeded` carries `limit`,
            `cap` and `used`; engine failures carry `engineCode`). Empty when none.
    """

    code: str
    message: str
    details: str


class ObjectMetadata(TypedDict):
    """Full metadata of one stored object.

    Attributes:
        key: Object key.
        size: Object size in bytes.
        contentType: MIME type; empty when unknown.
        etag: Backend entity tag; empty when unsupported.
        sha256: Raw SHA-256 digest (32 bytes) or empty when unknown.
        modifiedAtMs: Last modification time, UTC unix milliseconds; zero when unknown.
        versionId: Backend object version id (versioned buckets); empty when none.
        signature: Opaque change signature: an unchanged value means the object has not
            changed. Backend-defined (a local file's inode, size, mtime and ctime; an S3
            ETag and version id). Empty when the backend has none.
    """

    key: str
    size: int
    contentType: str
    etag: str
    sha256: bytes
    modifiedAtMs: int
    versionId: str
    signature: str


class ObjectInfo(TypedDict):
    """Compact object entry in a list page.

    Attributes:
        key: Object key.
        size: Object size in bytes.
        etag: Backend entity tag; empty when unsupported or not requested
            (`ListOptions.withSignatures`).
        modifiedAtMs: Last modification time, UTC unix milliseconds; zero when unknown
            or not requested.
        versionId: Backend object version id; empty when none or not requested.
        signature: Opaque change signature, as `ObjectMetadata.signature`; empty when
            unsupported or not requested.
    """

    key: str
    size: int
    etag: str
    modifiedAtMs: int
    versionId: str
    signature: str


class ListOptions(TypedDict):
    """Paging options for `Destination.list`.

    Attributes:
        prefix: Only keys starting with this prefix; empty lists everything.
        cursor: Opaque cursor from a previous `ListPage.nextCursor`; empty starts over.
        limit: Requested page size; clamped to `maxListPage`. `0` means guest default.
        withSignatures: Hint: fill `etag`, `modifiedAtMs`, `versionId` and `signature`
            on each entry. `ListPage.signaturesIncluded` says whether the guest did.
        modifiedSinceMs: Hint: only objects modified at or after this time (UTC unix
            milliseconds); zero lists everything. `ListPage.modifiedSinceApplied` says
            whether the guest filtered; when it did not, the page is unfiltered.
    """

    prefix: str
    cursor: str
    limit: int
    withSignatures: bool
    modifiedSinceMs: int


class ListPage(TypedDict):
    """One page of `Destination.list` results.

    Attributes:
        objects: Objects in this page, in backend order.
        nextCursor: Cursor for the next page; empty when exhausted.
        signaturesIncluded: True when every entry carries the signature fields the
            backend has (`ListOptions.withSignatures` honoured).
        modifiedSinceApplied: True when the guest left out objects modified before
            `ListOptions.modifiedSinceMs`. False means the page is unfiltered.
    """

    objects: list[ObjectInfo]
    nextCursor: str
    signaturesIncluded: bool
    modifiedSinceApplied: bool


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
        ifMatch: Conditional write (`storage.conditionalWrite`): publish only while the
            stored object's `etag` equals this value, else fail with `conflict`. Empty
            means unconditional. A staged put checks it at `commit`.
        ifNoneMatch: Conditional write (`storage.conditionalWrite`): publish only when
            no object exists at the key, else fail with `conflict`.
    """

    contentType: str
    contentLength: int
    sha256: bytes
    commitToken: str
    stageOnly: bool
    ifMatch: str
    ifNoneMatch: bool


class PutResult(TypedDict):
    """Summary of a stored (or committed) object.

    Attributes:
        key: Object key written.
        bytesWritten: Bytes persisted.
        etag: Backend entity tag; empty when unsupported.
        sha256: Raw SHA-256 digest of the stored bytes; empty when not computed.
        modifiedAtMs: Modification time of the stored object, UTC unix milliseconds;
            zero when unknown.
        versionId: Version id of the stored object; empty when none.
        signature: Change signature of the stored object (`ObjectMetadata.signature`),
            so the host can recognise its own write in a later listing; empty when none.
    """

    key: str
    bytesWritten: int
    etag: str
    sha256: bytes
    modifiedAtMs: int
    versionId: str
    signature: str


class CopyResult(TypedDict):
    """Summary of a server-side copy.

    Attributes:
        bytesCopied: Bytes copied.
    """

    bytesCopied: int


class MoveOptions(TypedDict):
    """Options for `Destination.move` and `Destination.moveMany`. Fields are only
    ever appended.

    Attributes:
        noOverwrite: Fail with `conflict` instead of replacing an object at the target
            key.
        ifSourceMatch: `move` only: move only while the source's `etag` equals this
            value, else fail with `conflict`. Empty skips the check.
    """

    noOverwrite: bool
    ifSourceMatch: str


class MoveResult(TypedDict):
    """Summary of `Destination.move`.

    Attributes:
        atomic: True when the backend moved atomically (a rename): no reader saw the
            object at both keys or at neither.
        alreadyMoved: True when the source was already gone and the target present, as
            when a move is repeated after a restart. The move counts as done.
    """

    atomic: bool
    alreadyMoved: bool


KeyPair = TypedDict(
    "KeyPair",
    {
        "from": str,
        "to": str,
    },
)
"""One source and target key.

Attributes:
    from: Source object key (or source prefix).
    to: Target object key (or target prefix).
"""


class MoveManyTargetPairs(TypedDict):
    """``MoveManyTarget`` member ``pairs``.

    Explicit moves, at most `maxBatchKeys`. Each is independent.

    Attributes:
        kind: Always ``"pairs"``.
        value: Explicit moves, at most `maxBatchKeys`. Each is independent.
    """

    kind: Literal["pairs"]
    value: list[KeyPair]


class MoveManyTargetPrefix(TypedDict):
    """``MoveManyTarget`` member ``prefix``.

    Every object under `prefix.from` moves under `prefix.to`, keeping the
    rest of its key. Paged through `MoveManyRequest.cursor`.

    Attributes:
        kind: Always ``"prefix"``.
        value: Every object under `prefix.from` moves under `prefix.to`, keeping the
            rest of its key. Paged through `MoveManyRequest.cursor`.
    """

    kind: Literal["prefix"]
    value: KeyPair


MoveManyTarget = Union[
    MoveManyTargetPairs,
    MoveManyTargetPrefix,
]
"""What `Destination.moveMany` moves."""


class MoveManyRequest(TypedDict):
    """Argument of `Destination.moveMany`. Fields are only ever appended.

    Attributes:
        target: Explicit pairs or a prefix.
        cursor: Prefix form: `MoveManyPage.nextCursor` from the previous call; empty
            starts. Ignored for `pairs`.
        limit: Prefix form: objects to move in this call, clamped to `maxBatchKeys`;
            zero means guest default.
        options: Overwrite rule applied to every move (`ifSourceMatch` is ignored).
    """

    target: MoveManyTarget
    cursor: str
    limit: int
    options: MoveOptions


class KeyOutcome(TypedDict):
    """Outcome for one key of a batch call.

    Attributes:
        key: The key this outcome is for (the source key for moves).
        error: Failure for this key; an empty `code` means it succeeded.
    """

    key: str
    error: PluginError


class MoveManyPage(TypedDict):
    """One page of `Destination.moveMany` progress.

    Attributes:
        results: Outcomes in request order. A prefix move done in one step (a folder
            rename) may report a single outcome keyed by the source prefix.
        nextCursor: Prefix form: cursor for the next call; empty when every object
            moved.
        atomic: True when every move in this page was atomic.
        objectsMoved: Objects moved in this page (a folder rename counts what it carried
            when known, else the outcomes).
    """

    results: list[KeyOutcome]
    nextCursor: str
    atomic: bool
    objectsMoved: int


class DeleteManyResult(TypedDict):
    """Result of `Destination.deleteMany`.

    Attributes:
        results: One outcome per requested key, in request order. A missing key
            succeeds.
    """

    results: list[KeyOutcome]


class HeadManyResult(TypedDict):
    """Result of `Destination.headMany`.

    Attributes:
        objects: One probe per requested key, in request order.
    """

    objects: list[HeadOk]


class TranslateKeysResult(TypedDict):
    """Success payload of `Destination.translateKeys`.

    Attributes:
        keys: One translated key per requested host key, in request order: the key the
            guest stores that host key under. Each is a relative key with no empty, `.`
            or `..` segment.
    """

    keys: list[str]


class ChangesRequest(TypedDict):
    """Argument of `Destination.changes`. Fields are only ever appended.

    Attributes:
        cursor: `ChangesPage.cursor` from the previous call. Empty asks for the current
            position: the page carries a cursor and no changes.
        limit: Requested changes per page, clamped to `maxChangesPage`; zero means guest
            default.
    """

    cursor: str
    limit: int


class ObjectChange(TypedDict):
    """One entry of a change feed.

    Attributes:
        key: Object key, relative to the destination root.
        kind: What happened to it.
        info: Listing fields after the change when the feed carries them (signature
            fields may be empty); empty for `removed`.
    """

    key: str
    kind: ObjectChangeKind
    info: ObjectInfo


class ChangesPage(TypedDict):
    """One page of `Destination.changes`.

    Attributes:
        changes: Changes since the request's cursor, oldest first. A key may repeat.
        cursor: Cursor for the next call; never empty.
        more: True when more changes are ready now; call again with `cursor`.
        complete: False when the feed may have missed changes since the request's cursor
            (an overflowed watcher, expired queue retention, a restart): the host backs
            it with a full listing. True means these are all the changes.
        liveWatch: True when this running guest produces the feed itself (a filesystem
            watcher) and stopping it would lose changes, so the host keeps it awake
            while `complete` stays true. Durable feeds (a notification queue) leave it
            false. The host honours it only from the first-party storage plugins it
            starts on demand.
    """

    changes: list[ObjectChange]
    cursor: str
    more: bool
    complete: bool
    liveWatch: bool


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
        catalog: Catalog behaviours a storefront declares so the host can use any store
            for catalog search without naming it; all false for other kinds.
        media: Work kinds and edition carriers the plugin lists and fetches; required
            for storefronts and metadata providers, empty lists for every other kind
            (see `MediaSupport`).
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
    catalog: CatalogCapabilities
    media: MediaSupport


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
            granted. Null when the guest does not export this capability.
        databases: Named plugin-owned `[[databases]]` bindings: each entry is an
            isolated database provisioned by the active adapter, separate from the
            Bookclerk library and from every other plugin. Empty when the manifest
            declares none.
        cancel: Host cancellation for the whole invocation (fence / lease loss). Null
            when the guest does not export this capability.
        storage: `WORK_FS`: host-granted object storage for durable plugin files (work
            filesystem); null unless `[work_fs]` is granted. Job input/output travel on
            `JobRunner.job(controller)`, never here. Null when the guest does not export
            this capability.
        queues: Named `[[queues.producers]]` bindings: one host-served producer per
            binding, each sending into the queue the row names. Empty when the manifest
            declares none.
        kv: Named `[[kv_namespaces]]` bindings: one host-served durable namespace per
            binding, scoped to this plugin. Empty when the manifest declares none.
        sockets: `SOCKETS`: host-brokered outbound WebSockets that stay open while the
            guest sleeps; null unless `[sockets]` is declared and granted. Null when the
            guest does not export this capability.
        notify: `NOTIFY`: raises the notification types the plugin declares in
            `[[notifications.types]]`; null when it declares none. Null when the guest
            does not export this capability.
        network: `NETWORK`: what the operator granted this plugin's network, including
            the destination each network setting names and whether it is approved, so a
            guest can tell an unapproved destination from an unreachable one.
        webhooks: `WEBHOOKS`: the instance's inbound webhooks, their URLs and the
            deliveries the host stored; null unless `[[webhooks]]` is declared and
            granted. Null when the guest does not export this capability.
    """

    config: ExtensibleConfig
    secrets: ExtensibleConfig
    adapter: DatabaseAdapterConfig
    events: NotRequired[EventPublisher | None]
    databases: list[NamedDatabase]
    cancel: NotRequired[Cancellation | None]
    storage: NotRequired[Destination | None]
    queues: list[NamedQueue]
    kv: list[NamedKv]
    sockets: NotRequired[Sockets | None]
    notify: NotRequired[NotificationRaiser | None]
    network: NetworkGrant
    webhooks: NotRequired[Webhooks | None]


class NetworkGrant(TypedDict):
    """The network grant in effect for one `PluginWorker.open`. Read-only
    feedback: the host enforces the grant whatever the guest does with this.

    Attributes:
        outbound: True when the grant allows outbound connections; false for `deny`.
        domains: Fetch destinations in effect, as `plugin.toml` `domains` entries
            (`host`, `*.host`, `host:port`, or `*` for any public host), including
            approved setting destinations; at most `maxNetworkGrantEntries`.
        tcp: Raw TCP destinations in effect, as `host:port` (IPv6 in brackets), one per
            granted port; at most `maxNetworkGrantEntries`.
        settings: One entry per `[[settings]]` entry that declares `network`, in
            declaration order; at most `maxNetworkGrantEntries`.
    """

    outbound: bool
    domains: list[str]
    tcp: list[str]
    settings: list[SettingNetworkGrant]


class SettingNetworkGrant(TypedDict):
    """One network setting and the destination its value names.

    Attributes:
        key: The setting key.
        network: Whether the plugin fetches the destination or opens a socket to it.
        destination: The destination as the consent form shows it: `scheme://host:port`
            for a fetch, `host:port` for TCP (IPv6 in brackets); empty when `state` is
            `unset` or `invalid`. Omitted when absent (wire zero value).
        state: Where the destination stands.
        error: Why the value names no destination, when `state` is `invalid`. Omitted
            when absent (wire zero value).
    """

    key: str
    network: SettingNetworkKind
    destination: NotRequired[str]
    state: SettingGrantState
    error: NotRequired[str]


class Entrypoints(TypedDict):
    """Exported entrypoints returned by `PluginWorker.open`. One capability per
    `plugin.toml` `entrypoints` entry / trigger; null when not exported. The host
    refuses an entrypoint the manifest or operator grant did not allow.

    Attributes:
        eventConsumer: `[[events.consumers]]` trigger: `event(batch)` handler. Null when
            the guest does not export this capability.
        jobRunner: `[triggers] jobs` trigger: `job(controller)` handler. Null when the
            guest does not export this capability.
        storefront: `storefront` entrypoint. Null when the guest does not export this
            capability.
        storage: `storage` entrypoint. Null when the guest does not export this
            capability.
        databaseAdapter: `databaseAdapter` entrypoint. Null when the guest does not
            export this capability.
        remoteLibrary: `remoteLibrary` entrypoint. Null when the guest does not export
            this capability.
        cli: `cli` entrypoint. Null when the guest does not export this capability.
        oidc: `oidc` entrypoint. Null when the guest does not export this capability.
        queueConsumer: `[[queues.consumers]]` trigger: `queue(batch)` handler. Null when
            the guest does not export this capability.
        scheduledHandler: `[triggers] crons` trigger: `scheduled(controller)` handler.
            Null when the guest does not export this capability.
        metadata: `metadata` entrypoint. Null when the guest does not export this
            capability.
        covers: `covers` entrypoint. Null when the guest does not export this
            capability.
        socketHandler: `[sockets]` handler: messages and closes on host-brokered
            sockets. Null when the guest does not export this capability.
        metadataFiles: `metadataFiles` entrypoint. Null when the guest does not export
            this capability.
        notifier: `notifier` entrypoint. Null when the guest does not export this
            capability.
        webhookHandler: `[[webhooks]]` handler: called after the host stores deliveries.
            Null when the guest does not export this capability.
    """

    eventConsumer: NotRequired[EventConsumer | None]
    jobRunner: NotRequired[JobRunner | None]
    storefront: NotRequired[ContentSource | None]
    storage: NotRequired[Destination | None]
    databaseAdapter: NotRequired[Database | None]
    remoteLibrary: NotRequired[RemoteLibrary | None]
    cli: NotRequired[PluginCli | None]
    oidc: NotRequired[Oidc | None]
    queueConsumer: NotRequired[QueueConsumer | None]
    scheduledHandler: NotRequired[ScheduledHandler | None]
    metadata: NotRequired[MetadataProvider | None]
    covers: NotRequired[CoverFetcher | None]
    socketHandler: NotRequired[SocketHandler | None]
    metadataFiles: NotRequired[MetadataFiles | None]
    notifier: NotRequired[Notifier | None]
    webhookHandler: NotRequired[WebhookHandler | None]


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


class EventReadRequest(TypedDict):
    """Request of `EventPublisher.read`: one page of past outbox events, oldest
    first. Only types the instance's consented `[[events.consumers]]` rows
    subscribe to are returned, through the same schema-version and filter match
    as delivery, and only back to `[events].retention_days`.

    Attributes:
        after: Opaque cursor from an earlier `EventReadPage.cursor`; empty starts at
            `sinceUnixMs`. At most `maxEventCursorBytes`. Set at most one of `after` and
            `sinceUnixMs`.
        sinceUnixMs: UTC Unix milliseconds to start from when `after` is empty; zero
            starts at the oldest retained event.
        types: Event types to return; empty means every consented subscription. Each
            entry must be a consented subscription type; at most `maxListPage`.
        limit: Most events to return; zero means `maxListPage`. At most `maxListPage`.
    """

    after: str
    sinceUnixMs: int
    types: list[str]
    limit: int


class EventReadPage(TypedDict):
    """Success payload of `EventPublisher.read`.

    Attributes:
        events: Matching events in outbox order. `deliveryAttempt` is zero and
            `deduplicationKey` is the event id: a read is not a delivery, so dedupe on
            `eventId`.
        cursor: Opaque position after the last event scanned; pass it as `after` to
            continue. Empty only when nothing was scanned and the request had no cursor.
            At most `maxEventCursorBytes`.
        more: True when more events may follow: the page filled or the host's scan
            budget ran out before the newest event.
        expiredEventIds: Ids of the events on this page whose delivery to this plugin
            expired unleased: the events the instance missed.
        truncated: True when the start position is older than `[events].retention_days`,
            so events before the retained window may be gone.
    """

    events: list[DomainEvent]
    cursor: str
    more: bool
    expiredEventIds: list[str]
    truncated: bool


class EventReadReplyOk(TypedDict):
    """``EventReadReply`` member ``ok``.

    Success: one page.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one page.
    """

    kind: Literal["ok"]
    value: EventReadPage


class EventReadReplyErr(TypedDict):
    """``EventReadReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


EventReadReply = Union[
    EventReadReplyOk,
    EventReadReplyErr,
]
"""Result union of `EventPublisher.read`."""


class JobController(TypedDict):
    """Everything one `JobRunner.job` invocation may touch. Capabilities are
    host-served; the guest never sees OS paths or sockets.

    Attributes:
        invocation: Durable command envelope.
        input: Job input objects. Null when the guest does not export this capability.
        output: Job output object store. Null when the guest does not export this
            capability.
        progress: Progress reporter (host coalesces frequent updates). Null when the
            guest does not export this capability.
        cancel: Host cancellation probe for this job (fence / lease). Null when the
            guest does not export this capability.
    """

    invocation: JobInvocation
    input: NotRequired[Source | None]
    output: NotRequired[Destination | None]
    progress: NotRequired[ProgressSink | None]
    cancel: NotRequired[Cancellation | None]


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
        body: Streamed object bytes. Null when the guest does not export this
            capability.
    """

    meta: ObjectMetadata
    body: NotRequired[ByteSource | None]


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


class MoveReplyOk(TypedDict):
    """``MoveReply`` member ``ok``.

    Success: how the move was done.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: how the move was done.
    """

    kind: Literal["ok"]
    value: MoveResult


class MoveReplyErr(TypedDict):
    """``MoveReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


MoveReply = Union[
    MoveReplyOk,
    MoveReplyErr,
]
"""Result union of `Destination.move`."""


class MoveManyReplyOk(TypedDict):
    """``MoveManyReply`` member ``ok``.

    Success: per-key outcomes and the continuation.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: per-key outcomes and the continuation.
    """

    kind: Literal["ok"]
    value: MoveManyPage


class MoveManyReplyErr(TypedDict):
    """``MoveManyReply`` member ``err``.

    Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure of the whole call; `code` is a `PluginErrorCode` wire
            string.
    """

    kind: Literal["err"]
    value: PluginError


MoveManyReply = Union[
    MoveManyReplyOk,
    MoveManyReplyErr,
]
"""Result union of `Destination.moveMany`."""


class DeleteManyReplyOk(TypedDict):
    """``DeleteManyReply`` member ``ok``.

    Success: per-key outcomes.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: per-key outcomes.
    """

    kind: Literal["ok"]
    value: DeleteManyResult


class DeleteManyReplyErr(TypedDict):
    """``DeleteManyReply`` member ``err``.

    Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure of the whole call; `code` is a `PluginErrorCode` wire
            string.
    """

    kind: Literal["err"]
    value: PluginError


DeleteManyReply = Union[
    DeleteManyReplyOk,
    DeleteManyReplyErr,
]
"""Result union of `Destination.deleteMany`."""


class HeadManyReplyOk(TypedDict):
    """``HeadManyReply`` member ``ok``.

    Success: one probe per key.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one probe per key.
    """

    kind: Literal["ok"]
    value: HeadManyResult


class HeadManyReplyErr(TypedDict):
    """``HeadManyReply`` member ``err``.

    Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure of the whole call; `code` is a `PluginErrorCode` wire
            string.
    """

    kind: Literal["err"]
    value: PluginError


HeadManyReply = Union[
    HeadManyReplyOk,
    HeadManyReplyErr,
]
"""Result union of `Destination.headMany`."""


class TranslateKeysReplyOk(TypedDict):
    """``TranslateKeysReply`` member ``ok``.

    Success: one translated key per host key.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one translated key per host key.
    """

    kind: Literal["ok"]
    value: TranslateKeysResult


class TranslateKeysReplyErr(TypedDict):
    """``TranslateKeysReply`` member ``err``.

    Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure of the whole call; `code` is a `PluginErrorCode` wire
            string.
    """

    kind: Literal["err"]
    value: PluginError


TranslateKeysReply = Union[
    TranslateKeysReplyOk,
    TranslateKeysReplyErr,
]
"""Result union of `Destination.translateKeys`."""


class ChangesReplyOk(TypedDict):
    """``ChangesReply`` member ``ok``.

    Success: one page of changes.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one page of changes.
    """

    kind: Literal["ok"]
    value: ChangesPage


class ChangesReplyErr(TypedDict):
    """``ChangesReply`` member ``err``.

    Typed failure; `invalid_cursor` means the cursor expired and the host
    starts over with a full listing.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `invalid_cursor` means the cursor expired and the host
            starts over with a full listing.
    """

    kind: Literal["err"]
    value: PluginError


ChangesReply = Union[
    ChangesReplyOk,
    ChangesReplyErr,
]
"""Result union of `Destination.changes`."""


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
        body: Streamed object bytes. Null when the guest does not export this
            capability.
    """

    meta: ObjectMetadata
    body: NotRequired[ByteSource | None]


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

    async def move(self, from_: str, to: str, options: MoveOptions) -> MoveReply:
        """Move one object (requires `storage.move`). Repeating a finished move
        succeeds with `alreadyMoved`.

        Args:
            from_: Source object key.
            to: Target object key.
            options: Overwrite and source preconditions.

        Returns:
            ``MoveReply``
        """
        ...

    async def move_many(self, request: MoveManyRequest) -> MoveManyReply:
        """Move a set of objects or a whole prefix (requires `storage.moveMany`).
        Paged and safe to repeat.

        Args:
            request: Pairs or prefix, cursor, and options.

        Returns:
            ``MoveManyReply``
        """
        ...

    async def delete_many(self, keys: list[str]) -> DeleteManyReply:
        """Remove up to `maxBatchKeys` objects (requires `storage.deleteMany`).

        Args:
            keys: Object keys to remove.

        Returns:
            ``DeleteManyReply``
        """
        ...

    async def head_many(self, keys: list[str]) -> HeadManyReply:
        """Probe up to `maxBatchKeys` objects (requires `storage.headMany`).

        Args:
            keys: Object keys to probe.

        Returns:
            ``HeadManyReply``
        """
        ...

    async def changes(self, request: ChangesRequest) -> ChangesReply:
        """Keys changed since a cursor (requires `storage.changes`). A hint for
        what to read; a scheduled listing stays the backstop.

        Args:
            request: Cursor and page size.

        Returns:
            ``ChangesReply``
        """
        ...

    async def translate_keys(self, keys: list[str]) -> TranslateKeysReply:
        """Translate up to `maxBatchKeys` host keys into the keys the guest stores
        them under (requires `storage.translateKeys`). A host key is `/`-joined
        segments, none empty, `.`, `..` or holding `\` or a control character.
        The same host key must always translate to the same key. Without the
        feature the host stores each host key as written.

        Args:
            keys: Host keys to translate.

        Returns:
            ``TranslateKeysReply``
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

    async def read(self, request: EventReadRequest) -> EventReadReply:
        """Read one page of past outbox events this instance subscribes to, oldest
        first, to catch up after a gap (`env.EVENTS.read`).

        Args:
            request: Where to start, which types, how many.

        Returns:
            ``EventReadReply``
        """
        ...


class NamedDatabase(TypedDict):
    """One named plugin-owned database binding delivered on `Bindings.databases`.

    Attributes:
        name: Binding name from `plugin.toml` `[[databases]]`.
        database: Isolated typed SQL session for this binding (plugin-owned schema).
            Null when the guest does not export this capability.
    """

    name: str
    database: NotRequired[GuestDatabase | None]


class ScheduledHandler(Protocol):
    """`[triggers] crons` handler (`Entrypoints.scheduledHandler`), the Workers
    `scheduled(controller)` trigger. Each fire is delivered at least once;
    a fire that throws is retried with backoff, keeping `scheduledTime` and
    `fireId`, so handlers dedupe on `fireId`.
    """

    async def scheduled(self, controller: ScheduledController) -> EmptyReply:
        """Run one cron fire to completion.

        Args:
            controller: The fire being delivered.

        Returns:
            ``EmptyReply``
        """
        ...


class QueueConsumer(Protocol):
    """`[[queues.consumers]]` handler (`Entrypoints.queueConsumer`). Delivery is
    at-least-once; consumers must be idempotent on `QueueMessage.id`.
    """

    async def queue(self, batch: QueueBatch) -> QueueBatchReply:
        """Deliver one ordered batch of messages from one queue.

        Args:
            batch: Messages in delivery order.

        Returns:
            ``QueueBatchReply``
        """
        ...


class QueueProducer(Protocol):
    """Queue producer binding: host-served send into the queue a
    `[[queues.producers]]` row names, granted through `Bindings.queues`.
    """

    async def send(self, message: QueueSendMessage) -> QueueSendReply:
        """Send one message.

        Args:
            message: Body, encoding, and delay.

        Returns:
            ``QueueSendReply``
        """
        ...

    async def send_batch(self, messages: list[QueueSendMessage], delay_seconds: int) -> QueueSendReply:
        """Send up to `maxListPage` messages in order; `delaySeconds` applies to
        every message that carries no delay of its own.

        Args:
            messages: Messages in send order.
            delay_seconds: Default delay for the batch.

        Returns:
            ``QueueSendReply``
        """
        ...


class NamedQueue(TypedDict):
    """One named queue producer delivered on `Bindings.queues`.

    Attributes:
        name: Binding name from `plugin.toml` `[[queues.producers]]`.
        queue: Queue the binding sends to.
        producer: Host-served producer for that queue. Null when the guest does not
            export this capability.
    """

    name: str
    queue: str
    producer: NotRequired[QueueProducer | None]


class KvNamespace(Protocol):
    """Durable key/value namespace: a Workers `KVNamespace`-shaped binding the host
    serves from the library database, scoped to the plugin and the namespace a
    `[[kv_namespaces]]` row names, granted through `Bindings.kv`. Values live in
    the library, so an instance moved to another host reads its own keys.
    """

    async def get(self, key: str) -> KvGetReply:
        """Read one key; `found = false` when absent or expired.

        Args:
            key: Key, at most `maxKvKeyBytes` UTF-8 bytes.

        Returns:
            ``KvGetReply``
        """
        ...

    async def put(self, key: str, value: bytes, options: KvPutOptions) -> KvPutReply:
        """Write one key, replacing any previous value and metadata.

        Args:
            key: Key, at most `maxKvKeyBytes` UTF-8 bytes.
            value: Value, at most the namespace's `max_value_bytes`.
            options: Expiration and metadata.

        Returns:
            ``KvPutReply``
        """
        ...

    async def delete(self, key: str) -> KvDeleteReply:
        """Delete one key; a missing key is not an error.

        Args:
            key: Key to delete.

        Returns:
            ``KvDeleteReply``
        """
        ...

    async def list(self, options: KvListOptions) -> KvListReply:
        """List keys in lexical order, one page at a time.

        Args:
            options: Prefix, cursor, and page size.

        Returns:
            ``KvListReply``
        """
        ...


class NamedKv(TypedDict):
    """One named durable namespace delivered on `Bindings.kv`.

    Attributes:
        name: Binding name from `plugin.toml` `[[kv_namespaces]]`.
        namespace: Namespace the binding is scoped to.
        kv: Host-served namespace. Null when the guest does not export this capability.
    """

    name: str
    namespace: str
    kv: NotRequired[KvNamespace | None]


class Sockets(Protocol):
    """Host-brokered outbound WebSockets, granted through `Bindings.sockets`.

    The guest asks for a socket; a broker the host runs for the plugin
    instance dials it under the plugin's network grant and keeps it open while
    the guest sleeps, in the manner of hibernatable Workers WebSockets. Each
    incoming message wakes the guest and arrives on
    `Entrypoints.socketHandler`. Heartbeat pairs declared at connect are
    answered by the broker without waking the guest.
    """

    async def connect(self, request: SocketConnectRequest) -> SocketConnectReply:
        """Open a socket; it is owned by the plugin instance, not this invocation.

        Args:
            request: Target, subprotocols, headers, heartbeats.

        Returns:
            ``SocketConnectReply``
        """
        ...

    async def send(self, socket: str, frame: SocketFrame) -> EmptyReply:
        """Send one message on an open socket.

        Args:
            socket: Socket id from `connect` or `list`.
            frame: Message, at most `maxSocketFrameBytes`.

        Returns:
            ``EmptyReply``
        """
        ...

    async def close(self, socket: str, code: int, reason: str) -> EmptyReply:
        """Close a socket; closing an unknown or closed socket is not an error.

        Args:
            socket: Socket id.
            code: WebSocket close code; zero sends 1000.
            reason: Close reason, at most 123 UTF-8 bytes.

        Returns:
            ``EmptyReply``
        """
        ...

    async def set_attachment(self, socket: str, attachment: bytes) -> EmptyReply:
        """Replace the attachment the host keeps for a socket.

        Args:
            socket: Socket id.
            attachment: Opaque bytes, at most `maxSocketAttachmentBytes`.

        Returns:
            ``EmptyReply``
        """
        ...

    async def list(self) -> SocketListReply:
        """The instance's open sockets, for a guest that just woke.

        Returns:
            ``SocketListReply``
        """
        ...


class SocketHandler(Protocol):
    """Guest handler for host-brokered sockets (`Entrypoints.socketHandler`)."""

    async def message(self, event: SocketMessage) -> SocketMessageReply:
        """One message arrived on an open socket.

        Args:
            event: Socket, frame, and attachment.

        Returns:
            ``SocketMessageReply``
        """
        ...

    async def closed(self, event: SocketClosed) -> EmptyReply:
        """A socket closed, by the remote side, the network, or the host.

        Args:
            event: Socket, close code, and attachment.

        Returns:
            ``EmptyReply``
        """
        ...


class Webhooks(Protocol):
    """Host-served inbound webhooks, granted through `Bindings.webhooks`.

    The host serves each `[[webhooks]]` row at a secret URL under the
    library's public origin, checks every request against the row's body cap,
    rate limit and signature rule, stores it and answers at once. The guest
    takes stored deliveries with `pull` and settles them with `ack`, in the
    manner of a Cloudflare Queues HTTP pull consumer: a pulled delivery stays
    leased until acknowledged, retried, or its visibility timeout ends.
    """

    async def url(self, name: str) -> WebhookUrlReply:
        """The URL an outside service should call for one webhook.

        Args:
            name: `[[webhooks]]` name.

        Returns:
            ``WebhookUrlReply``
        """
        ...

    async def pull(self, request: WebhookPullRequest) -> WebhookPullReply:
        """Lease up to `batchSize` stored deliveries, oldest first.

        Args:
            request: Webhook, batch size, visibility timeout.

        Returns:
            ``WebhookPullReply``
        """
        ...

    async def ack(self, request: WebhookAckRequest) -> EmptyReply:
        """Acknowledge finished deliveries and offer retried ones again at once.

        Args:
            request: Webhook and lease ids.

        Returns:
            ``EmptyReply``
        """
        ...


class WebhookHandler(Protocol):
    """Guest handler for inbound webhooks (`Entrypoints.webhookHandler`)."""

    async def delivered(self, event: WebhookDelivered) -> EmptyReply:
        """The host stored deliveries for one webhook; the guest pulls them.

        Args:
            event: Webhook and how many wait.

        Returns:
            ``EmptyReply``
        """
        ...


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
        """One page of one account's library scan pass (see `ScanPage`).

        Args:
            params: Account, cursor, sync token, credential.

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

    async def revoke_account(self, params: RevokeAccountParams) -> EmptyReply:
        """Undo store-side state for one account before the host deletes its
        stored credentials (for example, deregister the device registered at
        login). Best effort: the host revokes the account whatever this returns.

        Args:
            params: Account id and its stored credentials.

        Returns:
            ``EmptyReply``
        """
        ...

    async def purchase_hints(self, params: PurchaseHintsParams) -> PurchaseHintsReply:
        """Purchase links / price hints for several titles in one call, so a guest
        whose backend prices many products per request makes one request for a
        page of cards. A guest that cannot batch answers each query as
        `purchaseHint` would.

        Args:
            params: The titles to price, in order.

        Returns:
            ``PurchaseHintsReply``
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


class MetadataProvider(Protocol):
    """`metadata` entrypoint: bibliographic lookups against a public catalog
    (OpenLibrary, Audnexus, ...). A provider describes what it finds as
    edition records; the host decides identity and never trusts a record to
    name an existing library row. Network access is limited to the domains the
    manifest's `[capabilities.network]` declares.
    """

    async def health(self) -> HealthReply:
        """Liveness / readiness probe.

        Returns:
            ``HealthReply``
        """
        ...

    async def lookup(self, params: MetadataLookupParams) -> MetadataRecordsReply:
        """Records for the editions the given identifiers name.

        Args:
            params: Identifiers to resolve and a result cap.

        Returns:
            ``MetadataRecordsReply``
        """
        ...

    async def search(self, params: MetadataSearchParams) -> MetadataRecordsReply:
        """Records matching a title and, optionally, an author.

        Args:
            params: Title, author, language and a result cap.

        Returns:
            ``MetadataRecordsReply``
        """
        ...

    async def chapters(self, params: MetadataChaptersParams) -> MetadataChaptersReply:
        """Chapter layout of the edition the given identifiers name.

        Args:
            params: Identifiers of one edition.

        Returns:
            ``MetadataChaptersReply``
        """
        ...


class CoverFetcher(Protocol):
    """`covers` entrypoint: downloads cover images on the host's behalf, so the
    host itself never makes an outbound request to a URL a plugin or a store
    supplied. The fetcher only transports bytes: the host sniffs, decodes and
    re-encodes every image in its media worker before anything stores or
    serves it. Network access is limited to what the manifest's
    `[capabilities.network]` declares.
    """

    async def health(self) -> HealthReply:
        """Liveness / readiness probe.

        Returns:
            ``HealthReply``
        """
        ...

    async def fetch_covers(self, params: CoverFetchParams, sink: Destination) -> CoverImagesReply:
        """Fetches every URL concurrently and streams each body into `sink` under
        its index in `params.urls` (`"0"`, `"1"`, ...). The reply says, per URL,
        whether its body was stored.

        Args:
            params: URLs and the per-image byte cap.
            sink: Host-served store that accepts only those keys.

        Returns:
            ``CoverImagesReply``
        """
        ...


class MetadataFiles(Protocol):
    """`metadataFiles` entrypoint: reads and writes metadata files that sit next
    to an edition's media (sidecars such as `metadata.json`, OPF, NFO or CUE),
    one dialect per format. The plugin only converts bytes: the host reads the
    file, chooses every path and writes the result, so a dialect plugin needs
    no network or file grants. The manifest's `[metadata_files]` table names
    each dialect, the file names it reads and the fields it carries, so the
    host routes a file to a sleeping plugin without starting it.
    """

    async def health(self) -> HealthReply:
        """Liveness / readiness probe.

        Returns:
            ``HealthReply``
        """
        ...

    async def parse(self, params: MetadataFileParseParams) -> MetadataFileParseReply:
        """What one metadata file says about its edition.

        Args:
            params: Dialect, file name and bytes.

        Returns:
            ``MetadataFileParseReply``
        """
        ...

    async def render(self, params: MetadataFileRenderParams) -> MetadataFileRenderReply:
        """The bytes of a metadata file with the given fields set to the record's
        values and everything else kept from the existing file.

        Args:
            params: Dialect, record, fields and existing bytes.

        Returns:
            ``MetadataFileRenderReply``
        """
        ...


class Notifier(Protocol):
    """`notifier` entrypoint: delivers one host-rendered message to one
    destination (an email address, a chat webhook, a push subscription). The
    host decides what is worth a notification, who gets it and which instance
    carries it; the plugin only sends. Each send is one claimed `notify.send`
    job, so the host owns retries: a reply of `rate_limited`, `unavailable`,
    `deadline_exceeded` or `cancelled` is retried (`rate_limited` honouring
    `details.retryAfterMs`), any other error is a permanent failure for that
    destination.
    """

    async def health(self) -> HealthReply:
        """Liveness / readiness probe: credentials and reachability.

        Returns:
            ``HealthReply``
        """
        ...

    async def send(self, params: NotifySendParams) -> NotifySendReply:
        """Deliver one message to one destination.

        Args:
            params: Message and destination.

        Returns:
            ``NotifySendReply``
        """
        ...

    async def validate_destination(self, params: NotifyValidateParams) -> NotifyValidateReply:
        """Normalise and check a personal destination's address before the host
        saves it.

        Args:
            params: Address document to check.

        Returns:
            ``NotifyValidateReply``
        """
        ...


class NotificationRaiser(Protocol):
    """`NOTIFY` binding: raises a notification type the plugin declared in
    `[[notifications.types]]`. The host router applies the administrators'
    routing and each patron's preferences; the raising plugin never learns
    where the notification went.
    """

    async def raise_(self, params: NotifyRaiseParams) -> NotifyRaiseReply:
        """Raise one notification.

        Args:
            params: Declared type, fields and optional patron.

        Returns:
            ``NotifyRaiseReply``
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
            `WORK_FS`, `OAUTH`, `KV`, `EVENTS`, `[[queues.producers]]` names, ...).
        queueConsumers: Queues the default entrypoint consumes (`queue(batch)` trigger).
        queueProducers: Queues the guest may send to through its `[[queues.producers]]`
            bindings.
        crons: Five-field UTC cron schedules the default entrypoint handles
            (`scheduled(controller)` trigger, `[triggers] crons`), normalized to single
            spaces.
        sockets: True when the guest declared `[sockets]`: it receives the host-brokered
            `Sockets` binding and may export a `SocketHandler` for pushed messages.
        webhooks: True when the guest declared `[[webhooks]]`: it receives the host-
            served `Webhooks` binding and may export a `WebhookHandler` the host wakes
            when it stores deliveries.
    """

    entrypoints: list[Entrypoint]
    consumes: list[EventConsumerSpec]
    produces: list[str]
    jobs: list[str]
    databases: list[str]
    bindings: list[str]
    queueConsumers: list[str]
    queueProducers: list[str]
    crons: list[str]
    sockets: bool
    webhooks: bool


class CatalogCapabilities(TypedDict):
    """Catalog behaviours a storefront declares in `describe()`. Catalog search
    reads them instead of special-casing plugin ids; every flag defaults to
    false, so a guest that says nothing keeps the plain behaviour.

    Attributes:
        detailByIsbn: True when `catalogDetail` resolves a bare ISBN (sent in both
            `productId` and `isbn`) to a full bibliographic record. Catalog search then
            asks this store to fill description, narrators, runtime and genres on lean
            result rows from any store that carry an ISBN.
        priceByIsbn: True when `purchaseHint` / `purchaseHints` price a query by its
            exact ISBN (`lookup = isbn`): the answer is the store's listing of that ISBN
            or not found, never a title match. The host then prices a card at this store
            by the ISBNs of the card's recording before searching by title.
        priceByAsin: True when `purchaseHint` / `purchaseHints` price a query by its
            exact ASIN (`lookup = asin`), as `priceByIsbn` does for ISBNs.
        detailByProductId: True when `catalogDetail` resolves this store's own
            `productId` (an id its search, expand or deals hits gave) to a full
            bibliographic record. Catalog search then asks this store to fill lean
            result rows it listed itself, and title dialogs ask it for the editions it
            listed. A store that leaves it false is never woken for a detail lookup by
            product id.
    """

    detailByIsbn: bool
    priceByIsbn: bool
    priceByAsin: bool
    detailByProductId: bool


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
    """Params of `ContentSource.scan`: one page of one account's scan pass. The
    host injects that account's sealed credentials so the plugin does not need a
    private credential store under `pluginDataDir`, and drives the pass page by
    page: the first call of a pass sends an empty `cursor`, every later call
    echoes the previous page's `nextCursor`.

    Attributes:
        pluginDataDir: Scoped plugin data directory.
        accountId: Account this pass scans; always one of the plugin's connected
            accounts.
        pageSize: Storefront page size; the host always sends an explicit value.
        importEpisodes: When true, import podcast/episode-style rows.
        importPlusTitles: When true, import Plus/catalog entitlement titles.
        credential: Host-loaded credential blob for `accountId`.
        cursor: Opaque position the guest returned as `ScanPage.nextCursor`; empty on
            the first page of a pass. At most `maxScanCursorBytes`.
        syncToken: Opaque token the guest returned as `ScanPage.syncToken` at the end of
            the account's last pass; empty asks for a full pass. At most
            `maxScanCursorBytes`.
    """

    pluginDataDir: str
    accountId: str
    pageSize: int
    importEpisodes: bool
    importPlusTitles: bool
    credential: AccountCredential
    cursor: str
    syncToken: str


class MediaSupport(TypedDict):
    """Media a plugin works with, declared in `describe()` and in the manifest's
    `[media]` table. Required for plugins that export the storefront or
    metadata entrypoint: the host refuses to spawn one whose `workKinds` or
    `lists` is empty, holds `unspecified`, or is wider than its manifest, and
    refuses any record whose kinds fall outside the declaration.

    Attributes:
        workKinds: Work kinds the plugin reports.
        lists: Edition carriers the plugin reports in scans, catalog search and metadata
            records.
        fetches: Edition carriers `ContentSource.fetchTitle` can deliver; a subset of
            `lists`. Empty for metadata providers and for stores that only list.
    """

    workKinds: list[WorkKind]
    lists: list[MediaKind]
    fetches: list[MediaKind]


class Identifier(TypedDict):
    """One external identifier a storefront or metadata provider reports.

    Attributes:
        scheme: Scheme the value belongs to; it decides the identifier's level.
        value: The identifier as the source printed it; the host trims and normalises.
        marketplace: Marketplace the id is scoped to (ASIN); absent for global schemes.
            Omitted when absent (wire zero value).
    """

    scheme: IdentifierScheme
    value: str
    marketplace: NotRequired[str]


class Credit(TypedDict):
    """One credited contributor. Credits are listed in billing order.

    Attributes:
        name: Contributor display name as the source prints it, one person or body.
        role: What the contributor did.
        level: Level the credit attaches to.
    """

    name: str
    role: CreditRole
    level: CreditLevel


class SeriesEntry(TypedDict):
    """A work's place in one series.

    Attributes:
        name: Series name as the source prints it.
        position: Position label as the source prints it (`1`, `2.5`, `1-3`). Omitted
            when absent (wire zero value).
        storeSeriesId: The store's own id for the series (an Audible series ASIN) when
            known. Omitted when absent (wire zero value).
    """

    name: str
    position: NotRequired[str]
    storeSeriesId: NotRequired[str]


class ScanBook(TypedDict):
    """One library title returned by `ContentSource.scan`. The host forces `source`
    to the plugin id and, until the library stores works, expressions and
    editions, maps the record onto one flat library row.

    Attributes:
        accountId: Account that owns this library entry.
        productId: Storefront product / SKU id.
        title: Primary title string.
        marketplace: Marketplace / region when known. Omitted when absent (wire zero
            value).
        publisher: Publisher name when known. Omitted when absent (wire zero value).
        lengthMinutes: Runtime in whole minutes when known. Omitted when absent (wire
            zero value).
        subtitle: Subtitle when distinct from `title`. Omitted when absent (wire zero
            value).
        workKind: Kind of work the title is.
        expressionKind: How the work is realised in this edition.
        mediaKind: Carrier of this edition.
        identifiers: External identifiers (ISBN, ASIN, OpenLibrary ids).
        credits: Credited contributors in billing order.
        seriesEntries: Series the work belongs to.
        parentProductId: Product id of the parent podcast show, for a podcast episode.
            Omitted when absent (wire zero value).
        language: Content language code when known. Omitted when absent (wire zero
            value).
        abridgement: Whether the edition is abridged when the storefront says so.
        publishedAt: Publication date as RFC 3339 when known. Omitted when absent (wire
            zero value).
        purchasedAt: When the account bought or added the title, as RFC 3339, when
            known. Omitted when absent (wire zero value).
        categories: Category / genre labels as a single string when known. Omitted when
            absent (wire zero value).
    """

    accountId: str
    productId: str
    title: str
    marketplace: NotRequired[str]
    publisher: NotRequired[str]
    lengthMinutes: NotRequired[int]
    subtitle: NotRequired[str]
    workKind: WorkKind
    expressionKind: ExpressionKind
    mediaKind: MediaKind
    identifiers: list[Identifier]
    credits: list[Credit]
    seriesEntries: list[SeriesEntry]
    parentProductId: NotRequired[str]
    language: NotRequired[str]
    abridgement: Abridgement
    publishedAt: NotRequired[str]
    purchasedAt: NotRequired[str]
    categories: NotRequired[str]


class ScanPage(TypedDict):
    """One page of an account's scan pass, the result of `ContentSource.scan`.
    A pass is the run of pages from an empty `cursor` to the page whose
    `nextCursor` is empty; `syncToken` and `complete` are read from that last
    page only. A guest that does not page answers the first call with every
    title, an empty `nextCursor` and `complete = true`.

    Attributes:
        books: Titles for the host to upsert; at most `maxScanPageBooks`.
        removed: Product ids the storefront reports as gone from the account since the
            pass's `syncToken` (delta passes only); at most `maxScanPageBooks`.
        nextCursor: Opaque position of the next page; empty ends the pass. At most
            `maxScanCursorBytes`; never equal to the cursor this page answered.
        syncToken: On the last page: the opaque token the host sends as
            `ScanParams.syncToken` on the account's next pass; empty means the next pass
            is a full one. At most `maxScanCursorBytes`.
        complete: On the last page: true when the pass listed every title the account
            holds (a full pass), false for a delta pass that listed only changes.
        storePages: Storefront requests this page took.
        refreshedCredentials: The account's credentials as the guest holds them after
            refreshing them while producing this page (for example a renewed access
            token): the same encoding login returned, a JSON object of at most
            `maxRefreshedCredentialBytes`. The host stores them in place of the
            credentials it sent, unless the account was signed in again meanwhile, and
            sends them on the pass's later pages. Empty when nothing changed. Omitted
            when absent (wire zero value).
    """

    books: list[ScanBook]
    removed: list[str]
    nextCursor: str
    syncToken: str
    complete: bool
    storePages: int
    refreshedCredentials: NotRequired[bytes]


class ScanReplyOk(TypedDict):
    """``ScanReply`` member ``ok``.

    Success: one page of the pass.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one page of the pass.
    """

    kind: Literal["ok"]
    value: ScanPage


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


class FetchedFile(TypedDict):
    """One file `fetchTitle` wrote under the cache directory.

    Attributes:
        path: Absolute path to the file under `cacheDir`.
        role: What the file is to the edition.
        contentType: MIME type when the guest knows it (`audio/mp4`,
            `application/epub+zip`, `image/jpeg`); the host sniffs the file either way.
            Omitted when absent (wire zero value).
        title: Part title (disc or chapter label), for a `part`. Omitted when absent
            (wire zero value).
        durationMs: Playing time of the file in milliseconds, for audio, when known.
            Omitted when absent (wire zero value).
    """

    path: str
    role: FetchedFileRole
    contentType: NotRequired[str]
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
        mediaKind: Carrier of what was fetched. Must be one of the storefront's declared
            `MediaSupport.fetches`; `unspecified` is refused.
        files: The files the guest wrote under the cache directory: the edition's
            ordered `part`s or one `whole` file, plus an optional `cover`.
        chapters: Chapter markers; empty when unknown.
        pdfUrl: Companion PDF download URL when the store exposes one; the host
            downloads it itself. Omitted when absent (wire zero value).
        refreshedCredentials: The account's credentials as the guest holds them after
            refreshing them during the fetch, as in `ScanPage.refreshedCredentials`.
            Empty when nothing changed. Omitted when absent (wire zero value).
    """

    mediaKind: MediaKind
    files: list[FetchedFile]
    chapters: list[ChapterMarker]
    pdfUrl: NotRequired[str]
    refreshedCredentials: NotRequired[bytes]


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
        limit: Maximum hits to return; 0 means the storefront default (20 hits).
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
        lookup: How the host asks the store to find the title. `unspecified` (an older
            host) leaves the guest free to use any field, as before.
    """

    productId: NotRequired[str]
    title: NotRequired[str]
    authors: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    region: str
    withPrice: bool
    lookup: PriceLookup


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


class RevokeAccountParams(TypedDict):
    """Params of `ContentSource.revokeAccount`. The host sends the credentials it
    is about to delete so the guest can sign the device out at the store.

    Attributes:
        accountId: Account being revoked.
        credentials: Decrypted credential blob for this account, exactly as login
            returned it; never log it. Empty when none is stored. Omitted when absent
            (wire zero value).
    """

    accountId: str
    credentials: NotRequired[bytes]


class CatalogHit(TypedDict):
    """One catalog / candidate hit returned by `searchCatalog`,
    `expandCandidates`, `listDeals`, and `catalogDetail`.

    Attributes:
        productId: Storefront product / SKU id.
        title: Primary title.
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
        workKind: Kind of work the hit is.
        expressionKind: How the work is realised in this edition.
        mediaKind: Carrier of this edition.
        identifiers: External identifiers (ISBN, ASIN, OpenLibrary ids).
        credits: Credited contributors in billing order.
        seriesEntries: Series the work belongs to.
        parentProductId: Product id of the parent podcast show, for a podcast episode.
            Omitted when absent (wire zero value).
        rankHint: The storefront's own rank for this hit in `searchCatalog` results:
            1-based position in the store's ordering for the requested sort across all
            pages (page 2 of 20-hit pages starts at 21), or a store-wide popularity rank
            when the store has one. 0 means unranked. Catalog search merges stores by it
            for relevance and popularity sorts and puts unranked hits after ranked ones.
            Other catalog methods leave it 0. Omitted when absent (wire zero value).
    """

    productId: str
    title: str
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
    workKind: WorkKind
    expressionKind: ExpressionKind
    mediaKind: MediaKind
    identifiers: list[Identifier]
    credits: list[Credit]
    seriesEntries: list[SeriesEntry]
    parentProductId: NotRequired[str]
    rankHint: NotRequired[int]


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
        records: Every priced listing a title search matched (`lookup = title`), each
            with its full catalog record, so the host can tell recordings apart by
            identifiers, credits and kinds rather than by title. Empty for other lookups
            and for guests that do not return records; `hint` then stays the answer.
    """

    found: bool
    hint: PurchaseHint
    records: list[PricedRecord]


class PricedRecord(TypedDict):
    """One priced listing returned by a title search.

    Attributes:
        record: The listing's catalog record: identifiers, credits, kinds, language.
        hint: Its purchase link and prices, as `PurchaseHint` carries them.
    """

    record: CatalogHit
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


class PurchaseHintsParams(TypedDict):
    """Params of `ContentSource.purchaseHints`: the titles to price together.

    Attributes:
        queries: One query per title, each as for `purchaseHint`; at most `maxListPage`.
    """

    queries: list[PurchaseHintParams]


class PurchaseHintsResult(TypedDict):
    """Success payload of `ContentSource.purchaseHints`.

    Attributes:
        results: One answer per query, in the order the queries were sent.
    """

    results: list[PurchaseHintResult]


class PurchaseHintsReplyOk(TypedDict):
    """``PurchaseHintsReply`` member ``ok``.

    Success: an answer per query.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: an answer per query.
    """

    kind: Literal["ok"]
    value: PurchaseHintsResult


class PurchaseHintsReplyErr(TypedDict):
    """``PurchaseHintsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


PurchaseHintsReply = Union[
    PurchaseHintsReplyOk,
    PurchaseHintsReplyErr,
]
"""Result union of `ContentSource.purchaseHints`. A whole-batch `err` (for
example `rateLimited`) applies to every query in it.
"""


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


class QueueMessage(TypedDict):
    """One message on a Workers-style queue, delivered by `QueueConsumer.queue`.

    Attributes:
        id: Outbox row id; stable across redeliveries.
        timestampUnixMs: When the producer sent the message (UTC Unix milliseconds).
        body: Encoded body; at most `maxEventPayloadBytes`.
        contentType: Body encoding: `json`, `text`, or `bytes`.
        attempts: Delivery counter, starting at 1.
        source: Producer plugin id; empty when the host sent it.
    """

    id: str
    timestampUnixMs: int
    body: bytes
    contentType: str
    attempts: int
    source: str


class QueueBatch(TypedDict):
    """One `QueueConsumer.queue` delivery: messages from one queue in send order.

    Attributes:
        queue: Queue the messages were sent to.
        messages: Messages in delivery order; the reply carries one `QueueResult` per
            entry.
    """

    queue: str
    messages: list[QueueMessage]


class QueueRetry(TypedDict):
    """Redeliver a queue message later.

    Attributes:
        delaySeconds: Seconds before redelivery; zero applies the consumer's
            `retry_delay`.
        reason: Short human reason.
    """

    delaySeconds: int
    reason: str


class QueueResultAck(TypedDict):
    """``QueueResult`` member ``ack``.

    Handled; the host marks the message delivered.

    Attributes:
        kind: Always ``"ack"``.
    """

    kind: Literal["ack"]


class QueueResultRetry(TypedDict):
    """``QueueResult`` member ``retry``.

    Redeliver later.

    Attributes:
        kind: Always ``"retry"``.
        value: Redeliver later.
    """

    kind: Literal["retry"]
    value: QueueRetry


QueueResult = Union[
    QueueResultAck,
    QueueResultRetry,
]
"""Per-message outcome inside a `QueueConsumer.queue` batch reply."""


class QueueBatchOk(TypedDict):
    """Success payload of `QueueConsumer.queue`: exactly one entry per
    `QueueBatch.messages` entry, in order. A short list is a host-side error
    and the missing tail is retried.

    Attributes:
        results: Per-message outcomes in delivery order.
    """

    results: list[QueueResult]


class QueueBatchReplyOk(TypedDict):
    """``QueueBatchReply`` member ``ok``.

    Success: per-message outcomes.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: per-message outcomes.
    """

    kind: Literal["ok"]
    value: QueueBatchOk


class QueueBatchReplyErr(TypedDict):
    """``QueueBatchReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


QueueBatchReply = Union[
    QueueBatchReplyOk,
    QueueBatchReplyErr,
]
"""Result union of `QueueConsumer.queue`."""


class QueueSendMessage(TypedDict):
    """One message to send through a `[[queues.producers]]` binding.

    Attributes:
        body: Encoded body; at most `maxEventPayloadBytes`.
        contentType: Body encoding: `json` (default when empty), `text`, or `bytes`.
        delaySeconds: Seconds before the message may be delivered; zero means now.
    """

    body: bytes
    contentType: str
    delaySeconds: int


class QueueSendOk(TypedDict):
    """Success payload of `QueueProducer.send` / `sendBatch`.

    Attributes:
        messageIds: Outbox row ids in send order.
    """

    messageIds: list[str]


class QueueSendReplyOk(TypedDict):
    """``QueueSendReply`` member ``ok``.

    Success: the ids the host assigned.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the ids the host assigned.
    """

    kind: Literal["ok"]
    value: QueueSendOk


class QueueSendReplyErr(TypedDict):
    """``QueueSendReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


QueueSendReply = Union[
    QueueSendReplyOk,
    QueueSendReplyErr,
]
"""Result union of `QueueProducer.send` / `sendBatch`."""


class KvPutOptions(TypedDict):
    """Expiration and metadata for `KvNamespace.put`.

    Attributes:
        expirationUnixMs: Absolute expiry (UTC Unix milliseconds); zero means none.
        expirationTtlSeconds: Relative expiry in seconds from the write; zero means
            none. Ignored when `expirationUnixMs` is set.
        metadata: Opaque JSON metadata text, at most `maxKvMetadataBytes`; empty means
            none.
    """

    expirationUnixMs: int
    expirationTtlSeconds: int
    metadata: str


class KvGetOk(TypedDict):
    """Success payload of `KvNamespace.get`.

    Attributes:
        found: True when the key exists and has not expired.
        value: Stored value; empty when not found.
        metadata: Stored metadata JSON text; empty when none.
        expirationUnixMs: Absolute expiry (UTC Unix milliseconds); zero means none.
    """

    found: bool
    value: bytes
    metadata: str
    expirationUnixMs: int


class KvGetReplyOk(TypedDict):
    """``KvGetReply`` member ``ok``.

    Success: the value, when found.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the value, when found.
    """

    kind: Literal["ok"]
    value: KvGetOk


class KvGetReplyErr(TypedDict):
    """``KvGetReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


KvGetReply = Union[
    KvGetReplyOk,
    KvGetReplyErr,
]
"""Result union of `KvNamespace.get`."""


class KvPutOk(TypedDict):
    """Success payload of `KvNamespace.put`.

    Attributes:
        expirationUnixMs: Effective absolute expiry (UTC Unix milliseconds); zero means
            none.
    """

    expirationUnixMs: int


class KvDeleteOk(TypedDict):
    """Success payload of `KvNamespace.delete`.

    Attributes:
        existed: True when a live value existed before the delete.
    """

    existed: bool


class KvPutReplyOk(TypedDict):
    """``KvPutReply`` member ``ok``.

    Success: the write is durable.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the write is durable.
    """

    kind: Literal["ok"]
    value: KvPutOk


class KvPutReplyErr(TypedDict):
    """``KvPutReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


KvPutReply = Union[
    KvPutReplyOk,
    KvPutReplyErr,
]
"""Result union of `KvNamespace.put`."""


class KvDeleteReplyOk(TypedDict):
    """``KvDeleteReply`` member ``ok``.

    Success: the key is gone (or never existed).

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the key is gone (or never existed).
    """

    kind: Literal["ok"]
    value: KvDeleteOk


class KvDeleteReplyErr(TypedDict):
    """``KvDeleteReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


KvDeleteReply = Union[
    KvDeleteReplyOk,
    KvDeleteReplyErr,
]
"""Result union of `KvNamespace.delete`."""


class KvListOptions(TypedDict):
    """Page request for `KvNamespace.list`.

    Attributes:
        prefix: Only keys starting with this prefix; empty means all.
        cursor: Opaque cursor from a previous page; empty means the first page.
        limit: Most keys to return; zero means the host default, at most `maxListPage`.
    """

    prefix: str
    cursor: str
    limit: int


class KvListKey(TypedDict):
    """One key on a `KvNamespace.list` page.

    Attributes:
        key: Key name.
        metadata: Stored metadata JSON text; empty when none.
        expirationUnixMs: Absolute expiry (UTC Unix milliseconds); zero means none.
    """

    key: str
    metadata: str
    expirationUnixMs: int


class KvListOk(TypedDict):
    """Success payload of `KvNamespace.list`.

    Attributes:
        keys: Keys in lexical order.
        listComplete: True when no further page exists.
        cursor: Cursor for the next page; empty when `listComplete`.
    """

    keys: list[KvListKey]
    listComplete: bool
    cursor: str


class KvListReplyOk(TypedDict):
    """``KvListReply`` member ``ok``.

    Success: one page of keys.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one page of keys.
    """

    kind: Literal["ok"]
    value: KvListOk


class KvListReplyErr(TypedDict):
    """``KvListReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


KvListReply = Union[
    KvListReplyOk,
    KvListReplyErr,
]
"""Result union of `KvNamespace.list`."""


class ScheduledController(TypedDict):
    """One fire of a `[triggers] crons` schedule (`scheduled(controller)`).

    Attributes:
        cron: The schedule that fired, normalized to single spaces (`0 3 * * MON`).
        scheduledTime: Fire time, UTC unix milliseconds (always a whole minute). Retries
            of one fire keep it.
        fireId: Durable id of this fire, stable across retries; dedupe on it.
        attempt: Failure attempt, starting at 1.
        deadlineUnixMs: UTC unix milliseconds. Host fence/lease is authoritative; this
            hint must not outlive the fence.
    """

    cron: str
    scheduledTime: int
    fireId: str
    attempt: int
    deadlineUnixMs: int


class MetadataLookupParams(TypedDict):
    """Params of `MetadataProvider.lookup`.

    Attributes:
        identifiers: Identifiers naming the wanted edition or work; never empty. A
            provider ignores schemes it does not serve.
        marketplace: Marketplace to answer for (`us`, `uk`, ...) when the provider is
            regional. Omitted when absent (wire zero value).
        limit: Most records to return; the provider picks a small default when absent.
            Omitted when absent (wire zero value).
    """

    identifiers: list[Identifier]
    marketplace: NotRequired[str]
    limit: NotRequired[int]


class MetadataSearchParams(TypedDict):
    """Params of `MetadataProvider.search`.

    Attributes:
        title: Title to match; never empty.
        author: Author display name to narrow the match when known. Omitted when absent
            (wire zero value).
        language: Preferred content language code when known. Omitted when absent (wire
            zero value).
        marketplace: Marketplace to answer for when the provider is regional. Omitted
            when absent (wire zero value).
        limit: Most records to return; the provider picks a small default when absent.
            Omitted when absent (wire zero value).
    """

    title: str
    author: NotRequired[str]
    language: NotRequired[str]
    marketplace: NotRequired[str]
    limit: NotRequired[int]


class MetadataChaptersParams(TypedDict):
    """Params of `MetadataProvider.chapters`.

    Attributes:
        identifiers: Identifiers of one edition (an ASIN for Audnexus); never empty.
        marketplace: Marketplace to answer for when the provider is regional. Omitted
            when absent (wire zero value).
    """

    identifiers: list[Identifier]
    marketplace: NotRequired[str]


class MetadataRecord(TypedDict):
    """One edition as a metadata provider describes it: the same structured
    fields as `CatalogHit`, without store pricing.

    Attributes:
        title: Primary title.
        subtitle: Subtitle when distinct from `title`. Omitted when absent (wire zero
            value).
        description: Long description / blurb when known. Omitted when absent (wire zero
            value).
        coverUrl: Cover image URL. Omitted when absent (wire zero value).
        url: The provider's page for this record. Omitted when absent (wire zero value).
        publisher: Publisher name when known. Omitted when absent (wire zero value).
        lengthMinutes: Runtime in whole minutes when known. Omitted when absent (wire
            zero value).
        publishedAt: Publication date string as the provider prints it. Omitted when
            absent (wire zero value).
        categories: Category / genre labels as a single string when known. Omitted when
            absent (wire zero value).
        language: Content language code when known. Omitted when absent (wire zero
            value).
        abridgement: Whether the edition is abridged when the provider says so.
        workKind: Kind of work the record describes.
        expressionKind: How the work is realised in this edition.
        mediaKind: Carrier of this edition.
        identifiers: External identifiers (ISBN, ASIN, OpenLibrary ids).
        credits: Credited contributors in billing order.
        seriesEntries: Series the work belongs to.
        ratingOverall: Aggregate rating when the provider publishes one. Omitted when
            absent (wire zero value).
        ratingCount: Number of ratings behind `ratingOverall` when known. Omitted when
            absent (wire zero value).
    """

    title: str
    subtitle: NotRequired[str]
    description: NotRequired[str]
    coverUrl: NotRequired[str]
    url: NotRequired[str]
    publisher: NotRequired[str]
    lengthMinutes: NotRequired[int]
    publishedAt: NotRequired[str]
    categories: NotRequired[str]
    language: NotRequired[str]
    abridgement: Abridgement
    workKind: WorkKind
    expressionKind: ExpressionKind
    mediaKind: MediaKind
    identifiers: list[Identifier]
    credits: list[Credit]
    seriesEntries: list[SeriesEntry]
    ratingOverall: NotRequired[float]
    ratingCount: NotRequired[int]


class MetadataRecords(TypedDict):
    """Success payload of `MetadataProvider.lookup` / `search`.

    Attributes:
        records: Records in the provider's relevance order; empty when nothing matched.
    """

    records: list[MetadataRecord]


class MetadataRecordsReplyOk(TypedDict):
    """``MetadataRecordsReply`` member ``ok``.

    Success: matching records.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: matching records.
    """

    kind: Literal["ok"]
    value: MetadataRecords


class MetadataRecordsReplyErr(TypedDict):
    """``MetadataRecordsReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


MetadataRecordsReply = Union[
    MetadataRecordsReplyOk,
    MetadataRecordsReplyErr,
]
"""Result union of `MetadataProvider.lookup` / `search`."""


class MetadataChapter(TypedDict):
    """One chapter in a provider's chapter layout. Chapters are listed in
    pre-order: a chapter with `depth` n + 1 is nested under the nearest
    earlier chapter with `depth` n.

    Attributes:
        title: Chapter title.
        startMs: Start offset in milliseconds from the beginning of the title.
        lengthMs: Length in milliseconds when known. Omitted when absent (wire zero
            value).
        depth: Nesting depth; 0 for a top-level chapter.
    """

    title: str
    startMs: int
    lengthMs: NotRequired[int]
    depth: int


class MetadataChapters(TypedDict):
    """Success payload of `MetadataProvider.chapters`.

    Attributes:
        found: False when the provider has no chapter layout for the edition.
        chapters: Chapters in pre-order; empty when not `found`.
        brandIntroMs: Length of the store's spoken brand intro in milliseconds when
            known. Omitted when absent (wire zero value).
        brandOutroMs: Length of the store's spoken brand outro in milliseconds when
            known. Omitted when absent (wire zero value).
        runtimeMs: Total runtime the layout was measured against, in milliseconds.
            Omitted when absent (wire zero value).
        accurate: Whether the provider marks the offsets as exact rather than estimated.
    """

    found: bool
    chapters: list[MetadataChapter]
    brandIntroMs: NotRequired[int]
    brandOutroMs: NotRequired[int]
    runtimeMs: NotRequired[int]
    accurate: bool


class MetadataChaptersReplyOk(TypedDict):
    """``MetadataChaptersReply`` member ``ok``.

    Success: chapter layout or not-found marker.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: chapter layout or not-found marker.
    """

    kind: Literal["ok"]
    value: MetadataChapters


class MetadataChaptersReplyErr(TypedDict):
    """``MetadataChaptersReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


MetadataChaptersReply = Union[
    MetadataChaptersReplyOk,
    MetadataChaptersReplyErr,
]
"""Result union of `MetadataProvider.chapters`."""


class CoverFetchParams(TypedDict):
    """Params of `CoverFetcher.fetchCovers`.

    Attributes:
        urls: Absolute `https://` URLs to fetch, at most `maxCoverUrls`; never empty.
            The host has already checked each one against its URL policy.
        maxBytes: Largest body, in bytes, the host accepts for one image; at most
            `maxCoverBytes`. The sink refuses a larger body, which fails that URL only.
        conditions: Conditional request headers per URL, in the same order as `urls`,
            from the host's cached copy. Empty, or shorter than `urls`, means the
            remaining URLs are fetched unconditionally.
    """

    urls: list[str]
    maxBytes: int
    conditions: list[CoverCondition]


class CoverCondition(TypedDict):
    """Validators the host already holds for one URL, sent as conditional request
    headers so an unchanged image costs a `304` instead of its body.

    Attributes:
        ifNoneMatch: `If-None-Match` value: the `ETag` the host stored, verbatim; empty
            for none. Omitted when absent (wire zero value).
        ifModifiedSince: `If-Modified-Since` value: the `Last-Modified` the host stored,
            verbatim; empty for none. Omitted when absent (wire zero value).
    """

    ifNoneMatch: NotRequired[str]
    ifModifiedSince: NotRequired[str]


class CoverImage(TypedDict):
    """What happened to one requested URL.

    Attributes:
        url: The requested URL, verbatim.
        stored: Whether the body was put into the sink under this URL's index.
        contentType: `Content-Type` the server sent. Advisory: the host sniffs the
            bytes. Omitted when absent (wire zero value).
        error: Why nothing was stored (HTTP status, size cap, timeout, refused host).
            Omitted when absent (wire zero value).
        notModified: The server answered `304 Not Modified` to the URL's conditions:
            nothing was put into the sink and the host's copy is still current.
        etag: `ETag` the server sent, verbatim. Advisory: the host parses and caps it.
            Omitted when absent (wire zero value).
        lastModified: `Last-Modified` the server sent, verbatim. Omitted when absent
            (wire zero value).
        cacheControl: `Cache-Control` the server sent, verbatim (several lines joined
            with `, `). Omitted when absent (wire zero value).
        expires: `Expires` the server sent, verbatim. Omitted when absent (wire zero
            value).
        date: `Date` the server sent, verbatim. Omitted when absent (wire zero value).
        age: `Age` the server (or a cache in front of it) sent, verbatim. Omitted when
            absent (wire zero value).
    """

    url: str
    stored: bool
    contentType: NotRequired[str]
    error: NotRequired[str]
    notModified: bool
    etag: NotRequired[str]
    lastModified: NotRequired[str]
    cacheControl: NotRequired[str]
    expires: NotRequired[str]
    date: NotRequired[str]
    age: NotRequired[str]


class CoverImages(TypedDict):
    """Success payload of `CoverFetcher.fetchCovers`.

    Attributes:
        images: One entry per requested URL, in request order.
    """

    images: list[CoverImage]


class CoverImagesReplyOk(TypedDict):
    """``CoverImagesReply`` member ``ok``.

    Success: one entry per requested URL.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: one entry per requested URL.
    """

    kind: Literal["ok"]
    value: CoverImages


class CoverImagesReplyErr(TypedDict):
    """``CoverImagesReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


CoverImagesReply = Union[
    CoverImagesReplyOk,
    CoverImagesReplyErr,
]
"""Result union of `CoverFetcher.fetchCovers`."""


class MetadataFileRecord(TypedDict):
    """What one metadata file says about the edition it sits beside. A field the
    file does not state is left out of `present` and means "unknown", never
    "cleared"; a field in `present` with an empty value means the file states
    that it is empty. The host checks every value against the `maxMetadataFile*`
    caps and refuses a record that states a field its dialect does not carry.

    Attributes:
        present: Fields the file states, each at most once; at most the fields the
            dialect declares.
        title: Primary title. Omitted when absent (wire zero value).
        subtitle: Subtitle when distinct from `title`. Omitted when absent (wire zero
            value).
        description: Long description / blurb; at most
            `maxMetadataFileDescriptionBytes`. Omitted when absent (wire zero value).
        publisher: Publisher name. Omitted when absent (wire zero value).
        publishedAt: Publication date as `YYYY`, `YYYY-MM` or `YYYY-MM-DD`. Omitted when
            absent (wire zero value).
        language: Content language as the file prints it (a code or a name). Omitted
            when absent (wire zero value).
        genres: Genre labels in file order.
        tags: Free tags in file order.
        abridgement: Whether the edition is abridged; `unknown` when the file does not
            say.
        expressionKind: How the work is realised; `unspecified` when the file does not
            say.
        identifiers: External identifiers the file carries.
        credits: Credited contributors in billing order, only in the roles `creditRoles`
            names.
        creditRoles: Credit roles the file states, each at most once. A role listed here
            with no credit means the file states nobody in that role; a role left out is
            unknown. Empty unless `present` holds `credits`.
        seriesEntries: Series the work belongs to.
        chapters: Chapter layout in pre-order (see `MetadataChapter`); at most
            `maxMetadataFileChapters`, starts never decreasing.
        durationMs: Running time in milliseconds. Omitted when absent (wire zero value).
    """

    present: list[MetadataField]
    title: NotRequired[str]
    subtitle: NotRequired[str]
    description: NotRequired[str]
    publisher: NotRequired[str]
    publishedAt: NotRequired[str]
    language: NotRequired[str]
    genres: list[str]
    tags: list[str]
    abridgement: Abridgement
    expressionKind: ExpressionKind
    identifiers: list[Identifier]
    credits: list[Credit]
    creditRoles: list[CreditRole]
    seriesEntries: list[SeriesEntry]
    chapters: list[MetadataChapter]
    durationMs: NotRequired[int]


class MetadataFileParseParams(TypedDict):
    """Params of `MetadataFiles.parse`.

    Attributes:
        dialect: Dialect id from the manifest's `[[metadata_files.dialects]]` that the
            host routed the file to.
        fileName: The file's name without any folder (`metadata.json`, `Book.opf`).
        bytes: The whole file; at most `maxMetadataFileBytes`.
    """

    dialect: str
    fileName: str
    bytes: bytes


class MetadataFileParsed(TypedDict):
    """Success payload of `MetadataFiles.parse`.

    Attributes:
        record: What the file says.
    """

    record: MetadataFileRecord


class MetadataFileParseReplyOk(TypedDict):
    """``MetadataFileParseReply`` member ``ok``.

    Success: the parsed record.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the parsed record.
    """

    kind: Literal["ok"]
    value: MetadataFileParsed


class MetadataFileParseReplyErr(TypedDict):
    """``MetadataFileParseReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


MetadataFileParseReply = Union[
    MetadataFileParseReplyOk,
    MetadataFileParseReplyErr,
]
"""Result union of `MetadataFiles.parse`. A file the dialect cannot read is
`invalid_params`, so the host records it as unreadable and moves on.
"""


class MetadataAudioFile(TypedDict):
    """One audio file of the edition a sidecar describes, for formats that name
    their audio (a CUE sheet's `FILE` lines).

    Attributes:
        name: The file's name without any folder, as it sits next to the sidecar.
        durationMs: Running time in milliseconds; 0 when the host does not know it.
    """

    name: str
    durationMs: int


class MetadataFileRenderParams(TypedDict):
    """Params of `MetadataFiles.render`.

    Attributes:
        dialect: Dialect id from the manifest; one that declares `writes = true`.
        record: Values to write. Only the fields in `fields` are read from it.
        fields: Fields to set, each at most once and all declared by the dialect. A
            listed field with an empty value removes it from the file.
        existing: The file as it is now; empty when the host is creating it. Everything
            the dialect does not change (unknown keys, comments, other fields) is kept.
            At most `maxMetadataFileBytes`.
        audioFiles: The edition's audio files in playback order, at most
            `maxMetadataAudioFiles`; empty when the host has none or does not know them.
            Chapter starts in `record` count from the start of the first file.
    """

    dialect: str
    record: MetadataFileRecord
    fields: list[MetadataField]
    existing: bytes
    audioFiles: list[MetadataAudioFile]


class MetadataFileRendered(TypedDict):
    """Success payload of `MetadataFiles.render`.

    Attributes:
        bytes: The new file; at most `maxMetadataFileBytes`. Empty when the dialect has
            nothing left to write; the host decides whether to remove the file.
    """

    bytes: bytes


class MetadataFileRenderReplyOk(TypedDict):
    """``MetadataFileRenderReply`` member ``ok``.

    Success: the new file.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the new file.
    """

    kind: Literal["ok"]
    value: MetadataFileRendered


class MetadataFileRenderReplyErr(TypedDict):
    """``MetadataFileRenderReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


MetadataFileRenderReply = Union[
    MetadataFileRenderReplyOk,
    MetadataFileRenderReplyErr,
]
"""Result union of `MetadataFiles.render`."""


class NotificationField(TypedDict):
    """One labelled value shown with a notification.

    Attributes:
        name: Label; at most `maxNotificationTitleBytes`.
        value: Value; at most `maxNotificationFieldBytes`.
    """

    name: str
    value: str


class NotificationMessage(TypedDict):
    """A message the host rendered for one notification. Every link is built
    from the library's public origin, never from a request.

    Attributes:
        notificationId: Host-assigned id, the same on every destination's copy; plugins
            use it as an idempotency key where the service takes one.
        notificationType: Notification type id (`job_failed`, `<plugin>:sync_failed`).
        severity: How urgent it is.
        title: Short title; at most `maxNotificationTitleBytes`.
        text: Plain-text body; at most `maxNotificationTextBytes`.
        markdown: Markdown body for chat services; at most `maxNotificationTextBytes`.
            Omitted when absent (wire zero value).
        html: Minimal HTML body for email; at most `maxNotificationHtmlBytes`. Omitted
            when absent (wire zero value).
        link: Absolute `https` (or loopback `http`) link into the library. Omitted when
            absent (wire zero value).
        fields: Labelled values, at most `maxNotificationFields`.
        groupKey: Messages with one group key may be threaded or collapsed together.
            Omitted when absent (wire zero value).
        alertAction: Alert lifecycle step; `none` for a one-off message.
        alertKey: Alert this message opens or closes; required unless `alertAction` is
            `none`. Omitted when absent (wire zero value).
        sensitive: True when the body carries a secret (an account-mail link): the
            plugin and SDKs must never log it.
    """

    notificationId: str
    notificationType: str
    severity: NotificationSeverity
    title: str
    text: str
    markdown: NotRequired[str]
    html: NotRequired[str]
    link: NotRequired[str]
    fields: list[NotificationField]
    groupKey: NotRequired[str]
    alertAction: NotificationAlertAction
    alertKey: NotRequired[str]
    sensitive: bool


class NotifyDestination(TypedDict):
    """Where a `Notifier.send` goes.

    Attributes:
        kind: Channel or personal destination.
        destinationId: Host id of the destination, stable across sends.
        addressJson: Personal address as a JSON object of the fields the manifest's
            `[[notifier.address_fields]]` declare; empty for a channel. At most
            `maxNotifierAddressBytes`. May hold secrets; never logged.
    """

    kind: NotifyDestinationKind
    destinationId: str
    addressJson: str


class NotifySendParams(TypedDict):
    """Params of `Notifier.send`.

    Attributes:
        message: What to send.
        destination: Where to send it.
    """

    message: NotificationMessage
    destination: NotifyDestination


class NotifySent(TypedDict):
    """Success payload of `Notifier.send`.

    Attributes:
        providerId: The service's id for the delivered message, when it gives one.
            Omitted when absent (wire zero value).
    """

    providerId: NotRequired[str]


class NotifySendReplyOk(TypedDict):
    """``NotifySendReply`` member ``ok``.

    Delivered.

    Attributes:
        kind: Always ``"ok"``.
        value: Delivered.
    """

    kind: Literal["ok"]
    value: NotifySent


class NotifySendReplyErr(TypedDict):
    """``NotifySendReply`` member ``err``.

    Typed failure; `rate_limited`, `unavailable`, `deadline_exceeded` and `cancelled` are retried.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `rate_limited`, `unavailable`, `deadline_exceeded` and
            `cancelled` are retried.
    """

    kind: Literal["err"]
    value: PluginError


NotifySendReply = Union[
    NotifySendReplyOk,
    NotifySendReplyErr,
]
"""Result union of `Notifier.send`."""


class NotifyValidateParams(TypedDict):
    """Params of `Notifier.validateDestination`.

    Attributes:
        addressJson: Address document as the patron entered it; at most
            `maxNotifierAddressBytes`.
    """

    addressJson: str


class NotifyValidated(TypedDict):
    """Success payload of `Notifier.validateDestination`.

    Attributes:
        addressJson: Normalised address document the host saves; at most
            `maxNotifierAddressBytes`.
        label: Short label shown to the patron (`a***@example.com`); never a secret.
            Omitted when absent (wire zero value).
    """

    addressJson: str
    label: NotRequired[str]


class NotifyValidateReplyOk(TypedDict):
    """``NotifyValidateReply`` member ``ok``.

    Accepted, normalised address.

    Attributes:
        kind: Always ``"ok"``.
        value: Accepted, normalised address.
    """

    kind: Literal["ok"]
    value: NotifyValidated


class NotifyValidateReplyErr(TypedDict):
    """``NotifyValidateReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


NotifyValidateReply = Union[
    NotifyValidateReplyOk,
    NotifyValidateReplyErr,
]
"""Result union of `Notifier.validateDestination`. An address the plugin
refuses is `invalid_params`.
"""


class NotifyRaiseParams(TypedDict):
    """Params of `NotificationRaiser.raise`.

    Attributes:
        notificationType: Type id from the plugin's `[[notifications.types]]` (without
            the plugin prefix the host adds).
        fields: Labelled values, at most `maxNotificationFields`.
        patronId: Patron the notification is for; required for a patron-audience type
            and must be inside the invocation's account scope; empty otherwise. Omitted
            when absent (wire zero value).
        linkPath: Path inside the library the notification links to (starts with `/`);
            the host prefixes the public origin. Omitted when absent (wire zero value).
        groupKey: Messages with one group key may be threaded or collapsed together.
            Omitted when absent (wire zero value).
        alertAction: Alert lifecycle step; `none` for a one-off notification.
        alertKey: Alert this raise opens or closes; required unless `alertAction` is
            `none`. Omitted when absent (wire zero value).
    """

    notificationType: str
    fields: list[NotificationField]
    patronId: NotRequired[str]
    linkPath: NotRequired[str]
    groupKey: NotRequired[str]
    alertAction: NotificationAlertAction
    alertKey: NotRequired[str]


class NotifyRaised(TypedDict):
    """Success payload of `NotificationRaiser.raise`.

    Attributes:
        notificationId: Host-assigned notification id.
    """

    notificationId: str


class NotifyRaiseReplyOk(TypedDict):
    """``NotifyRaiseReply`` member ``ok``.

    Accepted by the router.

    Attributes:
        kind: Always ``"ok"``.
        value: Accepted by the router.
    """

    kind: Literal["ok"]
    value: NotifyRaised


class NotifyRaiseReplyErr(TypedDict):
    """``NotifyRaiseReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


NotifyRaiseReply = Union[
    NotifyRaiseReplyOk,
    NotifyRaiseReplyErr,
]
"""Result union of `NotificationRaiser.raise`."""


class SocketFrameText(TypedDict):
    """``SocketFrame`` member ``text``.

    Text message, UTF-8.

    Attributes:
        kind: Always ``"text"``.
        value: Text message, UTF-8.
    """

    kind: Literal["text"]
    value: str


class SocketFrameBinary(TypedDict):
    """``SocketFrame`` member ``binary``.

    Binary message.

    Attributes:
        kind: Always ``"binary"``.
        value: Binary message.
    """

    kind: Literal["binary"]
    value: bytes


SocketFrame = Union[
    SocketFrameText,
    SocketFrameBinary,
]
"""One WebSocket message."""


class SocketHeader(TypedDict):
    """One request header on `Sockets.connect`.

    Attributes:
        name: Header name; the broker refuses hop-by-hop and WebSocket handshake names.
        value: Header value. May carry a credential: the host never logs it.
    """

    name: str
    value: str


class SocketAutoResponse(TypedDict):
    """A heartbeat the broker answers itself: when `request` arrives byte for byte,
    it sends `response` and does not wake the guest.

    Attributes:
        request: Frame to match, at most `maxSocketAutoResponseBytes`.
        response: Frame to send back, at most `maxSocketAutoResponseBytes`.
    """

    request: SocketFrame
    response: SocketFrame


class SocketConnectRequest(TypedDict):
    """Arguments of `Sockets.connect`.

    Attributes:
        url: `wss://` or `ws://` URL, at most `maxSocketUrlBytes`. The host must be on
            the plugin's network grant.
        protocols: Requested subprotocols, at most `maxSocketHeaders`.
        headers: Extra request headers, at most `maxSocketHeaders`.
        autoResponses: Heartbeats the broker answers, at most `maxSocketAutoResponses`.
        attachment: Initial attachment, at most `maxSocketAttachmentBytes`.
    """

    url: str
    protocols: list[str]
    headers: list[SocketHeader]
    autoResponses: list[SocketAutoResponse]
    attachment: bytes


class SocketInfo(TypedDict):
    """One open host-brokered socket.

    Attributes:
        socket: Host-minted socket id, stable for the socket's life.
        url: URL the socket was opened to, without credentials.
        protocol: Subprotocol the server chose; empty when none. Omitted when absent
            (wire zero value).
        attachment: Current attachment.
    """

    socket: str
    url: str
    protocol: NotRequired[str]
    attachment: bytes


class SocketConnectReplyOk(TypedDict):
    """``SocketConnectReply`` member ``ok``.

    Success: the handshake finished and the socket is open.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the handshake finished and the socket is open.
    """

    kind: Literal["ok"]
    value: SocketInfo


class SocketConnectReplyErr(TypedDict):
    """``SocketConnectReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


SocketConnectReply = Union[
    SocketConnectReplyOk,
    SocketConnectReplyErr,
]
"""Result union of `Sockets.connect`."""


class SocketList(TypedDict):
    """Success payload of `Sockets.list`.

    Attributes:
        sockets: Open sockets, oldest first.
    """

    sockets: list[SocketInfo]


class SocketListReplyOk(TypedDict):
    """``SocketListReply`` member ``ok``.

    Success: the open sockets.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the open sockets.
    """

    kind: Literal["ok"]
    value: SocketList


class SocketListReplyErr(TypedDict):
    """``SocketListReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


SocketListReply = Union[
    SocketListReplyOk,
    SocketListReplyErr,
]
"""Result union of `Sockets.list`."""


class SocketMessageReplyOk(TypedDict):
    """``SocketMessageReply`` member ``ok``.

    Success: what the guest asks of the host after the message.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: what the guest asks of the host after the message.
    """

    kind: Literal["ok"]
    value: SocketMessageOk


class SocketMessageReplyErr(TypedDict):
    """``SocketMessageReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


SocketMessageReply = Union[
    SocketMessageReplyOk,
    SocketMessageReplyErr,
]
"""Reply of `SocketHandler.message`."""


class SocketMessageOk(TypedDict):
    """What a guest asks of the host after handling one pushed message.

    Attributes:
        pollEvents: True asks the host to call `Integration.pollEvents` now, while the
            guest is awake, instead of at the next scheduled poll: the message announced
            something the poll hands over (a new user). The host ignores it for a plugin
            whose host schedules no poll.
    """

    pollEvents: bool


class SocketMessage(TypedDict):
    """Argument of `SocketHandler.message`.

    Attributes:
        socket: Socket the message arrived on.
        frame: The message.
        attachment: The socket's attachment when the message arrived.
    """

    socket: str
    frame: SocketFrame
    attachment: bytes


class SocketClosed(TypedDict):
    """Argument of `SocketHandler.closed`.

    Attributes:
        socket: Socket that closed; its id is not reused.
        code: WebSocket close code; 1006 when the connection dropped without one.
        reason: Close reason the peer sent; empty when none. Omitted when absent (wire
            zero value).
        clean: True when the close handshake completed.
        attachment: The socket's last attachment.
    """

    socket: str
    code: int
    reason: NotRequired[str]
    clean: bool
    attachment: bytes


class WebhookUrl(TypedDict):
    """Success payload of `Webhooks.url`.

    Attributes:
        url: Absolute `https://` URL; empty when the library cannot be reached from the
            Internet or webhooks are turned off, and the plugin should use its fallback.
            Omitted when absent (wire zero value).
    """

    url: NotRequired[str]


class WebhookUrlReplyOk(TypedDict):
    """``WebhookUrlReply`` member ``ok``.

    Success: the URL, or none.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the URL, or none.
    """

    kind: Literal["ok"]
    value: WebhookUrl


class WebhookUrlReplyErr(TypedDict):
    """``WebhookUrlReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


WebhookUrlReply = Union[
    WebhookUrlReplyOk,
    WebhookUrlReplyErr,
]
"""Result union of `Webhooks.url`."""


class WebhookPullRequest(TypedDict):
    """Arguments of `Webhooks.pull`.

    Attributes:
        name: `[[webhooks]]` name.
        batchSize: Most deliveries to lease, 1 to `maxWebhookPullBatch`.
        visibilityTimeoutMs: How long leased deliveries stay hidden from the next pull,
            in milliseconds, at most `maxWebhookVisibilityMs`; zero means one minute.
    """

    name: str
    batchSize: int
    visibilityTimeoutMs: int


class WebhookHeader(TypedDict):
    """One request header kept with a delivery.

    Attributes:
        name: Lowercase header name, one the row's `headers` names or `content-type`.
        value: Header value as received.
    """

    name: str
    value: str


class WebhookDelivery(TypedDict):
    """One stored request, leased to the guest.

    Attributes:
        leaseId: Lease id for `ack`, at most `maxWebhookLeaseIdBytes`; a new one each
            time the delivery is pulled.
        receivedAtUnixMs: When the host stored the request, in Unix milliseconds.
        attempt: How many times this delivery has been pulled, counting this one.
        headers: Kept request headers, in the order the row names them.
        body: Request body as received, at most the row's `max_body_bytes`.
    """

    leaseId: str
    receivedAtUnixMs: int
    attempt: int
    headers: list[WebhookHeader]
    body: bytes


class WebhookPullPage(TypedDict):
    """Success payload of `Webhooks.pull`.

    Attributes:
        deliveries: Leased deliveries, oldest first.
        missed: True when the host dropped deliveries for this webhook since the last
            pull (the waiting cap was full or a delivery expired unacknowledged): the
            guest should treat what it follows as incomplete.
    """

    deliveries: list[WebhookDelivery]
    missed: bool


class WebhookPullReplyOk(TypedDict):
    """``WebhookPullReply`` member ``ok``.

    Success: the leased deliveries.

    Attributes:
        kind: Always ``"ok"``.
        value: Success: the leased deliveries.
    """

    kind: Literal["ok"]
    value: WebhookPullPage


class WebhookPullReplyErr(TypedDict):
    """``WebhookPullReply`` member ``err``.

    Typed failure; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


WebhookPullReply = Union[
    WebhookPullReplyOk,
    WebhookPullReplyErr,
]
"""Result union of `Webhooks.pull`."""


class WebhookAckRequest(TypedDict):
    """Arguments of `Webhooks.ack`.

    Attributes:
        name: `[[webhooks]]` name.
        acks: Leases whose deliveries are done and are deleted.
        retries: Leases whose deliveries the next pull offers again.
    """

    name: str
    acks: list[str]
    retries: list[str]


class WebhookDelivered(TypedDict):
    """Argument of `WebhookHandler.delivered`.

    Attributes:
        name: `[[webhooks]]` name.
        waiting: Deliveries waiting for this webhook when the host called.
    """

    name: str
    waiting: int


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


class ExecuteStreamHead(TypedDict):
    """First frame of a streamed execute reply: what the reply is about.

    Attributes:
        operationId: Echo of the request `operationId`.
        statementCount: Number of `statement` frames that follow, one per request
            statement.
    """

    operationId: str
    statementCount: int


class ExecuteStreamStatement(TypedDict):
    """Opens one statement result inside a streamed execute reply. Its rows, if
    any, follow in `rows` frames until the next `statement`, `end` or `err`.

    Attributes:
        index: Request statement index, `0`-based and increasing by one per frame.
        columns: Result column descriptors.
        rowsAffected: Rows changed by a mutation.
    """

    index: int
    columns: list[DbColumn]
    rowsAffected: int


class ExecuteStreamFrameHead(TypedDict):
    """``ExecuteStreamFrame`` member ``head``.

    First frame.

    Attributes:
        kind: Always ``"head"``.
        value: First frame.
    """

    kind: Literal["head"]
    value: ExecuteStreamHead


class ExecuteStreamFrameStatement(TypedDict):
    """``ExecuteStreamFrame`` member ``statement``.

    Opens the next statement result.

    Attributes:
        kind: Always ``"statement"``.
        value: Opens the next statement result.
    """

    kind: Literal["statement"]
    value: ExecuteStreamStatement


class ExecuteStreamFrameRows(TypedDict):
    """``ExecuteStreamFrame`` member ``rows``.

    Rows of the open statement, in order.

    Attributes:
        kind: Always ``"rows"``.
        value: Rows of the open statement, in order.
    """

    kind: Literal["rows"]
    value: list[DbRow]


class ExecuteStreamFrameEnd(TypedDict):
    """``ExecuteStreamFrame`` member ``end``.

    Last frame: the batch committed; engine timing.

    Attributes:
        kind: Always ``"end"``.
        value: Last frame: the batch committed; engine timing.
    """

    kind: Literal["end"]
    value: DbTiming


class ExecuteStreamFrameErr(TypedDict):
    """``ExecuteStreamFrame`` member ``err``.

    Last frame: the batch failed; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Last frame: the batch failed; `code` is a `PluginErrorCode` wire string.
    """

    kind: Literal["err"]
    value: PluginError


ExecuteStreamFrame = Union[
    ExecuteStreamFrameHead,
    ExecuteStreamFrameStatement,
    ExecuteStreamFrameRows,
    ExecuteStreamFrameEnd,
    ExecuteStreamFrameErr,
]
"""One frame of a streamed execute reply (`executeStreamed`). Frames are
consecutive unpacked single-segment Cap'n Proto messages, each at most
`maxScalarBytes` unless it carries one row that alone exceeds it. The
sequence is `head`, then for each statement `statement` followed by its
`rows` frames in order, then `end`; `err` ends the sequence early.
"""


class ExecuteStreamReplyInline(TypedDict):
    """``ExecuteStreamReply`` member ``inline``.

    Success: every frame, concatenated, at most `maxScalarBytes`.

    Attributes:
        kind: Always ``"inline"``.
        value: Success: every frame, concatenated, at most `maxScalarBytes`.
    """

    kind: Literal["inline"]
    value: bytes


class ExecuteStreamReplyStream(TypedDict):
    """``ExecuteStreamReply`` member ``stream``.

    Success: the frames as a byte stream.

    Attributes:
        kind: Always ``"stream"``.
        value: Success: the frames as a byte stream.
    """

    kind: Literal["stream"]
    value: ByteSource


class ExecuteStreamReplyErr(TypedDict):
    """``ExecuteStreamReply`` member ``err``.

    Typed failure before any frame; `code` is a `PluginErrorCode` wire string.

    Attributes:
        kind: Always ``"err"``.
        value: Typed failure before any frame; `code` is a `PluginErrorCode` wire
            string.
    """

    kind: Literal["err"]
    value: PluginError


ExecuteStreamReply = Union[
    ExecuteStreamReplyInline,
    ExecuteStreamReplyStream,
    ExecuteStreamReplyErr,
]
"""Result union of `executeStreamed`. The frames (`ExecuteStreamFrame`) travel
inline when they fit one scalar, otherwise as a transferred byte stream in
`maxStreamWindowBytes` windows. The adapter counts the reply against
`maxStreamedResultBytes` and `maxStreamedResultRows` before it commits, so
results still fail closed and are never truncated.
"""


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
        maxStreamedResultBytes: Maximum encoded bytes of one `ExecuteReply` answered
            through `executeStreamed` (the sum of its frames' statement results). `0`
            means the adapter does not stream and callers stay under the inline caps.
            When set it is at least `maxAtomicResultBytes`.
        maxStreamedResultRows: Maximum rows one statement may return through
            `executeStreamed`. `0` means the adapter does not stream. When set it is at
            least `maxResultRows`.
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
    maxStreamedResultBytes: int
    maxStreamedResultRows: int


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

    async def drop_unit(self, unit_ref: str) -> EmptyReply:
        """Physically drop one provisioned binding unit (`unitRef` is adapter-owned).

        Args:
            unit_ref: Adapter-native unit (sqlite path, postgres database, D1 name).

        Returns:
            ``EmptyReply``
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

    async def execute_streamed(self, request: AdapterExecuteRequest) -> ExecuteStreamReply:
        """`execute` whose reply is framed (`ExecuteStreamFrame`) and counted against
        `maxStreamedResultBytes` / `maxStreamedResultRows` instead of the inline
        caps. One transaction, one snapshot; `unsupported` when the adapter
        advertises no streamed budget.

        Args:
            request: Proven statements plus isolation and receipt.

        Returns:
            ``ExecuteStreamReply``
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

    async def limits(self) -> DbCapabilitiesReply:
        """Negotiated limits of this session, so guests size requests and reads
        before sending instead of discovering caps from errors.

        Returns:
            ``DbCapabilitiesReply``
        """
        ...

    async def execute_streamed(self, request: ExecuteRequest) -> ExecuteStreamReply:
        """`execute` whose reply is framed (`ExecuteStreamFrame`) and counted against
        the session's `maxStreamedResultBytes` / `maxStreamedResultRows`, so a
        result past the inline caps still arrives whole. `unsupported` when the
        session advertises no streamed budget.

        Args:
            request: Statements, binds, and deadline.

        Returns:
            ``ExecuteStreamReply``
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
    "ObjectChangeKind",
    "SettingNetworkKind",
    "SettingGrantState",
    "PluginErrorCode",
    "Entrypoint",
    "PortalAuthMode",
    "CliArgKind",
    "WorkKind",
    "ExpressionKind",
    "MediaKind",
    "IdentifierScheme",
    "CreditRole",
    "CreditLevel",
    "FetchedFileRole",
    "CatalogSort",
    "CatalogField",
    "PriceLookup",
    "Abridgement",
    "MetadataField",
    "NotificationSeverity",
    "NotificationAudience",
    "NotificationAlertAction",
    "NotifyDestinationKind",
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
    "MoveOptions",
    "MoveResult",
    "KeyPair",
    "MoveManyTargetPairs",
    "MoveManyTargetPrefix",
    "MoveManyTarget",
    "MoveManyRequest",
    "KeyOutcome",
    "MoveManyPage",
    "DeleteManyResult",
    "HeadManyResult",
    "TranslateKeysResult",
    "ChangesRequest",
    "ObjectChange",
    "ChangesPage",
    "PluginDescribe",
    "OidcClientTemplate",
    "OidcClientsOk",
    "OidcClientsReplyOk",
    "OidcClientsReplyErr",
    "OidcClientsReply",
    "ExtensibleConfig",
    "Bindings",
    "NetworkGrant",
    "SettingNetworkGrant",
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
    "EventReadRequest",
    "EventReadPage",
    "EventReadReplyOk",
    "EventReadReplyErr",
    "EventReadReply",
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
    "MoveReplyOk",
    "MoveReplyErr",
    "MoveReply",
    "MoveManyReplyOk",
    "MoveManyReplyErr",
    "MoveManyReply",
    "DeleteManyReplyOk",
    "DeleteManyReplyErr",
    "DeleteManyReply",
    "HeadManyReplyOk",
    "HeadManyReplyErr",
    "HeadManyReply",
    "TranslateKeysReplyOk",
    "TranslateKeysReplyErr",
    "TranslateKeysReply",
    "ChangesReplyOk",
    "ChangesReplyErr",
    "ChangesReply",
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
    "ScheduledHandler",
    "QueueConsumer",
    "QueueProducer",
    "NamedQueue",
    "KvNamespace",
    "NamedKv",
    "Sockets",
    "SocketHandler",
    "Webhooks",
    "WebhookHandler",
    "ContentSource",
    "RemoteLibrary",
    "PluginCli",
    "Oidc",
    "MetadataProvider",
    "CoverFetcher",
    "MetadataFiles",
    "Notifier",
    "NotificationRaiser",
    "EventConsumerSpec",
    "PluginCapabilities",
    "CatalogCapabilities",
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
    "MediaSupport",
    "Identifier",
    "Credit",
    "SeriesEntry",
    "ScanBook",
    "ScanPage",
    "ScanReplyOk",
    "ScanReplyErr",
    "ScanReply",
    "FetchOptions",
    "FetchTitleParams",
    "FetchedFile",
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
    "RevokeAccountParams",
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
    "PricedRecord",
    "PurchaseHintReplyOk",
    "PurchaseHintReplyErr",
    "PurchaseHintReply",
    "PurchaseHintsParams",
    "PurchaseHintsResult",
    "PurchaseHintsReplyOk",
    "PurchaseHintsReplyErr",
    "PurchaseHintsReply",
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
    "QueueMessage",
    "QueueBatch",
    "QueueRetry",
    "QueueResultAck",
    "QueueResultRetry",
    "QueueResult",
    "QueueBatchOk",
    "QueueBatchReplyOk",
    "QueueBatchReplyErr",
    "QueueBatchReply",
    "QueueSendMessage",
    "QueueSendOk",
    "QueueSendReplyOk",
    "QueueSendReplyErr",
    "QueueSendReply",
    "KvPutOptions",
    "KvGetOk",
    "KvGetReplyOk",
    "KvGetReplyErr",
    "KvGetReply",
    "KvPutOk",
    "KvDeleteOk",
    "KvPutReplyOk",
    "KvPutReplyErr",
    "KvPutReply",
    "KvDeleteReplyOk",
    "KvDeleteReplyErr",
    "KvDeleteReply",
    "KvListOptions",
    "KvListKey",
    "KvListOk",
    "KvListReplyOk",
    "KvListReplyErr",
    "KvListReply",
    "ScheduledController",
    "MetadataLookupParams",
    "MetadataSearchParams",
    "MetadataChaptersParams",
    "MetadataRecord",
    "MetadataRecords",
    "MetadataRecordsReplyOk",
    "MetadataRecordsReplyErr",
    "MetadataRecordsReply",
    "MetadataChapter",
    "MetadataChapters",
    "MetadataChaptersReplyOk",
    "MetadataChaptersReplyErr",
    "MetadataChaptersReply",
    "CoverFetchParams",
    "CoverCondition",
    "CoverImage",
    "CoverImages",
    "CoverImagesReplyOk",
    "CoverImagesReplyErr",
    "CoverImagesReply",
    "MetadataFileRecord",
    "MetadataFileParseParams",
    "MetadataFileParsed",
    "MetadataFileParseReplyOk",
    "MetadataFileParseReplyErr",
    "MetadataFileParseReply",
    "MetadataAudioFile",
    "MetadataFileRenderParams",
    "MetadataFileRendered",
    "MetadataFileRenderReplyOk",
    "MetadataFileRenderReplyErr",
    "MetadataFileRenderReply",
    "NotificationField",
    "NotificationMessage",
    "NotifyDestination",
    "NotifySendParams",
    "NotifySent",
    "NotifySendReplyOk",
    "NotifySendReplyErr",
    "NotifySendReply",
    "NotifyValidateParams",
    "NotifyValidated",
    "NotifyValidateReplyOk",
    "NotifyValidateReplyErr",
    "NotifyValidateReply",
    "NotifyRaiseParams",
    "NotifyRaised",
    "NotifyRaiseReplyOk",
    "NotifyRaiseReplyErr",
    "NotifyRaiseReply",
    "SocketFrameText",
    "SocketFrameBinary",
    "SocketFrame",
    "SocketHeader",
    "SocketAutoResponse",
    "SocketConnectRequest",
    "SocketInfo",
    "SocketConnectReplyOk",
    "SocketConnectReplyErr",
    "SocketConnectReply",
    "SocketList",
    "SocketListReplyOk",
    "SocketListReplyErr",
    "SocketListReply",
    "SocketMessageReplyOk",
    "SocketMessageReplyErr",
    "SocketMessageReply",
    "SocketMessageOk",
    "SocketMessage",
    "SocketClosed",
    "WebhookUrl",
    "WebhookUrlReplyOk",
    "WebhookUrlReplyErr",
    "WebhookUrlReply",
    "WebhookPullRequest",
    "WebhookHeader",
    "WebhookDelivery",
    "WebhookPullPage",
    "WebhookPullReplyOk",
    "WebhookPullReplyErr",
    "WebhookPullReply",
    "WebhookAckRequest",
    "WebhookDelivered",
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
    "ExecuteStreamHead",
    "ExecuteStreamStatement",
    "ExecuteStreamFrameHead",
    "ExecuteStreamFrameStatement",
    "ExecuteStreamFrameRows",
    "ExecuteStreamFrameEnd",
    "ExecuteStreamFrameErr",
    "ExecuteStreamFrame",
    "ExecuteStreamReplyInline",
    "ExecuteStreamReplyStream",
    "ExecuteStreamReplyErr",
    "ExecuteStreamReply",
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
