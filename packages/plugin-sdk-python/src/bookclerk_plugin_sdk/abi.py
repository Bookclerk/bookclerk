"""ABI constants and wire DTOs — aligned with ``crates/bookclerk-plugin-abi/schema/abi.json``.

Machine-facing method names and the negotiated ``api_version`` are shared by
native stdio guests and workerd Python Workers. Field names on TypedDicts use
**camelCase** to match the Workers RPC wire format (same as the TypeScript
``generated.ts`` projection). Regenerated projections in other languages
consume the same schema; do not rename entries here without updating the ABI
crate.

See ``docs/plugins.md`` for the guest contract narrative.
"""

from __future__ import annotations

from typing import Any, Literal, NotRequired, TypedDict

API_VERSION: int = 1
"""Negotiated Bookclerk plugin ABI version (must match ``plugin.toml`` ``api_version``)."""

METHOD_NAMES: tuple[str, ...] = (
    "handshake",
    "shutdown",
    "health",
    "diagnose",
    "start",
    "onEvent",
    "pollEvents",
    "scanLibrary",
    "syncListening",
    "authenticateUser",
    "cliDescribe",
    "cliInvoke",
    "login",
    "loginStart",
    "loginComplete",
    "credentialsUpdate",
    "scan",
    "fetchTitle",
    "searchCatalog",
    "expandCandidates",
    "purchaseHint",
    "listDeals",
    "listAccounts",
    "catalogDetail",
    "put",
    "putFile",
    "get",
    "exists",
    "list",
    "probe",
    "copy",
    "delete",
    "touchFile",
    "dbConnect",
    "dbPing",
    "dbQuery",
    "dbExecute",
)
"""Canonical Workers RPC method names exposed on the guest surface (camelCase wire)."""

PluginKind = Literal["source", "integration", "output", "database"]
"""Plugin surface kind advertised in handshake and ``plugin.toml``."""

PluginErrorCode = Literal[
    "invalid_params",
    "unauthorized",
    "forbidden",
    "not_found",
    "unavailable",
    "unsupported",
    "internal",
]
"""Stable error codes carried on :class:`PluginError` ``code``."""

PluginLogLevel = Literal["debug", "info", "warn", "error"]
"""Severity for plugin_log events pushed to the host."""

CliArgKind = Literal["string", "bool", "int", "path"]
"""CLI argument value kind for plugin-declared commands."""


class PluginError(TypedDict):
    """RPC or plugin failure payload returned on the wire.

    Attributes:
        code: Stable machine-readable failure code.
        message: Human-readable error message (secrets must already be redacted).
        details: Optional structured details for operators or host UIs.
    """

    code: PluginErrorCode
    message: str
    details: NotRequired[dict[str, Any]]


class HandshakeParams(TypedDict):
    """Parameters for the required ``handshake`` RPC method.

    Attributes:
        apiVersion: Host-offered ABI version (must be :data:`API_VERSION`).
        config: Install / operator config object passed into the guest.
    """

    apiVersion: int
    config: dict[str, Any]


class BrandDto(TypedDict):
    """Brand colors and icon for UI chrome (wire camelCase).

    Attributes:
        id: Stable brand identifier (often matches plugin id).
        name: Display name shown beside the brand mark in Accounts / Settings.
        bg: Brand panel background as a CSS color string.
        fg: Brand panel foreground / text as a CSS color string.
        accent: Highlight / CTA accent as a CSS color string.
        iconUrl: Absolute ``https://`` URL or relative path for the brand icon.
    """

    id: str
    name: str
    bg: str
    fg: str
    accent: str
    iconUrl: str


class ConfigOptionValueDto(TypedDict):
    """One selectable value under a :class:`ConfigOptionDto`.

    Attributes:
        id: Machine id written into config when the operator selects this value.
        label: Operator-facing label shown in the option picker.
    """

    id: str
    label: str


class ConfigOptionDto(TypedDict):
    """Config option descriptor discovered during handshake for sources.

    Attributes:
        key: Config key written under the plugin's config table when chosen.
        label: Operator-facing label for the option group in Settings.
        values: Allowed values the operator may pick for this option.
    """

    key: str
    label: str
    values: list[ConfigOptionValueDto]


class CliArgSpec(TypedDict, total=False):
    """One CLI argument declared by a plugin command (wire camelCase).

    Attributes:
        name: Internal argument name (also the default long flag).
        long: Long flag spelling without the leading ``--``.
        short: Single-character short flag without the leading ``-``.
        kind: Value kind; defaults to ``string`` when omitted.
        required: When true, the host must supply this argument.
        default: Default value encoded as a string when the flag is omitted.
        about: Short help text rendered next to the flag.
        positional: When true, the argument is positional rather than a flag.
    """

    name: str
    long: str
    short: str
    kind: CliArgKind
    required: bool
    default: str
    about: str
    positional: bool


