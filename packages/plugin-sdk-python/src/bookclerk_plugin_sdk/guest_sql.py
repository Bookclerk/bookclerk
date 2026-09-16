"""Guest SQL classification — must match host ``guest_statement_kind``."""

from __future__ import annotations

from typing import Literal

DbStatementKind = Literal["execute", "select", "returning"]


def _ident_start(c: str) -> bool:
    return c.isalpha() or c == "_"


def _ident_cont(c: str) -> bool:
    return _ident_start(c) or c.isdigit()


def _skip_ws_comments(sql: str, start: int) -> int:
    i = start
    while i < len(sql):
        while i < len(sql) and sql[i].isspace():
            i += 1
        if i + 1 < len(sql) and sql[i : i + 2] == "--":
            i += 2
            while i < len(sql) and sql[i] != "\n":
                i += 1
            continue
        if i + 1 < len(sql) and sql[i : i + 2] == "/*":
            i += 2
            while i + 1 < len(sql) and sql[i : i + 1] != "*/":
                i += 1
            i = min(i + 2, len(sql))
            continue
        break
    return i


def _keyword_at(sql: str, i: int, kw: str) -> bool:
    if i + len(kw) > len(sql):
        return False
    if sql[i : i + len(kw)].lower() != kw.lower():
        return False
    before_ok = i == 0 or not _ident_cont(sql[i - 1])
    after = sql[i + len(kw)] if i + len(kw) < len(sql) else " "
    return before_ok and not _ident_cont(after)


def _skip_ident_or_quoted(sql: str, i: int) -> int | None:
    if i >= len(sql):
        return None
    c = sql[i]
    if c in ('"', "`", "["):
        end = "]" if c == "[" else c
        j = i + 1
        while j < len(sql):
            if sql[j] == end:
                if end != "]" and j + 1 < len(sql) and sql[j + 1] == end:
                    j += 2
                    continue
                return j + 1
            j += 1
        return None
    if _ident_start(c):
        j = i + 1
        while j < len(sql) and _ident_cont(sql[j]):
            j += 1
        return j
    return None


def _skip_balanced_parens(sql: str, start: int) -> int | None:
    if start >= len(sql) or sql[start] != "(":
        return None
    depth = 0
    end: int | None = None

    def step(slice_sql: str, idx: int) -> int:
        nonlocal depth, end
        if end is not None:
            return 1
        c = slice_sql[idx]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                end = start + idx + 1
        return 1

    _for_each_unquoted(sql[start:], step)
    return end


def _sql_after_leading_ctes(sql: str) -> str:
    i = _skip_ws_comments(sql, 0)
    if not _keyword_at(sql, i, "WITH"):
        return sql
    i += 4
    i = _skip_ws_comments(sql, i)
    if _keyword_at(sql, i, "RECURSIVE"):
        i += 9
        i = _skip_ws_comments(sql, i)
    while True:
        nxt = _skip_ident_or_quoted(sql, i)
        if nxt is None:
            return sql
        i = _skip_ws_comments(sql, nxt)
        if i < len(sql) and sql[i] == "(":
            after = _skip_balanced_parens(sql, i)
            if after is None:
                return sql
            i = _skip_ws_comments(sql, after)
        if not _keyword_at(sql, i, "AS"):
            return sql
        i = _skip_ws_comments(sql, i + 2)
        if _keyword_at(sql, i, "NOT"):
            after_not = _skip_ws_comments(sql, i + 3)
            if _keyword_at(sql, after_not, "MATERIALIZED"):
                i = _skip_ws_comments(sql, after_not + 12)
        elif _keyword_at(sql, i, "MATERIALIZED"):
            i = _skip_ws_comments(sql, i + 12)
        if i >= len(sql) or sql[i] != "(":
            return sql
        after_body = _skip_balanced_parens(sql, i)
        if after_body is None:
            return sql
        i = _skip_ws_comments(sql, after_body)
        if i < len(sql) and sql[i] == ",":
            i = _skip_ws_comments(sql, i + 1)
            continue
        return sql[i:]


def _for_each_unquoted(sql: str, step) -> None:
    i = 0
    in_s = in_d = in_line = in_block = False
    while i < len(sql):
        c = sql[i]
        if in_line:
            if c == "\n":
                in_line = False
            i += 1
            continue
        if in_block:
            if c == "*" and i + 1 < len(sql) and sql[i + 1] == "/":
                in_block = False
                i += 2
                continue
            i += 1
            continue
        if in_s:
            if c == "'":
                if i + 1 < len(sql) and sql[i + 1] == "'":
                    i += 2
                    continue
                in_s = False
            i += 1
            continue
        if in_d:
            if c == '"':
                if i + 1 < len(sql) and sql[i + 1] == '"':
                    i += 2
                    continue
                in_d = False
            i += 1
            continue
        if i + 1 < len(sql) and sql[i : i + 2] == "--":
            in_line = True
            i += 2
            continue
        if i + 1 < len(sql) and sql[i : i + 2] == "/*":
            in_block = True
            i += 2
            continue
        if c == "'":
            in_s = True
            i += 1
            continue
        if c == '"':
            in_d = True
            i += 1
            continue
        n = max(1, step(sql, i))
        i += n


def _for_each_top_level_keyword(sql: str, on_keyword) -> None:
    depth = 0

    def step(slice_sql: str, idx: int) -> int:
        nonlocal depth
        c = slice_sql[idx]
        if c == "(":
            depth += 1
            return 1
        if c == ")":
            depth = max(0, depth - 1)
            return 1
        if depth == 0 and _ident_start(c):
            j = idx + 1
            while j < len(slice_sql) and _ident_cont(slice_sql[j]):
                j += 1
            on_keyword(idx, slice_sql[idx:j])
            return j - idx
        return 1

    _for_each_unquoted(sql, step)


def _has_top_level_keyword(sql: str, keyword: str) -> bool:
    want = keyword.upper()
    found = False

    def on_keyword(_idx: int, kw: str) -> None:
        nonlocal found
        if kw.upper() == want:
            found = True

    _for_each_top_level_keyword(sql, on_keyword)
    return found


def _first_top_level_keyword(sql: str) -> str | None:
    first: str | None = None

    def on_keyword(_idx: int, kw: str) -> None:
        nonlocal first
        if first is None:
            first = kw.upper()

    _for_each_top_level_keyword(sql, on_keyword)
    return first


def guest_statement_kind(sql: str) -> DbStatementKind:
    """Classify guest SQL kind to match the host canonical request hash."""
    main = _sql_after_leading_ctes(sql)
    if _has_top_level_keyword(main, "RETURNING"):
        return "returning"
    verb = _first_top_level_keyword(main)
    if verb in {"SELECT", "VALUES"}:
        return "select"
    return "execute"


def split_exec_queries(query: str) -> list[str]:
    """Split Cloudflare-style ``exec()`` newline-separated queries."""
    out: list[str] = []
    for line in query.split("\n"):
        trimmed = line.strip()
        if not trimmed:
            continue
        # Linear strip of the trailing semicolon run (no backtracking regex).
        trimmed = trimmed.rstrip(";").strip()
        if trimmed:
            out.append(trimmed)
    return out
