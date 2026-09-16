"""GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.

Python projection of the JSON payload contracts declared in
``crates/bookclerk-plugin-abi/schema/plugin.capnp`` (the payloads carried
inside ``Text`` fields of the Cap'n Proto ABI: ``describe().metadataJson``,
role ``paramsJson``, and ``cliInvoke`` params/results). TypedDict keys are
the literal JSON keys (camelCase).
"""

from __future__ import annotations

from typing import Any, Literal, NotRequired, TypedDict

JsonValue = Any
"""Arbitrary JSON value carried inside a payload field."""

JsonObject = dict[str, Any]
"""Loose JSON object used for config blobs and structured payloads."""

PLUGIN_ERROR_CODES: tuple[str, ...] = ("invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict")
"""Stable `PluginError.code` strings. Unknown future codes are forwarded as-is; SDKs surface them as a local `unknown` while keeping the raw wire code."""

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
"""Union of known `PluginErrorCode` wire strings."""


class PluginMetadata(TypedDict):
    """Identity extras carried as JSON in `describe().metadataJson`: portal auth, brand colors, config option discovery, and an embedded CLI schema.

    Attributes:
        apiVersion: ABI version the guest speaks; must equal `apiVersion`.
        id: Stable plugin id matching `plugin.toml` / install directory
            name.
        kind: Plugin kind: "source", "integration", "output", or "database".
        displayName: Human-readable name for UI lists; omitted when absent.
        capabilities: Declared capability method names the guest implements
            (e.g. "health", "login", "fetchTitle").
        portalAuthMode: Portal Accounts connect mode: "oauth" or "password".
        passwordEnvVar: Optional env var name operators may set for password
            helpers; never required for Accounts UI connect.
        aliases: Alternate ids accepted for config / CLI targeting; omitted
            when empty.
        sortKey: Optional UI sort weight among peers of the same kind.
        brand: Portal brand colors and icon URL for Accounts / library
            chrome.
        configOptions: Discoverable config option groups for source UIs.
        cli: Optional embedded CLI schema (same shape as `cliDescribe`).
    """

    apiVersion: int
    id: str
    kind: str
    displayName: NotRequired[str]
    capabilities: NotRequired[list[str]]
    portalAuthMode: NotRequired[str]
    passwordEnvVar: NotRequired[str]
    aliases: NotRequired[list[str]]
    sortKey: NotRequired[int]
    brand: NotRequired[Brand]
    configOptions: NotRequired[list[ConfigOption]]
    cli: NotRequired[CliSchema]


class Brand(TypedDict):
    """Portal brand crossing the RPC boundary. Distinct from `plugin.toml` `logo`: `iconUrl` is the live URL or data URI the SPA renders.

    Attributes:
        id: Brand id (often matches the plugin id).
        name: Display name shown next to the brand swatch.
        bg: Background CSS color (hex or named).
        fg: Foreground CSS color for text on `bg`.
        accent: Accent CSS color for highlights / CTAs.
        iconUrl: Icon URL or data URI for the portal.
    """

    id: str
    name: str
    bg: str
    fg: str
    accent: str
    iconUrl: str


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
    """Declared plugin CLI surface (`cliDescribe` / metadata `cli` / `plugin.toml`).

    Attributes:
        commands: Commands exposed as `bookclerk plugins <id> <command>
            ...`.
    """

    commands: NotRequired[list[CliCommandSpec]]


class CliCommandSpec(TypedDict):
    """One plugin CLI command under `CliSchema`.

    Attributes:
        name: Command verb after the plugin id (for example "ping").
        about: Short help text for `--help`; omitted when absent.
        args: Argument / flag specs for this command (default empty).
    """

    name: str
    about: NotRequired[str]
    args: NotRequired[list[CliArgSpec]]


CLI_ARG_KINDS: tuple[str, ...] = ("string", "bool", "int", "path")
"""Value kind for a `CliArgSpec` (wire lowercase: "string" / "bool" / ...)."""

CliArgKind = Literal["string", "bool", "int", "path"]
"""Union of known `CliArgKind` wire strings."""


