#!/usr/bin/env python3
"""Extract Retrofit request params + Gson response field shapes from jadx sources.

APK analysis can recover the *declared* contract (paths, @Query/@Path/@Body fields,
and @SerializedName / field JSON keys on response DTOs). It cannot prove runtime
wire format alone — live smoke with credentials validates that.
"""

from __future__ import annotations

import re
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

# Simpler line-oriented parse for Kotlin→Java Retrofit interfaces.
ANNOT_LINE_RE = re.compile(
    r"^\s*@(GET|POST|PUT|DELETE|PATCH)\((?:value\s*=\s*)?\"([^\"]*)\"\)\s*$"
)
METHOD_SIG_RE = re.compile(
    r"^\s*(?:Object|[\w.<>,\s\?]+)\s+(\w+)\s*\((.*)\)\s*;?\s*$"
)
PARAM_ANN_RE = re.compile(
    r"@(Query|Path|Field|Part|Header|Body|Url|QueryMap|FieldMap)"
    r"(?:\(\"([^\"]*)\"\))?\s+"
    r"(?:final\s+)?([\w.<>,\s\?]+)\s+(\w+)"
)
RETURN_TYPE_RE = re.compile(
    r"Continuation<\? super NetworkResponse<(?P<inner>[^>]+)>>"
    r"|NetworkResponse<(?P<inner2>[^>]+)>"
    r"|Call<(?P<inner3>[^>]+)>"
)
SERIALIZED_RE = re.compile(
    r'@SerializedName\(\s*"([^"]+)"\s*\)\s*'
    r"(?:private|public|protected)?\s*(?:final\s+)?[\w.<>,\s\?]+\s+(\w+)\s*;",
    re.MULTILINE,
)
FIELD_RE = re.compile(
    r"(?:private|public|protected)\s+(?:final\s+)?([\w.<>,\s\?]+)\s+(\w+)\s*;",
    re.MULTILINE,
)
IMPORT_RE = re.compile(r"^\s*import\s+([\w.]+)\s*;", re.MULTILINE)
GENERIC_LIST_RE = re.compile(r"(?:List|Set|Collection|ArrayList)<\s*([\w.]+)\s*>")

# Endpoints we deep-track for schema drift.
# Auth-required (bookclerk-libro acquire path) + public catalog (CI smoke without secrets).
TRACKED_ENDPOINT_KEYS = {
    "library",
    "download-manifest",
    "audiobooks/{isbn}/packaged_m4b",
    "explore/audiobook_details/{isbn}",
    "explore/search",
    "explore/search/suggest",
    "explore/genres",
}


@dataclass
class RequestParam:
    kind: str  # Query, Path, Body, Field, QueryMap, ...
    name: str | None  # annotation value; None for @Body / @QueryMap
    java_type: str
    param_name: str


@dataclass
class EndpointContract:
    method: str
    path: str
    source: str
    function: str
    params: list[RequestParam] = field(default_factory=list)
    response_type: str | None = None
    request_body_type: str | None = None


@dataclass
class TypeShape:
    java_type: str
    source: str | None
    json_fields: dict[str, str] = field(default_factory=dict)  # json_key -> java_type
    children: dict[str, dict[str, Any]] = field(default_factory=dict)


def _simple_name(java_type: str) -> str:
    t = java_type.strip()
    t = t.replace("? extends ", "").replace("? super ", "").replace("?", "")
    t = t.split(".")[-1]
    return t.strip()


def _resolve_type_path(sources: Path, simple: str, imports: dict[str, str]) -> Path | None:
    if simple in {"String", "Integer", "Long", "Boolean", "Double", "Float", "Int", "Void", "Object"}:
        return None
    fqcn = imports.get(simple)
    if fqcn:
        rel = Path(*fqcn.split("."))
        candidate = sources / f"{rel}.java"
        if candidate.exists():
            return candidate
    # Search under fm/libro for the simple name.
    matches = list((sources / "fm" / "libro").rglob(f"{simple}.java"))
    if len(matches) == 1:
        return matches[0]
    if matches:
        # Prefer networking responses/requests.
        for m in matches:
            if "networking" in str(m):
                return m
        return matches[0]
    return None


