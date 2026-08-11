"""Bookclerk Python plugin guest SDK.

Provides the dual-stack guest surface for native stdio Workers RPC and
(via :mod:`bookclerk_plugin_sdk.workerd`) Cloudflare Python Workers.

Authors subclass :class:`BookclerkPlugin` and either call
:meth:`BookclerkPluginGuest.serve` (native) or export a
``WorkerEntrypoint`` subclass under workerd. See ``docs/plugins.md`` and
``docs/code-documentation.md``.

Typical imports:

- Native: ``from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest``
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