class CliArgSpec(TypedDict):
    """One CLI argument or flag under a `CliCommandSpec`.

    Attributes:
        name: Internal arg name used as the key in `CliInvokeParams.args`.
        long: Long flag without leading dashes (e.g. "message" ->
            `--message`).
        short: Optional short flag character (e.g. "m" -> `-m`).
        kind: Parsed value kind (default "string").
        required: When true, the host rejects invoke if the arg is missing.
        default: Default string form when the operator omits the arg.
        about: Help text for this arg; omitted when absent.
        positional: When true, the arg is positional rather than a flagged
            option.
    """

    name: str
    long: NotRequired[str]
    short: NotRequired[str]
    kind: NotRequired[CliArgKind]
    required: NotRequired[bool]
    default: NotRequired[str]
    about: NotRequired[str]
    positional: NotRequired[bool]


class CliInvokeParams(TypedDict):
    """Params JSON for `cliInvoke`.

    Attributes:
        command: Command name matching a `CliCommandSpec.name`.
        args: Named argument values (keys match `CliArgSpec.name`; default
            `{}`).
    """

    command: str
    args: NotRequired[JsonValue]


class CliInvokeResult(TypedDict):
    """Result JSON for `cliInvoke`.

    Attributes:
        exitCode: Process-style exit code (0 = success).
        stdout: Captured standard output text.
        stderr: Captured standard error text.
        json: Optional structured payload for machine consumers; omitted
            when absent.
    """

    exitCode: NotRequired[int]
    stdout: NotRequired[str]
    stderr: NotRequired[str]
    json: NotRequired[JsonValue]


class DatabaseAdapterConfig(TypedDict):
    """Author-facing database adapter configuration carried in `DatabaseContext.config` (mediaType `application/vnd.bookclerk.db-adapter-config+json`). This is the generic bootstrap mechanism for third-party adapters: the operator's granted `[database.<id>]` table plus the scoped writable data dir. First-party host-managed adapters receive host-private connect params instead.

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin
            (`.../plugins/<id>/data`).
        config: Granted plugin settings (operator `[database.<id>]` table)
            as a JSON object; `{}` when the operator configured nothing.
        binding: Named plugin database binding this open serves; omitted for
            the primary library open. Adapters advertising
            `DbCapabilities.pluginDatabases` must serve each binding from
            its own isolated database.
        instanceId: Host-issued opaque instance id for this (owner plugin,
            binding) pair. Collision-resistant and stable across re-opens.
            Omitted for the primary library open. Third-party adapters must
            key isolated databases on this value rather than `binding` alone
            (two plugins may both declare `DB`).
    """

    pluginDataDir: str
    config: NotRequired[JsonValue]
    binding: NotRequired[str]
    instanceId: NotRequired[str]


class HealthResult(TypedDict):
    """JSON health payload for guests that report identity alongside liveness. Role-level `health` RPCs return the typed `HealthOk` instead.

    Attributes:
        ok: When true, the guest considers itself healthy enough for
            traffic.
        id: Plugin id echo; omitted when the guest does not duplicate
            identity.
        enabled: Whether the guest believes it is enabled in config; omitted
            when unknown.
        detail: Short human detail for CLI / UI status lines; omitted when
            absent.
    """

    ok: NotRequired[bool]
    id: NotRequired[str]
    enabled: NotRequired[bool]
    detail: NotRequired[str]


class DiagnoseResult(TypedDict):
    """JSON result of `diagnose`. Each line is printed by `bookclerk plugins diagnose` / the control plane.

    Attributes:
        lines: Human-readable probe lines (default empty).
    """

    lines: NotRequired[list[str]]


