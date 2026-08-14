"""Dependency-aware CI planner for Bookclerk.

Maps git path changes to Cargo workspace packages (via ``cargo metadata``),
computes reverse-transitive dependents, and emits a machine-readable plan for
selective CI jobs. See GitHub issue #157.
"""

from __future__ import annotations

from .plan import (
    FULL_SUITE_PATH_PREFIXES,
    FULL_SUITE_PATHS,
    Plan,
    PlanError,
    build_plan,
    load_metadata,
    package_index_from_metadata,
    plan_to_github_output,
    plan_to_json,
    plan_to_summary,
)

__all__ = [
    "FULL_SUITE_PATH_PREFIXES",
    "FULL_SUITE_PATHS",
    "Plan",
    "PlanError",
    "build_plan",
    "load_metadata",
    "package_index_from_metadata",
    "plan_to_github_output",
    "plan_to_json",
    "plan_to_summary",
]
