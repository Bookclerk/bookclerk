"""Workerd Python Workers template — package import for BookclerkPlugin.

Example main module for ``runtime = "workerd"`` guests. Authors copy and adapt
this file as ``modules/plugin.py``:

    from bookclerk_plugin_sdk.workerd import BookclerkPlugin, Integration, js

``plugin.toml`` should declare:

    runtime = "workerd"
    [workerd]
    main_module = "plugin.py"
    compatibility_flags = ["python_workers", "disable_python_external_sdk"]

``bookclerk-workerd`` injects the SDK. Native guests use Rust ``serve`` /
``PluginRoot``.
"""

from __future__ import annotations

from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

API_VERSION = 2
"""Handshake ``apiVersion`` returned by this template guest."""

PLUGIN_ID = "my_python_plugin"
"""Example plugin id; replace before shipping."""

KIND = "integration"
"""Example plugin kind (``source`` / ``integration`` / ``output`` / ``database``)."""


class Default(BookclerkPlugin):
    """Template workerd entrypoint exported as the default Workers binding."""

    async def handshake(self, _params=None):
        """Return a minimal successful handshake payload.

        Args:
            _params: Host handshake parameters (unused in the template).

        Returns:
            JS object with ``apiVersion``, ``id``, ``kind``, and capabilities.
        """
        return js(
            {
                "apiVersion": API_VERSION,
                "id": PLUGIN_ID,
                "kind": KIND,
                "displayName": "My Python Plugin",
                "capabilities": ["health", "diagnose", "cli"],
            }
        )

    async def health(self, _params=None):
        """Report template guest liveness.

        Args:
            _params: Optional health parameters from the host.

        Returns:
            JS object with ``ok``, ``id``, and a ready detail string.
        """
        return js({"ok": True, "id": PLUGIN_ID, "detail": "python workerd plugin ready"})

    async def diagnose(self, _params=None):
        """Return a single diagnostic line for the template guest.

        Args:
            _params: Optional diagnose parameters from the host.

        Returns:
            JS object with a ``lines`` list.
        """
        return js({"lines": [f"{PLUGIN_ID}: ok"]})

    async def cliInvoke(self, params=None):
        """Handle the sample ``ping`` CLI command.

        Args:
            params: Host CLI invoke payload with ``command`` and ``args``.

        Returns:
            JS object with ``exitCode`` and ``stdout``/``stderr``.
        """
        command = params.get("command") if hasattr(params, "get") else None
        if command != "ping":
            return js({"exitCode": 2, "stderr": f"unknown command {command}"})
        args = (params.get("args") if hasattr(params, "get") else {}) or {}
        message = args.get("message") if hasattr(args, "get") else "hi"
        if not isinstance(message, str):
            message = "hi"
        return js({"exitCode": 0, "stdout": f"pong: {message}\n"})