def parse_java_type_shape(
    sources: Path,
    java_type: str,
    *,
    imports: dict[str, str] | None = None,
    depth: int = 0,
    seen: set[str] | None = None,
) -> TypeShape | None:
    """Resolve Gson JSON keys for a Java/Kotlin data class.

    ``seen`` tracks types on the *current* parent chain for cycle detection.
    Each nested field gets a copy so sibling fields that share a DTO type are
    not skipped after the first (which would hide schema drift).
    """
    if depth > 4:
        return None
    seen = seen or set()
    simple = _simple_name(java_type)
    if not simple or simple in seen:
        return None
    # Unwrap List<T>
    list_m = GENERIC_LIST_RE.search(java_type)
    if list_m:
        return parse_java_type_shape(
            sources, list_m.group(1), imports=imports, depth=depth, seen=seen
        )

    path = _resolve_type_path(sources, simple, imports or {})
    if path is None or not path.exists():
        return None
    # Branch-local set: ancestors + this type. Siblings do not share mutations.
    branch_seen = set(seen)
    branch_seen.add(simple)
    text = path.read_text(encoding="utf-8", errors="replace")
    local_imports = {m.group(1).split(".")[-1]: m.group(1) for m in IMPORT_RE.finditer(text)}
    if imports:
        local_imports = {**imports, **local_imports}

    shape = TypeShape(java_type=simple, source=str(path.relative_to(sources)))
    # Prefer @SerializedName pairs.
    annotated_fields: set[str] = set()
    for json_key, field_name in SERIALIZED_RE.findall(text):
        annotated_fields.add(field_name)
        type_m = re.search(
            rf"(?:private|public|protected)\s+(?:final\s+)?([\w.<>,\s\?]+)\s+{re.escape(field_name)}\s*;",
            text,
        )
        java_field_type = type_m.group(1).strip() if type_m else "Object"
        shape.json_fields[json_key] = java_field_type

    # Unannotated fields: Gson uses the Java field name as the JSON key.
    for field_type, field_name in FIELD_RE.findall(text):
        if field_name in annotated_fields:
            continue
        if field_name.startswith("$") or field_name in {"Companion", "INSTANCE"}:
            continue
        field_type = field_type.strip()
        if "static" in field_type.split():
            continue
        shape.json_fields[field_name] = field_type

    primitives = {
        "String",
        "Integer",
        "Long",
        "Boolean",
        "Double",
        "Float",
        "Int",
        "Void",
        "Object",
        "int",
        "long",
        "boolean",
        "double",
        "float",
    }
    for json_key, java_field_type in list(shape.json_fields.items()):
        nested_simple = _simple_name(java_field_type)
        list_m = GENERIC_LIST_RE.search(java_field_type)
        target = list_m.group(1) if list_m else nested_simple
        target_simple = _simple_name(target)
        if target_simple in primitives:
            continue
        child = parse_java_type_shape(
            sources,
            target_simple,
            imports=local_imports,
            depth=depth + 1,
            seen=branch_seen,
        )
        if child:
            shape.children[json_key] = {
                "java_type": child.java_type,
                "source": child.source,
                "json_fields": child.json_fields,
                "children": child.children,
            }
    return shape


def _parse_params(param_blob: str) -> tuple[list[RequestParam], str | None]:
    params: list[RequestParam] = []
    body_type: str | None = None
    # Normalize whitespace / newlines.
    blob = " ".join(param_blob.split())
    # Split on commas not inside generics is hard; find each @Annotation instead.
    for m in PARAM_ANN_RE.finditer(blob):
        kind, ann_name, java_type, param_name = m.groups()
        java_type = " ".join(java_type.split())
        if kind == "Body":
            body_type = java_type
            params.append(
                RequestParam(kind=kind, name=None, java_type=java_type, param_name=param_name)
            )
        elif kind in {"QueryMap", "FieldMap", "Url"}:
            params.append(
                RequestParam(kind=kind, name=None, java_type=java_type, param_name=param_name)
            )
        else:
            params.append(
                RequestParam(
                    kind=kind,
                    name=ann_name,
                    java_type=java_type,
                    param_name=param_name,
                )
            )
    return params, body_type


def _parse_return_type(sig_line: str, params_blob: str) -> str | None:
    combined = sig_line + " " + params_blob
    m = RETURN_TYPE_RE.search(combined)
    if not m:
        return None
    return (m.group("inner") or m.group("inner2") or m.group("inner3") or "").strip() or None