class CliCommandSpec(TypedDict, total=False):
    """One plugin CLI command declared in handshake / ``cliDescribe``.

    Attributes:
        name: Command name passed as ``cliInvoke.command``.
        about: Short help text for ``bookclerk plugin <id> --help``.
        args: Arguments accepted by this command.
    """

    name: str
    about: str
    args: list[CliArgSpec]


class CliSchema(TypedDict, total=False):
    """Declared CLI surface returned from handshake or ``cliDescribe``.

    Attributes:
        commands: Commands the guest exposes to ``bookclerk plugin``.
    """

    commands: list[CliCommandSpec]


class HandshakeResult(TypedDict, total=False):
    """Result of a successful ``handshake`` negotiation (wire camelCase).

    Attributes:
        apiVersion: Negotiated ABI version (must match :data:`API_VERSION`).
        id: Globally unique plugin id (``[a-z0-9_]{2,32}`` grammar).
        kind: Plugin kind (``source``, ``integration``, ``output``, or ``database``).
        displayName: Optional operator-facing display name.
        capabilities: Capability method names this guest implements.
        brand: Optional UI brand block.
        configOptions: Optional config options for Accounts / Settings.
        cli: Optional CLI schema (may also come from ``cliDescribe``).
        portalAuthMode: Portal login mode (``oauth`` or ``password``).
        passwordEnvVar: Env var name for password-mode portal auth.
        aliases: Alternate ids accepted by the host for this install.
        sortKey: Sort key for stable ordering in UI lists (lower first).
    """

    apiVersion: int
    id: str
    kind: str
    displayName: str
    capabilities: list[str]
    brand: BrandDto
    configOptions: list[ConfigOptionDto]
    cli: CliSchema
    portalAuthMode: Literal["oauth", "password"]
    passwordEnvVar: str
    aliases: list[str]
    sortKey: int


class CliInvokeParams(TypedDict, total=False):
    """Parameters for the ``cliInvoke`` RPC method.

    Attributes:
        command: Command name matching a :class:`CliCommandSpec` ``name``.
        args: Named argument map (flag names → values).
    """

    command: str
    args: dict[str, Any]


class CliInvokeResult(TypedDict, total=False):
    """Result of a ``cliInvoke`` call.

    Attributes:
        exitCode: Process-style exit code (``0`` = success).
        stdout: Captured stdout text shown to the operator.
        stderr: Captured stderr text shown on failure or diagnostics.
        json: Optional structured JSON payload alongside text output.
    """

    exitCode: int
    stdout: str
    stderr: str
    json: Any


class HealthResult(TypedDict, total=False):
    """Result of the ``health`` RPC method.

    Attributes:
        ok: Whether the guest considers itself healthy for host scheduling.
        id: Optional plugin id echo for multi-guest probes.
        enabled: Optional enablement flag mirroring operator config.
        detail: Optional human-readable health detail for Status / doctor UIs.
    """

    ok: bool
    id: str
    enabled: bool
    detail: str


class DiagnoseResult(TypedDict):
    """Result of the ``diagnose`` RPC method (``plugins doctor``).

    Attributes:
        lines: Operator-facing diagnostic lines (empty when nothing to report).
    """

    lines: list[str]


class LoginParams(TypedDict, total=False):
    """Parameters for one-shot ``login`` and interactive ``loginStart``.

    Wire field names are camelCase (``pluginDataDir``, ``callbackBind``, …).

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin.
        marketplace: Marketplace / locale code (for example ``us``, ``uk``).
        label: Operator-facing account label in the Accounts UI.
        email: Account email for password-mode storefronts.
        password: Account password (never log or persist plainly).
        force: When true, overwrite an existing sealed credential blob.
        callbackBind: Optional guest OAuth callback bind address (``host:port``).
        callbackIpc: Host-owned callback IPC endpoint (socket path or pipe name).
        callbackPublicBase: Public base URL for the host TCP listener.
        external: When true, use paste-redirect OAuth instead of a local callback.
        responseUrl: Pre-supplied OAuth redirect URL from the operator.
        showQr: Prefer QR output when the guest can render an authorize URL.
        timeoutSecs: Seconds to wait for OAuth callback capture.
        extra: Store-specific knobs; guests may ignore unknown keys.
    """

    pluginDataDir: str
    marketplace: str
    label: str
    email: str
    password: str
    force: bool
    callbackBind: str
    callbackIpc: str
    callbackPublicBase: str
    external: bool
    responseUrl: str
    showQr: bool
    timeoutSecs: int
    extra: dict[str, Any]


class LoginCompleteParams(TypedDict):
    """Parameters for interactive ``loginComplete``.

    Attributes:
        sessionId: Opaque session id returned by ``loginStart``.
    """

    sessionId: str


