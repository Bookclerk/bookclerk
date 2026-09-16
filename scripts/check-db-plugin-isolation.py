#!/usr/bin/env python3
"""Forbid database plugins from embedding Bookclerk domain SQL.

Database guests execute host-authored generic plans. Plugin sources must not
import `bookclerk-library` entities or mention application table identifiers.
See docs/adr/sql-database-contract.md.

Usage:
    python3 scripts/check-db-plugin-isolation.py
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLUGIN_GLOBS = (
    "crates/bookclerk-plugins/platform/database-*/src/**/*.rs",
    "crates/bookclerk-plugins/optional/database-*/src/**/*.rs",
    "crates/bookclerk-plugin-sdk/src/database_adapter/**/*.rs",
)

FORBIDDEN_IMPORTS = (
    re.compile(r"bookclerk_library::entities"),
    re.compile(r"use\s+bookclerk_library::entities"),
    re.compile(r"bookclerk_library::sql_plan"),
    re.compile(r"bookclerk_library::migrations"),
    re.compile(r"\bmigration_sql\b"),
    re.compile(r"\bmigration_sql_postgres\b"),
    re.compile(r"\bmigration_sql_d1\b"),
    re.compile(r"\binterpret_plan\b"),
    re.compile(r"\bDbAtomicParams\b"),
    re.compile(r"\batomic_status\b"),
    # Host semantic frontend — adapters must not rerun it.
    re.compile(r"\bdesugar_canonical_sql\b"),
    re.compile(r"\bdesugar_execute_request\b"),
    re.compile(r"\bUnresolvedExecuteRequest\b"),
)

# Application tables the host owns. Guests must not mention these identifiers.
FORBIDDEN_TABLES = (
    "domain_events",
    "event_deliveries",
    "event_outbox_stats",
    "event_subscriber_nodes",
    "users",
    "jobs",
    "job_temp_paths",
    "job_queue_control",
    "books",
    "accounts",
    "claim_tickets",
    "oidc_rp_states",
    "webauthn_challenges",
    "portal_identities",
    "portal_sessions",
    "operator_sessions",
    "encrypted_secrets",
    "db_atomic_receipts",
    "db_serialization_slots",
)

TABLE_RE = re.compile(
    r"(?:FROM|INTO|UPDATE|JOIN|TABLE|EXISTS)\s+(?:IF\s+NOT\s+EXISTS\s+)?"
    r"(" + "|".join(re.escape(t) for t in FORBIDDEN_TABLES) + r")\b",
    re.IGNORECASE,
)

JSON_EACH_RE = re.compile(r"\bjson_each\b", re.IGNORECASE)

# Production database guests must not take non-optional production deps on these
# crates. Use `bookclerk-plugin-sdk` (feature `db`) and `bookclerk-db-exec`
# instead. Optional (`host-helpers`) and `[dev-dependencies]` are allowed.
LIBRARY_ISOLATION_PACKAGES = (
    "bookclerk-plugin-database-sqlite",
    "bookclerk-plugin-database-postgres",
    "bookclerk-plugin-database-d1",
)

FORBIDDEN_PRODUCTION_DEPS = (
    "bookclerk-library",
)


def check_cargo_metadata() -> list[str]:
    """Refuse forbidden production deps on database guest crates.

    Optional (`host-helpers`) and `[dev-dependencies]` are allowed, but the
    default/release feature graph must not enable `bookclerk-library`.
    """
    proc = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        return [f"cargo metadata failed: {proc.stderr.strip() or proc.stdout.strip()}"]
    meta = json.loads(proc.stdout)
    hits: list[str] = []
    for pkg in meta.get("packages", []):
        if pkg.get("name") not in LIBRARY_ISOLATION_PACKAGES:
            continue
        manifest = Path(pkg.get("manifest_path", ""))
        try:
            manifest.relative_to(ROOT)
        except ValueError:
            continue
        features = pkg.get("features") or {}
        default_enabled = expand_feature_graph(features, "default")
        if "host-helpers" in (features.get("default") or []):
            hits.append(
                f"{pkg['name']}: default features enable host-helpers "
                "(production `cargo build -p` would compile bookclerk-library)"
            )
        if "bookclerk-library" in default_enabled:
            hits.append(
                f"{pkg['name']}: default/release feature graph depends on bookclerk-library"
            )
        for dep in pkg.get("dependencies", []):
            dep_name = dep.get("name")
            if dep_name not in FORBIDDEN_PRODUCTION_DEPS:
                continue
            kind = dep.get("kind")
            optional = bool(dep.get("optional"))
            if kind in (None, "normal") and not optional:
                hits.append(
                    f"{pkg['name']}: non-optional dependency on {dep_name} "
                    "(use bookclerk-db-guest for first-party session workers, "
                    "bookclerk-plugin-sdk::database_adapter for author helpers, "
                    "optional host-helpers, or a dev-dependency)"
                )
    return hits


def expand_feature_graph(features: dict, name: str) -> set[str]:
    """Expands Cargo feature `name` into enabled feature/dep names."""
    seen: set[str] = set()
    stack = list(features.get(name) or [])
    while stack:
        item = stack.pop()
        if item in seen:
            continue
        seen.add(item)
        if item.startswith("dep:"):
            seen.add(item[4:])
            continue
        if "/" in item:
            seen.add(item.split("/", 1)[0])
            continue
        stack.extend(features.get(item) or [])
    return seen


def iter_plugin_sources() -> list[Path]:
    files: list[Path] = []
    for glob in PLUGIN_GLOBS:
        files.extend(ROOT.glob(glob))
    return sorted({p.resolve() for p in files if p.is_file()})


def strip_cfg_test_regions(src: str) -> str:
    """Replace test-only modules and `#[test]` / `#[tokio::test]` items with spaces."""
    out = list(src)
    markers = ("#[cfg(test)]", "#[test]", "#[tokio::test")
    i = 0
    while True:
        idx = min(
            (src.find(m, i) for m in markers if src.find(m, i) >= 0),
            default=-1,
        )
        if idx < 0:
            break
        brace = src.find("{", idx)
        if brace < 0:
            break
        depth = 0
        j = brace
        while j < len(src):
            ch = src[j]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    j += 1
                    break
            j += 1
        for k in range(idx, min(j, len(src))):
            if out[k] != "\n":
                out[k] = " "
        i = j
    return "".join(out)


def strip_comments(src: str) -> str:
    """Blank line/block comments while preserving newlines."""
    out: list[str] = []
    i = 0
    n = len(src)
    while i < n:
        if src.startswith("//", i):
            while i < n and src[i] != "\n":
                out.append(" ")
                i += 1
            continue
        if src.startswith("/*", i):
            while i + 1 < n and not src.startswith("*/", i):
                out.append("\n" if src[i] == "\n" else " ")
                i += 1
            out.extend("  ")
            i = min(i + 2, n)
            continue
        out.append(src[i])
        i += 1
    return "".join(out)


def check_file(path: Path) -> list[str]:
    rel = path.relative_to(ROOT).as_posix()
    src = path.read_text(encoding="utf-8")
    scanned = strip_comments(strip_cfg_test_regions(src))
    hits: list[str] = []
    for rx in FORBIDDEN_IMPORTS:
        for m in rx.finditer(scanned):
            line = scanned.count("\n", 0, m.start()) + 1
            hits.append(
                f"{rel}:{line}: forbidden host/domain symbol `{m.group(0)}` in database plugin"
            )
    for m in TABLE_RE.finditer(scanned):
        line = scanned.count("\n", 0, m.start()) + 1
        hits.append(f"{rel}:{line}: application table identifier `{m.group(1)}`")
    for m in JSON_EACH_RE.finditer(scanned):
        line = scanned.count("\n", 0, m.start()) + 1
        hits.append(f"{rel}:{line}: guest SQL must not use json_each (host filters in Rust)")
    return hits


# Host domain/planner crates must not read bootstrap-only engine identity.
LIBRARY_BOOTSTRAP_GLOBS = ("crates/bookclerk-library/src/**/*.rs",)

FORBIDDEN_BOOTSTRAP_READS = (
    re.compile(r"\bSqlFamily\b"),
    re.compile(r"\bfamily_from_connect\b"),
    re.compile(r"\bmod\s+dialect\b"),
    re.compile(r"\.sql_family\b"),
    re.compile(r"\bsql_family\s*:"),
    re.compile(r"\.diagnostic_engine\b"),
    re.compile(r"\bdiagnostic_engine\s*:"),
)

# Host domain/planner crates must not rewrite canonical `?` into engine `$n`.
FORBIDDEN_LIBRARY_PLACEHOLDER_REWRITE = (
    re.compile(r"rewrite_sql_placeholders"),
    re.compile(r"""push\(['\"]\$['\"]\)"""),
    re.compile(r"""push_str\(['\"]\$['\"]\)"""),
)