def parse_api_contracts(sources: Path) -> list[EndpointContract]:
    """Parse fm.libro *Api.java Retrofit interfaces into contracts with params."""
    roots = [sources / "fm" / "libro"]
    contracts: list[EndpointContract] = []
    seen: set[tuple[str, str, str]] = set()

    for root in roots:
        if not root.exists():
            continue
        for java in root.rglob("*Api.java"):
            if "Factory" in java.name or "Module" in java.name:
                continue
            text = java.read_text(encoding="utf-8", errors="replace")
            rel = str(java.relative_to(sources))
            imports = {m.group(1).split(".")[-1]: m.group(1) for m in IMPORT_RE.finditer(text)}
            lines = text.splitlines()
            i = 0
            while i < len(lines):
                am = ANNOT_LINE_RE.match(lines[i])
                if not am:
                    i += 1
                    continue
                method, path = am.group(1).upper(), am.group(2)
                # Next non-empty line(s) should be the method signature (may wrap).
                j = i + 1
                sig_parts: list[str] = []
                while j < len(lines):
                    stripped = lines[j].strip()
                    if not stripped or stripped.startswith("@"):
                        if sig_parts:
                            break
                        j += 1
                        continue
                    sig_parts.append(stripped)
                    if ")" in stripped:
                        break
                    j += 1
                sig = " ".join(sig_parts)
                sm = METHOD_SIG_RE.match(sig) or METHOD_SIG_RE.search(sig)
                if not sm:
                    # Object foo(...) form without explicit return generics on same pattern.
                    alt = re.search(r"\b(\w+)\s*\((.*)\)\s*;?", sig)
                    if not alt:
                        i += 1
                        continue
                    func_name, param_blob = alt.group(1), alt.group(2)
                else:
                    func_name, param_blob = sm.group(1), sm.group(2)

                if func_name in {"if", "for", "while", "switch"}:
                    i += 1
                    continue

                key = (method, path, func_name)
                if key in seen:
                    i = j + 1
                    continue
                seen.add(key)

                params, body_type = _parse_params(param_blob)
                response_type = _parse_return_type(sig, param_blob)
                # Resolve short names via imports for reporting.
                if response_type:
                    sn = _simple_name(response_type)
                    if sn in imports:
                        response_type = imports[sn].split(".")[-1]
                    else:
                        response_type = sn
                if body_type:
                    body_type = _simple_name(body_type)

                contracts.append(
                    EndpointContract(
                        method=method,
                        path=path,
                        source=rel,
                        function=func_name,
                        params=params,
                        response_type=response_type,
                        request_body_type=body_type,
                    )
                )
                i = j + 1
                continue
            i += 1

    contracts.sort(key=lambda c: (c.path, c.method, c.function))
    return contracts


def build_tracked_schema(sources: Path, contracts: list[EndpointContract]) -> dict[str, Any]:
    """Deep schema for endpoints bookclerk-libro uses."""
    by_path = {c.path: c for c in contracts}
    out: dict[str, Any] = {}
    for rel in TRACKED_ENDPOINT_KEYS:
        c = by_path.get(rel)
        if not c:
            out[rel] = {"missing_in_apk": True}
            continue
        imports: dict[str, str] = {}
        src_path = sources / c.source
        if src_path.exists():
            imports = {
                m.group(1).split(".")[-1]: m.group(1)
                for m in IMPORT_RE.finditer(src_path.read_text(encoding="utf-8", errors="replace"))
            }
        entry: dict[str, Any] = {
            "method": c.method,
            "path": c.path,
            "function": c.function,
            "source": c.source,
            "query": sorted(
                p.name for p in c.params if p.kind == "Query" and p.name
            ),
            "path_params": sorted(
                p.name for p in c.params if p.kind == "Path" and p.name
            ),
            "has_query_map": any(p.kind == "QueryMap" for p in c.params),
            "request_body_type": c.request_body_type,
            "response_type": c.response_type,
        }
        if c.request_body_type:
            body_shape = parse_java_type_shape(
                sources, c.request_body_type, imports=imports
            )
            if body_shape:
                entry["request_json_fields"] = sorted(body_shape.json_fields.keys())
                entry["request_field_types"] = body_shape.json_fields
        if c.response_type:
            resp_shape = parse_java_type_shape(
                sources, c.response_type, imports=imports
            )
            if resp_shape:
                entry["response_json_fields"] = sorted(resp_shape.json_fields.keys())
                entry["response_field_types"] = resp_shape.json_fields
                entry["response_children"] = resp_shape.children
        out[rel] = entry

    # OAuth is not a Retrofit *Api path in the same style — pull AuthPasswordRequest /
    # AuthResponse directly when present.
    auth_req = parse_java_type_shape(sources, "AuthPasswordRequest")
    auth_resp = parse_java_type_shape(sources, "AuthResponse")
    oauth: dict[str, Any] = {
        "method": "POST",
        "path": "/oauth/token",
    }
    if auth_req:
        oauth["request_json_fields"] = sorted(auth_req.json_fields.keys())
        oauth["request_field_types"] = auth_req.json_fields
    if auth_resp:
        oauth["response_json_fields"] = sorted(auth_resp.json_fields.keys())
        oauth["response_field_types"] = auth_resp.json_fields
    out["oauth/token"] = oauth
    return out