class CredentialsUpdateParams(TypedDict):
    """Parameters for ``credentialsUpdate`` — guest-requested credential write-back.

    Attributes:
        accountId: Account id whose sealed credential blob should be replaced.
        credentials: Replacement opaque credential JSON for the host to re-seal.
    """

    accountId: str
    credentials: dict[str, Any]


class ScanParams(TypedDict, total=False):
    """Parameters for source ``scan`` (wire camelCase).

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin.
        accounts: Account ids to include; empty means all known accounts.
        pageSize: Page size for storefront pagination.
        importEpisodes: When true, import podcast / series episodes.
        importPlusTitles: When true, import Plus / catalog-included titles.
        credentials: Host-loaded credential blobs keyed by ``accountId``.
    """

    pluginDataDir: str
    accounts: list[str]
    pageSize: int
    importEpisodes: bool
    importPlusTitles: bool
    credentials: dict[str, dict[str, Any]]


class FetchTitleParams(TypedDict, total=False):
    """Parameters for source ``fetchTitle`` (wire camelCase).

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin.
        accountId: Account that owns the title.
        titleId: Library / storefront title identifier (ASIN, ISBN, UUID, …).
        cacheDir: Absolute path to the guest download cache for this fetch.
        credentials: Host-loaded credential blob for this account.
        sourceConfig: Opaque plugin table from ``[sources.<id>]``.
        download: Host acquire/download options matching ``DownloadOptions``.
    """

    pluginDataDir: str
    accountId: str
    titleId: str
    cacheDir: str
    credentials: dict[str, Any]
    sourceConfig: dict[str, Any]
    download: dict[str, Any]


class SearchCatalogParams(TypedDict, total=False):
    """Parameters for ``searchCatalog`` (wire camelCase).

    Attributes:
        query: Free-text catalog query.
        region: Marketplace / region code when multi-market.
        limit: Maximum number of hits to return.
        page: 1-based page index for paginated catalogs.
        sort: Store-specific sort key (for example ``relevance``).
        field: Optional field to search within.
        language: Preferred content language filter.
    """

    query: str
    region: str
    limit: int
    page: int
    sort: str
    field: str
    language: str


class PutParams(TypedDict, total=False):
    """Parameters for destination ``put`` (inline Base64 body; wire camelCase).

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin.
        bucket: Destination bucket name (S3-compatible stores).
        prefix: Key prefix applied before object keys.
        region: AWS / S3-compatible region identifier.
        endpoint: Optional custom endpoint (MinIO, R2, …).
        forcePathStyle: When true, use path-style URLs.
        credentials: Host-injected credential blob.
        key: Destination object key (relative to ``prefix``).
        dataBase64: Base64-encoded object body.
        meta: Optional object metadata to store with the bytes.
    """

    pluginDataDir: str
    bucket: str
    prefix: str
    region: str
    endpoint: str
    forcePathStyle: bool
    credentials: dict[str, Any]
    key: str
    dataBase64: str
    meta: dict[str, Any]


class KeyParams(TypedDict, total=False):
    """Parameters for key-scoped destination methods (``get`` / ``exists`` / ``delete``).

    Attributes:
        pluginDataDir: Scoped writable directory for this plugin.
        bucket: Destination bucket name (S3-compatible stores).
        prefix: Key prefix applied before object keys.
        region: AWS / S3-compatible region identifier.
        endpoint: Optional custom endpoint (MinIO, R2, …).
        forcePathStyle: When true, use path-style URLs.
        credentials: Host-injected credential blob.
        key: Destination object key (relative to ``prefix``).
    """

    pluginDataDir: str
    bucket: str
    prefix: str
    region: str
    endpoint: str
    forcePathStyle: bool
    credentials: dict[str, Any]
    key: str


class StatementDto(TypedDict, total=False):
    """SQL statement DTO for ``dbQuery`` / ``dbExecute`` (wire camelCase).

    Attributes:
        sql: SQL text with placeholders as understood by the guest dialect.
        values: Ordered bind values for the statement (default empty).
    """

    sql: str
    values: list[Any]


__all__ = [
    "API_VERSION",
    "METHOD_NAMES",
    "BrandDto",
    "CliArgKind",
    "CliArgSpec",
    "CliCommandSpec",
    "CliInvokeParams",
    "CliInvokeResult",
    "CliSchema",
    "ConfigOptionDto",
    "ConfigOptionValueDto",
    "CredentialsUpdateParams",
    "DiagnoseResult",
    "FetchTitleParams",
    "HandshakeParams",
    "HandshakeResult",
    "HealthResult",
    "KeyParams",
    "LoginCompleteParams",
    "LoginParams",
    "PluginError",
    "PluginErrorCode",
    "PluginKind",
    "PluginLogLevel",
    "PutParams",
    "ScanParams",
    "SearchCatalogParams",
    "StatementDto",
]
