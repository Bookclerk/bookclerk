"""Bookclerk Python plugin guest SDK.

Provides the workerd guest surface via :mod:`bookclerk_plugin_sdk.workerd`.
Authors subclass :class:`BookclerkPlugin` and export the raw class. Native
guests use the Rust SDK (`serve` / `PluginRoot`). See ``docs/plugins.md``.

Typical import:

- Workerd: ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``
"""

from .abi import (
    API_VERSION,
    METHOD_NAMES,
    BrandDto,
    CliInvokeParams,
    CliInvokeResult,
    CliSchema,
    FetchTitleParams,
    HandshakeParams,
    HandshakeResult,
    HealthResult,
    LoginParams,
    PluginError,
    ScanParams,
    StatementDto,
)
from .native import BookclerkPlugin, BookclerkPluginGuest

__all__ = [
    "API_VERSION",
    "METHOD_NAMES",
    "BrandDto",
    "BookclerkPlugin",
    "BookclerkPluginGuest",
    "CliInvokeParams",
    "CliInvokeResult",
    "CliSchema",
    "FetchTitleParams",
    "HandshakeParams",
    "HandshakeResult",
    "HealthResult",
    "LoginParams",
    "PluginError",
    "ScanParams",
    "StatementDto",
]
