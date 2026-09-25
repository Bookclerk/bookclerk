"""Dependency-aware CI planner and executor for Bookclerk.

``plan`` maps changed paths to affected checks (stage 1) and their
prerequisites (stage 2) using ``cargo metadata`` plus ``relations.toml``.
``execute`` resolves the execution mode, validates the plan artifact in each
job, runs checks, and evaluates the CI Gate. See docs/ci.md and issue #157.
"""