# Adapter-owned physical lowering: library production sources must not call it.
FORBIDDEN_LIBRARY_LOWERING = (
    re.compile(r"\blower_canonical_sql\b"),
    re.compile(r"\blower_canonical_sql_typed\b"),
    re.compile(r"\blower_canonical_to_postgres\b"),
    re.compile(r"\blower_canonical_ddl_to_postgres\b"),
    re.compile(r"\bschema_sql_for_backend\b"),
    re.compile(r"\bexpand_host_schema_batch\b"),
    re.compile(r"\bpostgres_identity_companions\b"),
    re.compile(r"\bbinding_companions\b"),
    re.compile(r"\brealize_host_ddl\b"),
    re.compile(r"\brealize_binding_ddl\b"),
    re.compile(r"\bfrom_adapter_backend\b"),
    re.compile(r"\bexecute_physical_sql\b"),
    re.compile(r"\bquery_physical_sql\b"),
    re.compile(r"\bquery_physical_sql_typed\b"),
    re.compile(r"\bload_physical_sql_type_env\b"),
    re.compile(r"bookclerk_db_exec::execute_typed_on_session"),
    re.compile(r"bookclerk_db_exec::execute_typed_on_txn\b"),
    re.compile(r"bookclerk_db_exec::execute_typed_join_body"),
    re.compile(r"bookclerk_db_exec::execute_typed_envelope(?!_on)"),
    re.compile(r"\bHostExecuteEnvelope\b"),
    re.compile(r"\bget_database_backend\b"),
)

