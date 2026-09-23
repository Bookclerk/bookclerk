"""Sparse out-of-tree workerd launcher (no Rust ``bookclerk-workerd`` binary).

Downloads the pinned Cloudflare ``workerd``, materializes Cap'n Proto + bridge
assets, and runs describe/health smoke tests. Public helpers are re-exported
from :mod:`.config`, :mod:`.ensure`, and :mod:`.smoke`.
"""

from .config import (
    GeneratedConfig,
    allocate_workerd_state_dir,
    egress_domains_for,
    materialize_config,
    plugin_global_outbound,
)
from .ensure import (
    binary_name,
    default_cache_dir,
    ensure_workerd,
    load_pin,
    package_root,
    platform_key,
    validate_fetch_url,
    validate_spawn_executable,
)
from ..path_guard import resolve_under
from .smoke import run_smoke

__all__ = [
    "GeneratedConfig",
    "allocate_workerd_state_dir",
    "binary_name",
    "default_cache_dir",
    "egress_domains_for",
    "ensure_workerd",
    "load_pin",
    "materialize_config",
    "package_root",
    "platform_key",
    "plugin_global_outbound",
    "resolve_under",
    "run_smoke",
    "validate_fetch_url",
    "validate_spawn_executable",
]