def _child_field_sets(shape: dict[str, Any]) -> dict[str, set[str]]:
    """Map nested response child key → set of JSON field names."""
    out: dict[str, set[str]] = {}
    children = shape.get("response_children") or {}
    if not isinstance(children, dict):
        return out
    for child_key, child in children.items():
        if not isinstance(child, dict):
            continue
        fields = child.get("json_fields") or {}
        if isinstance(fields, dict):
            out[child_key] = set(fields.keys())
        elif isinstance(fields, list):
            out[child_key] = set(fields)
    return out


def compare_shapes(
    apk_shapes: dict[str, Any],
    expected: dict[str, Any],
) -> list[dict[str, Any]]:
    """Compare APK-extracted shapes to the committed expected_shapes.json."""
    drifts: list[dict[str, Any]] = []
    for key, exp in expected.items():
        if key.startswith("_") or not isinstance(exp, dict):
            continue
        apk = apk_shapes.get(key) or {}
        if apk.get("missing_in_apk"):
            drifts.append(
                {
                    "field": f"schema.{key}",
                    "apk": None,
                    "client": "expected present",
                    "severity": "error",
                    "detail": "tracked endpoint missing from APK Retrofit surfaces",
                }
            )
            continue
        if "has_query_map" in exp:
            apk_flag = bool(apk.get("has_query_map"))
            if bool(exp["has_query_map"]) != apk_flag:
                drifts.append(
                    {
                        "field": f"schema.{key}.has_query_map",
                        "apk": apk_flag,
                        "client": exp["has_query_map"],
                        "severity": "info",
                    }
                )
        for list_key, severity in (
            ("query", "error"),
            ("path_params", "error"),
            ("request_json_fields", "error"),
            ("response_json_fields", "info"),
        ):
            exp_set = set(exp.get(list_key) or [])
            apk_set = set(apk.get(list_key) or [])
            if not exp_set and not apk_set:
                continue
            missing = sorted(exp_set - apk_set)
            extra = sorted(apk_set - exp_set)
            if missing:
                drifts.append(
                    {
                        "field": f"schema.{key}.{list_key}.missing_in_apk",
                        "apk": missing,
                        "client": sorted(exp_set),
                        "severity": severity,
                    }
                )
            if extra:
                drifts.append(
                    {
                        "field": f"schema.{key}.{list_key}.extra_in_apk",
                        "apk": extra,
                        "client": sorted(exp_set),
                        "severity": "info",
                    }
                )
        # Nested DTO fields (e.g. download-manifest.tracks → ApiTrack).
        exp_children = _child_field_sets(exp)
        apk_children = _child_field_sets(apk)
        for child_key, exp_fields in exp_children.items():
            apk_fields = apk_children.get(child_key) or set()
            # Expected may list a subset of APK fields (stable CI core).
            missing = sorted(exp_fields - apk_fields)
            if missing:
                drifts.append(
                    {
                        "field": f"schema.{key}.response_children.{child_key}.missing_in_apk",
                        "apk": missing,
                        "client": sorted(exp_fields),
                        "severity": "error",
                    }
                )
    return drifts


def contracts_as_dict(contracts: list[EndpointContract]) -> list[dict[str, Any]]:
    return [
        {
            **asdict(c),
            "params": [asdict(p) for p in c.params],
        }
        for c in contracts
    ]


def json_key_tree(value: Any, *, max_depth: int = 3, depth: int = 0) -> Any:
    """Summarize a live JSON value as a key tree for schema comparison."""
    if depth >= max_depth:
        return type(value).__name__
    if isinstance(value, dict):
        return {k: json_key_tree(v, max_depth=max_depth, depth=depth + 1) for k, v in value.items()}
    if isinstance(value, list):
        if not value:
            return []
        return [json_key_tree(value[0], max_depth=max_depth, depth=depth + 1)]
    return type(value).__name__


def top_level_keys(value: Any) -> list[str]:
    if isinstance(value, dict):
        return sorted(value.keys())
    return []