class LoginParams(TypedDict):
    """Params JSON for `ContentSource.login`. Password sources fill email/password; OAuth sources use callback / external fields. There is no files-dir root or library DB path -- only `pluginDataDir`.

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin only
            (`.../plugins/<id>/data`).
        marketplace: Marketplace / locale for the storefront (default empty
            -> guest default).
        label: Optional operator label stored on the account row.
        email: Account email / username for password logins; omitted for
            pure OAuth.
        password: Account password for password logins; never logged;
            omitted for OAuth.
        force: When true, overwrite an existing sealed credential for this
            account.
        callbackBind: Optional bind address for OAuth callback servers
            (`host:port`). Ignored when `callbackIpc` is set (host owns the
            TCP listener).
        callbackIpc: Host-owned callback IPC endpoint the guest must connect
            to. When set (with `callbackPublicBase`), the guest must not
            bind a TCP listener.
        callbackPublicBase: Public base URL for the host TCP listener, e.g.
            `http://127.0.0.1:12345`.
        external: When true, use external / paste-redirect OAuth instead of
            a local callback server.
        responseUrl: Pre-supplied OAuth redirect URL (paste flow); omitted
            otherwise.
        showQr: Prefer QR output when the guest supports it.
        timeoutSecs: Seconds to wait for OAuth callback capture; guest
            default when omitted.
        extra: Store-specific knobs as a JSON object; guests may ignore
            unknowns.
    """

    pluginDataDir: str
    marketplace: NotRequired[str]
    label: NotRequired[str]
    email: NotRequired[str]
    password: NotRequired[str]
    force: NotRequired[bool]
    callbackBind: NotRequired[str]
    callbackIpc: NotRequired[str]
    callbackPublicBase: NotRequired[str]
    external: NotRequired[bool]
    responseUrl: NotRequired[str]
    showQr: NotRequired[bool]
    timeoutSecs: NotRequired[int]
    extra: NotRequired[JsonValue]


LoginStartParams = LoginParams
"""Params JSON for `ContentSource.loginStart` -- same shape as `LoginParams`."""


class LoginCompleteParams(TypedDict):
    """Params JSON for `ContentSource.loginComplete`.

    Attributes:
        sessionId: Session id previously returned by `loginStart`.
    """

    sessionId: str


class ScanParams(TypedDict):
    """Params JSON for `ContentSource.scan`. Host injects sealed credentials so the plugin does not need a private credential store under `pluginDataDir`.

    Attributes:
        pluginDataDir: Scoped plugin data directory.
        accounts: Account ids to scan; empty means all scan-enabled
            accounts.
        pageSize: Storefront page size (default 50).
        importEpisodes: When true, import podcast/episode-style rows
            (default true).
        importPlusTitles: When true, import Plus/catalog entitlement titles
            (default true).
        credentials: Host-loaded credential blobs keyed by account id (JSON
            object).
    """

    pluginDataDir: str
    accounts: NotRequired[list[str]]
    pageSize: NotRequired[int]
    importEpisodes: NotRequired[bool]
    importPlusTitles: NotRequired[bool]
    credentials: NotRequired[JsonValue]


class FetchTitleParams(TypedDict):
    """Params JSON for `ContentSource.fetchTitle`. Plugin writes media under `cacheDir` and returns plain (DRM-free) paths. Host injects credentials; guests must not open `library.db` or `master.key`.

    Attributes:
        pluginDataDir: Scoped plugin data directory.
        accountId: Account whose credentials apply.
        titleId: Library / storefront title id to download.
        cacheDir: Absolute path the guest should write media into (jail-
            granted TMPDIR).
        credentials: Host-loaded credential blob for this account; omitted
            when unavailable.
        sourceConfig: Opaque plugin table from `[sources.<id>]`.
        download: Host acquire/download options (JSON object matching host
            DownloadOptions).
    """

    pluginDataDir: str
    accountId: str
    titleId: str
    cacheDir: str
    credentials: NotRequired[JsonValue]
    sourceConfig: NotRequired[JsonValue]
    download: NotRequired[JsonValue]