# Host backup/schema must not emit engine catalog or isolation SQL.
FORBIDDEN_LIBRARY_DIALECT_SQL = (
    re.compile(r"FROM\s+sqlite_sequence\b", re.IGNORECASE),
    re.compile(r"INTO\s+sqlite_sequence\b", re.IGNORECASE),
    re.compile(r"FROM\s+sqlite_master\b", re.IGNORECASE),
    re.compile(r"\binformation_schema\b", re.IGNORECASE),
    re.compile(r"REPEATABLE\s+READ", re.IGNORECASE),
    re.compile(r"defer_foreign_keys", re.IGNORECASE),
    re.compile(r"DROP\s+FUNCTION\b", re.IGNORECASE),
    re.compile(r"PRAGMA\s+user_version", re.IGNORECASE),
    re.compile(r"FROM\s+pragma_user_version\b", re.IGNORECASE),
)


def iter_library_sources() -> list[Path]:
    files: list[Path] = []
    for glob in LIBRARY_BOOTSTRAP_GLOBS:
        files.extend(ROOT.glob(glob))
    skip_names = {"tests.rs", "conformance.rs", "differential.rs"}
    return sorted(
        {
            p.resolve()
            for p in files
            if p.is_file() and p.name not in skip_names
        }
    )


def check_library_bootstrap_isolation(path: Path) -> list[str]:
    """Forbid planner/domain reads of sqlFamily / diagnosticEngine / SqlFamily."""
    rel = path.relative_to(ROOT).as_posix()
    src = path.read_text(encoding="utf-8")
    scanned = strip_comments(strip_cfg_test_regions(src))
    hits: list[str] = []
    for rx in FORBIDDEN_BOOTSTRAP_READS:
        for m in rx.finditer(scanned):
            line = scanned.count("\n", 0, m.start()) + 1
            hits.append(
                f"{rel}:{line}: bootstrap-only engine metadata `{m.group(0)}` "
                "must not drive host schema/plan/domain code "
                "(see docs/sql-contract/v1.md; SeaORM bootstrap stays in plugin-host)"
            )
    for rx in FORBIDDEN_LIBRARY_PLACEHOLDER_REWRITE:
        for m in rx.finditer(scanned):
            line = scanned.count("\n", 0, m.start()) + 1
            hits.append(
                f"{rel}:{line}: host SQL must stay canonical `?` (`{m.group(0)}`); "
                "adapter SDK lowers placeholders"
            )
    for rx in FORBIDDEN_LIBRARY_LOWERING:
        for m in rx.finditer(scanned):
            line = scanned.count("\n", 0, m.start()) + 1
            hits.append(
                f"{rel}:{line}: host must not call adapter lowering `{m.group(0)}`; "
                "send canonical SQL through AdapterDatabaseSession.execute"
            )
    for rx in FORBIDDEN_LIBRARY_DIALECT_SQL:
        for m in rx.finditer(scanned):
            line = scanned.count("\n", 0, m.start()) + 1
            hits.append(
                f"{rel}:{line}: host must not emit engine dialect SQL `{m.group(0)}`; "
                "adapters own snapshot/identity/restore primitives"
            )
    return hits


