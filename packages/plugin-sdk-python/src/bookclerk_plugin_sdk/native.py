"""Native stdio Workers RPC — BookclerkPlugin + BookclerkPluginGuest.

Dual-stack with workerd:

- Native:  ``from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest``
- Workerd: ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``

``BookclerkPluginGuest.serve`` frames stdin/stdout; authors subclass ``BookclerkPlugin``.
See ``docs/plugins.md`` for the host/guest contract and ABI methods.
"""

from __future__ import annotations

import json
import sys
from typing import Any, Callable, Mapping, MutableMapping


class BookclerkPlugin:
    """Branded native guest base — same method surface as workerd BookclerkPlugin.

    Subclass and override the methods your ``plugin.toml`` advertises. Unimplemented
    optional methods raise ``RuntimeError`` with ``code = "unsupported"``. CamelCase
    aliases match Workers RPC method names used on the wire.

    Examples:
        >>> class Echo(BookclerkPlugin):
        ...     def handshake(self, params):
        ...         return {
        ...             "apiVersion": 1,
        ...             "id": "echo",
        ...             "kind": "source",
        ...             "capabilities": ["health"],
        ...         }
        >>> # BookclerkPluginGuest.serve(Echo())
    """

    def handshake(self, params: Mapping[str, Any]) -> Mapping[str, Any]:
        """Run the guest handshake against the host bridge.

        Args:
            params: Negotiated install identity, ``apiVersion``, and host config.

        Returns:
            Handshake result including ``apiVersion``, plugin ``id``, and ``kind``.

        Raises:
            NotImplementedError: Always on the base class; subclasses must override.
        """
        raise NotImplementedError("handshake")

    def shutdown(self) -> None:
        """Shut down the guest cleanly.

        Returns:
            ``None``. The base implementation is a no-op.
        """
        return None

    def health(self) -> Mapping[str, Any]:
        """Report guest liveness.

        Returns:
            Mapping with at least ``ok`` (defaults to ``True``).
        """
        return {"ok": True}

    def diagnose(self) -> Mapping[str, Any]:
        """Return diagnostic lines for operator tooling.

        Returns:
            Mapping with a ``lines`` list (empty by default).
        """
        return {"lines": []}

    def on_event(self, _event: Mapping[str, Any]) -> None:
        """Handle a host-pushed event (snake_case form).

        Args:
            _event: Event payload from the host.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("onEvent not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    # CamelCase aliases match Workers RPC method names / workerd overrides.
    def onEvent(self, event: Mapping[str, Any]) -> None:  # noqa: N802
        """Handle a host-pushed event (Workers RPC name).

        Args:
            event: Event payload from the host.

        Raises:
            RuntimeError: If :meth:`on_event` is not overridden.
        """
        return self.on_event(event)

    def cli_describe(self) -> Mapping[str, Any]:
        """Describe CLI commands exposed by this guest.

        Returns:
            Mapping with a ``commands`` list (empty by default).
        """
        return {"commands": []}

    def cliDescribe(self) -> Mapping[str, Any]:  # noqa: N802
        """Describe CLI commands (Workers RPC name).

        Returns:
            Result of :meth:`cli_describe`.
        """
        return self.cli_describe()

    def cli_invoke(self, _params: Mapping[str, Any]) -> Mapping[str, Any]:
        """Invoke a guest CLI command (snake_case form).

        Args:
            _params: Command name and argument map from the host.

        Returns:
            Command result (typically ``exitCode``, ``stdout``, ``stderr``).

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("cliInvoke not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def cliInvoke(self, params: Mapping[str, Any]) -> Mapping[str, Any]:  # noqa: N802
        """Invoke a guest CLI command (Workers RPC name).

        Args:
            params: Command name and argument map from the host.

        Returns:
            Result of :meth:`cli_invoke`.

        Raises:
            RuntimeError: If :meth:`cli_invoke` is not overridden.
        """
        return self.cli_invoke(params)

    def start(self, _params: Mapping[str, Any]) -> Any:
        """Start long-running guest work (integration plugins).

        Args:
            _params: Host start parameters.

        Returns:
            Plugin-defined start result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("start not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def start(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Start long-running guest work (Workers RPC name).

        Args:
            params: Host start parameters.

        Returns:
            Plugin-defined start result.

        Raises:
            RuntimeError: If start is not overridden.
        """
        return self.start(params)

    def poll_events(self) -> Any:
        """Poll for guest-emitted events (snake_case form).

        Returns:
            Plugin-defined event batch.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("pollEvents not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def pollEvents(self) -> Any:  # noqa: N802
        """Poll for guest-emitted events (Workers RPC name).

        Returns:
            Result of :meth:`poll_events`.

        Raises:
            RuntimeError: If :meth:`poll_events` is not overridden.
        """
        return self.poll_events()

    def scan_library(self, _params: Mapping[str, Any]) -> Any:
        """Scan the connected library for titles (snake_case form).

        Args:
            _params: Scan options from the host.

        Returns:
            Plugin-defined scan result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("scanLibrary not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def scanLibrary(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Scan the connected library for titles (Workers RPC name).

        Args:
            params: Scan options from the host.

        Returns:
            Result of :meth:`scan_library`.

        Raises:
            RuntimeError: If :meth:`scan_library` is not overridden.
        """
        return self.scan_library(params)

    def sync_listening(self) -> Any:
        """Sync listening progress with the storefront (snake_case form).

        Returns:
            Plugin-defined sync result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("syncListening not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def syncListening(self) -> Any:  # noqa: N802
        """Sync listening progress with the storefront (Workers RPC name).

        Returns:
            Result of :meth:`sync_listening`.

        Raises:
            RuntimeError: If :meth:`sync_listening` is not overridden.
        """
        return self.sync_listening()

    def authenticate_user(self, _params: Mapping[str, Any]) -> Any:
        """Authenticate a library user via the guest (snake_case form).

        Args:
            _params: Authentication parameters from the host.

        Returns:
            Plugin-defined auth result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("authenticateUser not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def authenticateUser(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Authenticate a library user via the guest (Workers RPC name).

        Args:
            params: Authentication parameters from the host.

        Returns:
            Result of :meth:`authenticate_user`.

        Raises:
            RuntimeError: If :meth:`authenticate_user` is not overridden.
        """
        return self.authenticate_user(params)

    def login(self, _params: Mapping[str, Any]) -> Any:
        """Perform a synchronous store login (snake_case form).

        Args:
            _params: Login credentials / options.

        Returns:
            Plugin-defined login result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("login not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def login(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Perform a synchronous store login (Workers RPC name).

        Args:
            params: Login credentials / options.

        Returns:
            Plugin-defined login result.

        Raises:
            RuntimeError: If login is not overridden.
        """
        return self.login(params)

    def login_start(self, _params: Mapping[str, Any]) -> Any:
        """Begin an interactive / OAuth login flow (snake_case form).

        Args:
            _params: Login-start parameters from the host.

        Returns:
            Plugin-defined start payload (e.g. redirect URL).

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("loginStart not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def loginStart(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Begin an interactive / OAuth login flow (Workers RPC name).

        Args:
            params: Login-start parameters from the host.

        Returns:
            Result of :meth:`login_start`.

        Raises:
            RuntimeError: If :meth:`login_start` is not overridden.
        """
        return self.login_start(params)

    def login_complete(self, _params: Mapping[str, Any]) -> Any:
        """Complete an interactive / OAuth login flow (snake_case form).

        Args:
            _params: Completion parameters (callback payload, codes, etc.).

        Returns:
            Plugin-defined completion result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("loginComplete not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def loginComplete(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Complete an interactive / OAuth login flow (Workers RPC name).

        Args:
            params: Completion parameters (callback payload, codes, etc.).

        Returns:
            Result of :meth:`login_complete`.

        Raises:
            RuntimeError: If :meth:`login_complete` is not overridden.
        """
        return self.login_complete(params)

    def credentials_update(self, _params: Mapping[str, Any]) -> Any:
        """Update stored credentials for an account (snake_case form).

        Args:
            _params: Credential update payload.

        Returns:
            Plugin-defined update result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("credentialsUpdate not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def credentialsUpdate(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Update stored credentials for an account (Workers RPC name).

        Args:
            params: Credential update payload.

        Returns:
            Result of :meth:`credentials_update`.

        Raises:
            RuntimeError: If :meth:`credentials_update` is not overridden.
        """
        return self.credentials_update(params)

    def scan(self, _params: Mapping[str, Any]) -> Any:
        """Scan a source account for titles (snake_case form).

        Args:
            _params: Scan options from the host.

        Returns:
            Plugin-defined scan result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("scan not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def scan(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Scan a source account for titles (Workers RPC name).

        Args:
            params: Scan options from the host.

        Returns:
            Plugin-defined scan result.

        Raises:
            RuntimeError: If scan is not overridden.
        """
        return self.scan(params)

    def fetch_title(self, _params: Mapping[str, Any]) -> Any:
        """Fetch / acquire a single title (snake_case form).

        Args:
            _params: Title identity and acquire options.

        Returns:
            Plugin-defined fetch result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("fetchTitle not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def fetchTitle(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Fetch / acquire a single title (Workers RPC name).

        Args:
            params: Title identity and acquire options.

        Returns:
            Result of :meth:`fetch_title`.

        Raises:
            RuntimeError: If :meth:`fetch_title` is not overridden.
        """
        return self.fetch_title(params)

    def search_catalog(self, _params: Mapping[str, Any]) -> Any:
        """Search the storefront catalog (snake_case form).

        Args:
            _params: Search query and filters.

        Returns:
            Plugin-defined search hits.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("searchCatalog not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def searchCatalog(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Search the storefront catalog (Workers RPC name).

        Args:
            params: Search query and filters.

        Returns:
            Result of :meth:`search_catalog`.

        Raises:
            RuntimeError: If :meth:`search_catalog` is not overridden.
        """
        return self.search_catalog(params)

    def expand_candidates(self, _params: Mapping[str, Any]) -> Any:
        """Expand discover candidates for a work (snake_case form).

        Args:
            _params: Candidate expansion parameters.

        Returns:
            Plugin-defined candidate list.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("expandCandidates not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def expandCandidates(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Expand discover candidates for a work (Workers RPC name).

        Args:
            params: Candidate expansion parameters.

        Returns:
            Result of :meth:`expand_candidates`.

        Raises:
            RuntimeError: If :meth:`expand_candidates` is not overridden.
        """
        return self.expand_candidates(params)

    def purchase_hint(self, _params: Mapping[str, Any]) -> Any:
        """Return a purchase hint / deep link (snake_case form).

        Args:
            _params: Title identity for the hint.

        Returns:
            Plugin-defined purchase hint.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("purchaseHint not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def purchaseHint(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Return a purchase hint / deep link (Workers RPC name).

        Args:
            params: Title identity for the hint.

        Returns:
            Result of :meth:`purchase_hint`.

        Raises:
            RuntimeError: If :meth:`purchase_hint` is not overridden.
        """
        return self.purchase_hint(params)

    def list_deals(self, _params: Mapping[str, Any]) -> Any:
        """List storefront deals (snake_case form).

        Args:
            _params: Deal listing options.

        Returns:
            Plugin-defined deal list.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("listDeals not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def listDeals(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """List storefront deals (Workers RPC name).

        Args:
            params: Deal listing options.

        Returns:
            Result of :meth:`list_deals`.

        Raises:
            RuntimeError: If :meth:`list_deals` is not overridden.
        """
        return self.list_deals(params)

    def list_accounts(self, _params: Mapping[str, Any]) -> Any:
        """List accounts known to the guest (snake_case form).

        Args:
            _params: Account listing options.

        Returns:
            Plugin-defined account list.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("listAccounts not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def listAccounts(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """List accounts known to the guest (Workers RPC name).

        Args:
            params: Account listing options.

        Returns:
            Result of :meth:`list_accounts`.

        Raises:
            RuntimeError: If :meth:`list_accounts` is not overridden.
        """
        return self.list_accounts(params)

    def catalog_detail(self, _params: Mapping[str, Any]) -> Any:
        """Fetch catalog detail for a title (snake_case form).

        Args:
            _params: Catalog identity parameters.

        Returns:
            Plugin-defined catalog detail.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("catalogDetail not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def catalogDetail(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Fetch catalog detail for a title (Workers RPC name).

        Args:
            params: Catalog identity parameters.

        Returns:
            Result of :meth:`catalog_detail`.

        Raises:
            RuntimeError: If :meth:`catalog_detail` is not overridden.
        """
        return self.catalog_detail(params)

    def put(self, _params: Mapping[str, Any]) -> Any:
        """Write bytes to a destination (snake_case form).

        Args:
            _params: Destination path and payload.

        Returns:
            Plugin-defined put result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("put not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def put(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Write bytes to a destination (Workers RPC name).

        Args:
            params: Destination path and payload.

        Returns:
            Plugin-defined put result.

        Raises:
            RuntimeError: If put is not overridden.
        """
        return self.put(params)

    def put_file(self, _params: Mapping[str, Any]) -> Any:
        """Write a local file to a destination (snake_case form).

        Args:
            _params: Source file and destination path.

        Returns:
            Plugin-defined put-file result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("putFile not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def putFile(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Write a local file to a destination (Workers RPC name).

        Args:
            params: Source file and destination path.

        Returns:
            Result of :meth:`put_file`.

        Raises:
            RuntimeError: If :meth:`put_file` is not overridden.
        """
        return self.put_file(params)

    def get(self, _params: Mapping[str, Any]) -> Any:
        """Read bytes from a destination (snake_case form).

        Args:
            _params: Destination path to read.

        Returns:
            Plugin-defined get result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("get not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def get(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Read bytes from a destination (Workers RPC name).

        Args:
            params: Destination path to read.

        Returns:
            Plugin-defined get result.

        Raises:
            RuntimeError: If get is not overridden.
        """
        return self.get(params)

    def exists(self, _params: Mapping[str, Any]) -> Any:
        """Test whether a destination object exists (snake_case form).

        Args:
            _params: Destination path to probe.

        Returns:
            Plugin-defined existence result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("exists not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def exists(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Test whether a destination object exists (Workers RPC name).

        Args:
            params: Destination path to probe.

        Returns:
            Plugin-defined existence result.

        Raises:
            RuntimeError: If exists is not overridden.
        """
        return self.exists(params)

    def list(self, _params: Mapping[str, Any]) -> Any:
        """List objects under a destination prefix (snake_case form).

        Args:
            _params: Listing prefix / options.

        Returns:
            Plugin-defined listing result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("list not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def list(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """List objects under a destination prefix (Workers RPC name).

        Args:
            params: Listing prefix / options.

        Returns:
            Plugin-defined listing result.

        Raises:
            RuntimeError: If list is not overridden.
        """
        return self.list(params)

    def probe(self, _params: Mapping[str, Any]) -> Any:
        """Probe destination connectivity / capabilities (snake_case form).

        Args:
            _params: Probe options.

        Returns:
            Plugin-defined probe result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("probe not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def probe(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Probe destination connectivity / capabilities (Workers RPC name).

        Args:
            params: Probe options.

        Returns:
            Plugin-defined probe result.

        Raises:
            RuntimeError: If probe is not overridden.
        """
        return self.probe(params)

    def copy(self, _params: Mapping[str, Any]) -> Any:
        """Copy an object within a destination (snake_case form).

        Args:
            _params: Source and destination paths.

        Returns:
            Plugin-defined copy result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("copy not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def copy(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Copy an object within a destination (Workers RPC name).

        Args:
            params: Source and destination paths.

        Returns:
            Plugin-defined copy result.

        Raises:
            RuntimeError: If copy is not overridden.
        """
        return self.copy(params)

    def delete(self, _params: Mapping[str, Any]) -> Any:
        """Delete an object from a destination (snake_case form).

        Args:
            _params: Destination path to delete.

        Returns:
            Plugin-defined delete result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("delete not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def delete(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Delete an object from a destination (Workers RPC name).

        Args:
            params: Destination path to delete.

        Returns:
            Plugin-defined delete result.

        Raises:
            RuntimeError: If delete is not overridden.
        """
        return self.delete(params)

    def touch_file(self, _params: Mapping[str, Any]) -> Any:
        """Touch / update metadata on a destination object (snake_case form).

        Args:
            _params: Path and touch options.

        Returns:
            Plugin-defined touch result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("touchFile not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def touchFile(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Touch / update metadata on a destination object (Workers RPC name).

        Args:
            params: Path and touch options.

        Returns:
            Result of :meth:`touch_file`.

        Raises:
            RuntimeError: If :meth:`touch_file` is not overridden.
        """
        return self.touch_file(params)

    def db_connect(self, _params: Mapping[str, Any]) -> Any:
        """Open a database session (snake_case form).

        Args:
            _params: Connection parameters from the host.

        Returns:
            Plugin-defined connection handle / status.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("dbConnect not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbConnect(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Open a database session (Workers RPC name).

        Args:
            params: Connection parameters from the host.

        Returns:
            Result of :meth:`db_connect`.

        Raises:
            RuntimeError: If :meth:`db_connect` is not overridden.
        """
        return self.db_connect(params)

    def db_ping(self) -> Any:
        """Ping the database backend (snake_case form).

        Returns:
            Plugin-defined ping result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("dbPing not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbPing(self) -> Any:  # noqa: N802
        """Ping the database backend (Workers RPC name).

        Returns:
            Result of :meth:`db_ping`.

        Raises:
            RuntimeError: If :meth:`db_ping` is not overridden.
        """
        return self.db_ping()

    def db_query(self, _params: Mapping[str, Any]) -> Any:
        """Run a read query against the database (snake_case form).

        Args:
            _params: SQL / query parameters.

        Returns:
            Plugin-defined query rows.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("dbQuery not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbQuery(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Run a read query against the database (Workers RPC name).

        Args:
            params: SQL / query parameters.

        Returns:
            Result of :meth:`db_query`.

        Raises:
            RuntimeError: If :meth:`db_query` is not overridden.
        """
        return self.db_query(params)

    def db_execute(self, _params: Mapping[str, Any]) -> Any:
        """Execute a write statement against the database (snake_case form).

        Args:
            _params: SQL / execute parameters.

        Returns:
            Plugin-defined execute result.

        Raises:
            RuntimeError: With ``code="unsupported"`` when not overridden.
        """
        err = RuntimeError("dbExecute not implemented")
        err.code = "unsupported"  # type: ignore[attr-defined]
        raise err

    def dbExecute(self, params: Mapping[str, Any]) -> Any:  # noqa: N802
        """Execute a write statement against the database (Workers RPC name).

        Args:
            params: SQL / execute parameters.

        Returns:
            Result of :meth:`db_execute`.

        Raises:
            RuntimeError: If :meth:`db_execute` is not overridden.
        """
        return self.db_execute(params)


class BookclerkPluginGuest:
    """Native guest runner — hosts a BookclerkPlugin on stdin/stdout (Workers RPC).

    Reads one JSON-RPC-style request per line from stdin and writes a JSON
    response line to stdout. Shutdown returns after the ``shutdown`` method.
    """

    @staticmethod
    def serve(
        plugin: Any = None,
        *,
        handlers: Mapping[str, Callable[[Any], Any]] | None = None,
    ) -> None:
        """Serve Workers RPC frames for ``plugin`` or an explicit ``handlers`` map.

        Args:
            plugin: Instance implementing at least ``handshake`` (typically a
                :class:`BookclerkPlugin` subclass). Ignored when ``handlers`` is set.
            handlers: Optional explicit method-name → callable dispatch table.

        Raises:
            TypeError: If neither a valid plugin nor handlers provide ``handshake``.

        Examples:
            >>> # await / run until stdin closes:
            >>> # BookclerkPluginGuest.serve(MyPlugin())
        """
        dispatch = dict(handlers) if handlers is not None else _dispatch_from_plugin(plugin)
        for raw in sys.stdin:
            line = raw.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError as err:
                _write({"id": None, "error": {"code": "internal", "message": f"invalid JSON: {err}"}})
                continue
            req_id = req.get("id")
            method = req.get("method") or ""
            params = req.get("params")
            try:
                if method not in dispatch:
                    err = RuntimeError(f"unsupported method: {method}")
                    err.code = "unsupported"  # type: ignore[attr-defined]
                    raise err
                result = dispatch[method](params)
                _write({"id": req_id, "result": result})
                if method == "shutdown":
                    return
            except Exception as err:  # noqa: BLE001 — RPC boundary
                code = getattr(err, "code", "internal")
                _write({"id": req_id, "error": {"code": code, "message": str(err)}})


def _dispatch_from_plugin(plugin: Any) -> dict[str, Callable[[Any], Any]]:
    def invoke(name: str, *alts: str) -> Callable[[Any], Any] | None:
        for n in (name, *alts):
            if hasattr(plugin, n):
                return getattr(plugin, n)
        return None

    hs = invoke("handshake")
    if hs is None:
        raise TypeError("plugin must implement handshake (subclass BookclerkPlugin)")

    def on_shutdown(_p: Any) -> None:
        fn = invoke("shutdown")
        if fn:
            fn()
        return None

    def on_health(_p: Any) -> Mapping[str, Any]:
        fn = invoke("health")
        return fn() if fn else {"ok": True}

    def on_diagnose(_p: Any) -> Mapping[str, Any]:
        fn = invoke("diagnose")
        return fn() if fn else {"lines": []}

    def on_event(p: Any) -> Mapping[str, Any]:
        fn = invoke("onEvent", "on_event")
        if not fn:
            err = RuntimeError("onEvent not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        fn(p or {})
        return {"ok": True}

    def on_cli_describe(_p: Any) -> Mapping[str, Any]:
        fn = invoke("cliDescribe", "cli_describe")
        return fn() if fn else {"commands": []}

    def on_cli_invoke(p: Any) -> Mapping[str, Any]:
        fn = invoke("cliInvoke", "cli_invoke")
        if not fn:
            err = RuntimeError("cliInvoke not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_start(p: Any) -> Any:
        fn = invoke("start", "start")
        if not fn:
            err = RuntimeError("start not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_poll_events(_p: Any) -> Any:
        fn = invoke("pollEvents", "poll_events")
        if not fn:
            err = RuntimeError("pollEvents not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn()

    def _on_scan_library(p: Any) -> Any:
        fn = invoke("scanLibrary", "scan_library")
        if not fn:
            err = RuntimeError("scanLibrary not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_sync_listening(_p: Any) -> Any:
        fn = invoke("syncListening", "sync_listening")
        if not fn:
            err = RuntimeError("syncListening not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn()

    def _on_authenticate_user(p: Any) -> Any:
        fn = invoke("authenticateUser", "authenticate_user")
        if not fn:
            err = RuntimeError("authenticateUser not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_login(p: Any) -> Any:
        fn = invoke("login", "login")
        if not fn:
            err = RuntimeError("login not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_login_start(p: Any) -> Any:
        fn = invoke("loginStart", "login_start")
        if not fn:
            err = RuntimeError("loginStart not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_login_complete(p: Any) -> Any:
        fn = invoke("loginComplete", "login_complete")
        if not fn:
            err = RuntimeError("loginComplete not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_credentials_update(p: Any) -> Any:
        fn = invoke("credentialsUpdate", "credentials_update")
        if not fn:
            err = RuntimeError("credentialsUpdate not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_scan(p: Any) -> Any:
        fn = invoke("scan", "scan")
        if not fn:
            err = RuntimeError("scan not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_fetch_title(p: Any) -> Any:
        fn = invoke("fetchTitle", "fetch_title")
        if not fn:
            err = RuntimeError("fetchTitle not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_search_catalog(p: Any) -> Any:
        fn = invoke("searchCatalog", "search_catalog")
        if not fn:
            err = RuntimeError("searchCatalog not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_expand_candidates(p: Any) -> Any:
        fn = invoke("expandCandidates", "expand_candidates")
        if not fn:
            err = RuntimeError("expandCandidates not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_purchase_hint(p: Any) -> Any:
        fn = invoke("purchaseHint", "purchase_hint")
        if not fn:
            err = RuntimeError("purchaseHint not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_list_deals(p: Any) -> Any:
        fn = invoke("listDeals", "list_deals")
        if not fn:
            err = RuntimeError("listDeals not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_list_accounts(p: Any) -> Any:
        fn = invoke("listAccounts", "list_accounts")
        if not fn:
            err = RuntimeError("listAccounts not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_catalog_detail(p: Any) -> Any:
        fn = invoke("catalogDetail", "catalog_detail")
        if not fn:
            err = RuntimeError("catalogDetail not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_put(p: Any) -> Any:
        fn = invoke("put", "put")
        if not fn:
            err = RuntimeError("put not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_put_file(p: Any) -> Any:
        fn = invoke("putFile", "put_file")
        if not fn:
            err = RuntimeError("putFile not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_get(p: Any) -> Any:
        fn = invoke("get", "get")
        if not fn:
            err = RuntimeError("get not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_exists(p: Any) -> Any:
        fn = invoke("exists", "exists")
        if not fn:
            err = RuntimeError("exists not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_list(p: Any) -> Any:
        fn = invoke("list", "list")
        if not fn:
            err = RuntimeError("list not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_probe(p: Any) -> Any:
        fn = invoke("probe", "probe")
        if not fn:
            err = RuntimeError("probe not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_copy(p: Any) -> Any:
        fn = invoke("copy", "copy")
        if not fn:
            err = RuntimeError("copy not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_delete(p: Any) -> Any:
        fn = invoke("delete", "delete")
        if not fn:
            err = RuntimeError("delete not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_touch_file(p: Any) -> Any:
        fn = invoke("touchFile", "touch_file")
        if not fn:
            err = RuntimeError("touchFile not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_db_connect(p: Any) -> Any:
        fn = invoke("dbConnect", "db_connect")
        if not fn:
            err = RuntimeError("dbConnect not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_db_ping(_p: Any) -> Any:
        fn = invoke("dbPing", "db_ping")
        if not fn:
            err = RuntimeError("dbPing not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn()

    def _on_db_query(p: Any) -> Any:
        fn = invoke("dbQuery", "db_query")
        if not fn:
            err = RuntimeError("dbQuery not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    def _on_db_execute(p: Any) -> Any:
        fn = invoke("dbExecute", "db_execute")
        if not fn:
            err = RuntimeError("dbExecute not implemented")
            err.code = "unsupported"  # type: ignore[attr-defined]
            raise err
        return fn(p or {})

    return {
        "handshake": lambda p: hs(p or {}),
        "shutdown": on_shutdown,
        "health": on_health,
        "diagnose": on_diagnose,
        "onEvent": on_event,
        "cliDescribe": on_cli_describe,
        "cliInvoke": on_cli_invoke,
        "start": _on_start,
        "pollEvents": _on_poll_events,
        "scanLibrary": _on_scan_library,
        "syncListening": _on_sync_listening,
        "authenticateUser": _on_authenticate_user,
        "login": _on_login,
        "loginStart": _on_login_start,
        "loginComplete": _on_login_complete,
        "credentialsUpdate": _on_credentials_update,
        "scan": _on_scan,
        "fetchTitle": _on_fetch_title,
        "searchCatalog": _on_search_catalog,
        "expandCandidates": _on_expand_candidates,
        "purchaseHint": _on_purchase_hint,
        "listDeals": _on_list_deals,
        "listAccounts": _on_list_accounts,
        "catalogDetail": _on_catalog_detail,
        "put": _on_put,
        "putFile": _on_put_file,
        "get": _on_get,
        "exists": _on_exists,
        "list": _on_list,
        "probe": _on_probe,
        "copy": _on_copy,
        "delete": _on_delete,
        "touchFile": _on_touch_file,
        "dbConnect": _on_db_connect,
        "dbPing": _on_db_ping,
        "dbQuery": _on_db_query,
        "dbExecute": _on_db_execute,
    }


def _write(obj: MutableMapping[str, Any]) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