class SearchCatalogParams(TypedDict):
    """Params JSON for `ContentSource.searchCatalog`.

    Attributes:
        query: Free-text search query.
        region: Storefront region / marketplace code (default empty -> guest
            default).
        limit: Maximum hits to return (default 20).
        page: 1-based page for storefronts that page (default 1).
        sort: Sort key: "relevance" / "popularity" / "rating" / "title" /
            "author".
        field: Optional facet ("author" / "narrator" / "series" / "genre").
        language: Preferred content language (soft-prioritize; e.g. "en").
    """

    query: str
    region: NotRequired[str]
    limit: NotRequired[int]
    page: NotRequired[int]
    sort: NotRequired[str]
    field: NotRequired[str]
    language: NotRequired[str]


class ExpandCandidatesParams(TypedDict):
    """Params JSON for `ContentSource.expandCandidates`. Seed fields identify a known title; the guest returns related catalog hits.

    Attributes:
        source: Source plugin id hint when expanding across storefronts.
        productId: Seed storefront product id.
        title: Seed title text.
        authors: Seed authors string.
        narrators: Seed narrators string.
        series: Seed series name.
        seriesAsin: Seed series ASIN when known.
        asin: Seed Amazon ASIN.
        isbn: Seed ISBN.
        region: Storefront region / marketplace code.
        limit: Maximum candidates to return (default 20).
    """

    source: NotRequired[str]
    productId: NotRequired[str]
    title: NotRequired[str]
    authors: NotRequired[str]
    narrators: NotRequired[str]
    series: NotRequired[str]
    seriesAsin: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    region: NotRequired[str]
    limit: NotRequired[int]


class PurchaseHintParams(TypedDict):
    """Params JSON for `ContentSource.purchaseHint`. At least one identity field (`productId` / `asin` / `isbn` / title+authors) should be set; guests may return `invalid_params` when none are usable.

    Attributes:
        productId: Storefront product id when known.
        title: Title text for fuzzy lookup.
        authors: Authors string for fuzzy lookup.
        asin: Amazon ASIN when known.
        isbn: ISBN when known.
        region: Storefront region / marketplace code.
        withPrice: When true, guests should include live price fields when
            available.
    """

    productId: NotRequired[str]
    title: NotRequired[str]
    authors: NotRequired[str]
    asin: NotRequired[str]
    isbn: NotRequired[str]
    region: NotRequired[str]
    withPrice: NotRequired[bool]


class ListDealsParams(TypedDict):
    """Params JSON for `ContentSource.listDeals`.

    Attributes:
        limit: Optional maximum number of deals to return; guest default
            when omitted.
    """

    limit: NotRequired[int]


class CatalogDetailParams(TypedDict):
    """Params JSON for `ContentSource.catalogDetail`.

    Attributes:
        productId: Store product id (Libro ISBN or ISBN-slug).
        isbn: Optional ISBN when it differs from `productId`.
    """

    productId: str
    isbn: NotRequired[str]


class ScanLibraryParams(TypedDict):
    """Params JSON for `Integration.scanLibrary` (remote library sync).

    Attributes:
        force: When true, force a full rescan even if the guest would
            otherwise incremental-sync.
    """

    force: NotRequired[bool]


class AuthenticateUserParams(TypedDict):
    """Params JSON for `Integration.authenticateUser`.

    Attributes:
        username: Integration username / login id.
        password: Integration password; never logged by the host.
    """

    username: str
    password: str

__all__ = [
    "AuthenticateUserParams",
    "Brand",
    "CLI_ARG_KINDS",
    "CatalogDetailParams",
    "CliArgKind",
    "CliArgSpec",
    "CliCommandSpec",
    "CliInvokeParams",
    "CliInvokeResult",
    "CliSchema",
    "ConfigOption",
    "ConfigOptionValue",
    "DatabaseAdapterConfig",
    "DiagnoseResult",
    "ExpandCandidatesParams",
    "FetchTitleParams",
    "HealthResult",
    "JsonObject",
    "JsonValue",
    "ListDealsParams",
    "LoginCompleteParams",
    "LoginParams",
    "LoginStartParams",
    "PLUGIN_ERROR_CODES",
    "PluginErrorCode",
    "PluginMetadata",
    "PurchaseHintParams",
    "ScanLibraryParams",
    "ScanParams",
    "SearchCatalogParams",
]