PLUGIN_HOST_GLOBS = ("crates/bookclerk-plugin-host/src/**/*.rs",)

FORBIDDEN_PLUGIN_HOST_LOWERING = (
    re.compile(r"\blower_canonical_sql\b"),
    re.compile(r"\blower_canonical_sql_typed\b"),
    re.compile(r"\blower_canonical_ddl_to_postgres\b"),
    re.compile(r"\bschema_sql_for_backend\b"),
    re.compile(r"\brealize_host_ddl\b"),
    re.compile(r"\brealize_binding_ddl\b"),
    re.compile(r"\bfrom_adapter_backend\b"),
    re.compile(r"\bexecute_physical_sql\b"),
    re.compile(r"\bquery_physical_sql\b"),
)

# Schema/SQL packs must stay already-separate statements. Cookie/header
# `split(';')` lives outside these globs (bookclerkd, media, OIDC).
SCHEMA_SPLIT_GLOBS = (
    "crates/bookclerk-plugins/platform/database-*/src/**/*.rs",
    "crates/bookclerk-plugins/optional/database-*/src/**/*.rs",
    "crates/bookclerk-plugin-sdk/src/database_adapter/**/*.rs",
    "crates/bookclerk-library/src/**/*.rs",
    "crates/bookclerk-db-exec/src/**/*.rs",
    "crates/bookclerk-db-guest/src/**/*.rs",
    "crates/bookclerk-plugin-host/src/**/*.rs",
    "crates/bookclerk-plugin-abi/src/**/*.rs",
)

SPLIT_SEMI = re.compile(r"""\.split\(\s*(['"]);\1\s*\)""")

SQLITE_FALLBACK_GLOBS = (
    "crates/bookclerk-library/src/**/*.rs",
    "crates/bookclerk-plugin-host/src/**/*.rs",
    "crates/bookclerk-db-guest/src/**/*.rs",
    "crates/bookclerk-db-exec/src/**/*.rs",
)

SQLITE_FALLBACK = re.compile(
    r"_ =>\s*(?:Self::Sqlite|PhysicalEngine::sqlite(?:\(\))?|"
    r"DbCapabilities::advertised_sqlite\(\)|advertised_sqlite\(\)|"
    r"DatabaseBackend::Sqlite|DbBackend::Sqlite)",
)

LIBRARY_BACKEND_IDENT = re.compile(r"\b(?:DatabaseBackend|DbBackend)::")
LIBRARY_PHYSICAL_ENGINE = re.compile(r"\bPhysicalEngine\b")


def iter_plugin_host_sources() -> list[Path]:
    files: list[Path] = []
    for glob in PLUGIN_HOST_GLOBS:
        files.extend(ROOT.glob(glob))
    return sorted({p.resolve() for p in files if p.is_file()})


def check_plugin_host_isolation(path: Path) -> list[str]:
    """Forbid plugin-host production from calling adapter physical lowering."""
    rel = path.relative_to(ROOT).as_posix()
    src = path.read_text(encoding="utf-8")
    scanned = strip_comments(strip_cfg_test_regions(src))
    hits: list[str] = []
    for rx in FORBIDDEN_PLUGIN_HOST_LOWERING:
        for m in rx.finditer(scanned):
            line = scanned.count("\n", 0, m.start()) + 1
            hits.append(
                f"{rel}:{line}: plugin-host must not call adapter lowering `{m.group(0)}`; "
                "send canonical AdapterExecuteRequest"
            )
    return hits


def iter_glob_sources(globs: tuple[str, ...], skip_names: set[str] | None = None) -> list[Path]:
    skip = skip_names or set()
    files: list[Path] = []
    for glob in globs:
        files.extend(ROOT.glob(glob))
    return sorted(
        {
            p.resolve()
            for p in files
            if p.is_file() and p.name not in skip
        }
    )


def check_no_schema_split(path: Path) -> list[str]:
    """Forbid reconstructing statement lists with `split(';')`."""
    rel = path.relative_to(ROOT).as_posix()
    src = path.read_text(encoding="utf-8")
    scanned = strip_comments(strip_cfg_test_regions(src))
    hits: list[str] = []
    for m in SPLIT_SEMI.finditer(scanned):
        line = scanned.count("\n", 0, m.start()) + 1
        hits.append(
            f"{rel}:{line}: schema/SQL packs must be already-separate statements "
            f"(`{m.group(0)}`); do not split on ';'"
        )
    return hits


def check_no_sqlite_fallback(path: Path) -> list[str]:
    """Forbid `_ => sqlite` catch-alls; unknown engines fail closed."""
    rel = path.relative_to(ROOT).as_posix()
    src = path.read_text(encoding="utf-8")
    scanned = strip_comments(strip_cfg_test_regions(src))
    hits: list[str] = []
    for m in SQLITE_FALLBACK.finditer(scanned):
        line = scanned.count("\n", 0, m.start()) + 1
        hits.append(
            f"{rel}:{line}: unknown SeaORM backends must fail closed "
            f"(`{m.group(0).strip()}`); do not default to SQLite"
        )
    return hits


def check_library_no_backend_ident(path: Path) -> list[str]:
    """Forbid SeaORM backend identity and PhysicalEngine in library production."""
    rel = path.relative_to(ROOT).as_posix()
    src = path.read_text(encoding="utf-8")
    scanned = strip_comments(strip_cfg_test_regions(src))
    hits: list[str] = []
    for m in LIBRARY_BACKEND_IDENT.finditer(scanned):
        line = scanned.count("\n", 0, m.start()) + 1
        hits.append(
            f"{rel}:{line}: library production must not name `{m.group(0)}`; "
            "canonical transport lives in bookclerk-db-exec; adapters own engines"
        )
    for m in LIBRARY_PHYSICAL_ENGINE.finditer(scanned):
        line = scanned.count("\n", 0, m.start()) + 1
        hits.append(
            f"{rel}:{line}: library production must not name `PhysicalEngine`; "
            "stamp AdapterExecuteRequest and let adapters infer the engine"
        )
    return hits


def main() -> int:
    files = iter_plugin_sources()
    if not files:
        print("check-db-plugin-isolation: no database plugin sources found", file=sys.stderr)
        return 2
    hits: list[str] = []
    for path in files:
        hits.extend(check_file(path))
    hits.extend(check_cargo_metadata())
    library_files = iter_library_sources()
    for path in library_files:
        hits.extend(check_library_bootstrap_isolation(path))
        hits.extend(check_library_no_backend_ident(path))
    host_files = iter_plugin_host_sources()
    for path in host_files:
        hits.extend(check_plugin_host_isolation(path))
    skip_tests = {"tests.rs", "conformance.rs", "differential.rs"}
    for path in iter_glob_sources(SCHEMA_SPLIT_GLOBS, skip_tests):
        hits.extend(check_no_schema_split(path))
    for path in iter_glob_sources(SQLITE_FALLBACK_GLOBS, skip_tests):
        hits.extend(check_no_sqlite_fallback(path))
    if hits:
        print("Database plugin isolation violations:", file=sys.stderr)
        for h in hits:
            print(f"  {h}", file=sys.stderr)
        print(
            "Guests must execute host-authored plans only; host selects schema versions. "
            "Host SQL stays canonical; adapters own physical lowering. "
            "Diagnostic engine identity must not admit or generate SQL. "
            "See docs/adr/sql-database-contract.md and docs/sql-contract/v1.md",
            file=sys.stderr,
        )
        return 1
    print(
        f"ok: {len(files)} database plugin sources + {len(library_files)} library sources "
        f"+ {len(host_files)} plugin-host sources + cargo metadata"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
