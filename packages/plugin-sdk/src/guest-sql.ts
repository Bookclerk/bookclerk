/**
 * Guest SQL statement classification — must match the host
 * `guest_statement_kind` in `bookclerk-plugin-abi`.
 */

import type { DbStatementKind } from "./db-execute.js";

function identStart(c: string): boolean {
  const code = c.charCodeAt(0);
  return (code >= 65 && code <= 90) || (code >= 97 && code <= 122) || c === "_";
}

function identCont(c: string): boolean {
  return identStart(c) || (c >= "0" && c <= "9");
}

function skipWsComments(sql: string, start: number): number {
  let i = start;
  while (i < sql.length) {
    while (i < sql.length && /\s/.test(sql[i]!)) {
      i += 1;
    }
    if (sql[i] === "-" && sql[i + 1] === "-") {
      i += 2;
      while (i < sql.length && sql[i] !== "\n") {
        i += 1;
      }
      continue;
    }
    if (sql[i] === "/" && sql[i + 1] === "*") {
      i += 2;
      while (i + 1 < sql.length && !(sql[i] === "*" && sql[i + 1] === "/")) {
        i += 1;
      }
      i = Math.min(i + 2, sql.length);
      continue;
    }
    break;
  }
  return i;
}

function keywordAt(sql: string, i: number, kw: string): boolean {
  if (i + kw.length > sql.length) {
    return false;
  }
  if (sql.slice(i, i + kw.length).toLowerCase() !== kw.toLowerCase()) {
    return false;
  }
  const beforeOk = i === 0 || !identCont(sql[i - 1]!);
  const after = sql[i + kw.length] ?? " ";
  return beforeOk && !identCont(after);
}

function skipIdentOrQuoted(sql: string, i: number): number | null {
  const c = sql[i];
  if (c === undefined) {
    return null;
  }
  if (c === '"' || c === "`" || c === "[") {
    const end = c === "[" ? "]" : c;
    let j = i + 1;
    while (j < sql.length) {
      if (sql[j] === end) {
        if (end !== "]" && sql[j + 1] === end) {
          j += 2;
          continue;
        }
        return j + 1;
      }
      j += 1;
    }
    return null;
  }
  if (identStart(c)) {
    let j = i + 1;
    while (j < sql.length && identCont(sql[j]!)) {
      j += 1;
    }
    return j;
  }
  return null;
}

function skipBalancedParens(sql: string, start: number): number | null {
  if (sql[start] !== "(") {
    return null;
  }
  let depth = 0;
  let end: number | null = null;
  forEachUnquoted(sql.slice(start), (slice, idx) => {
    if (end !== null) {
      return 1;
    }
    const c = slice[idx]!;
    if (c === "(") {
      depth += 1;
    } else if (c === ")") {
      depth -= 1;
      if (depth === 0) {
        end = start + idx + 1;
      }
    }
    return 1;
  });
  return end;
}

function sqlAfterLeadingCtes(sql: string): string {
  let i = skipWsComments(sql, 0);
  if (!keywordAt(sql, i, "WITH")) {
    return sql;
  }
  i += 4;
  i = skipWsComments(sql, i);
  if (keywordAt(sql, i, "RECURSIVE")) {
    i += 9;
    i = skipWsComments(sql, i);
  }
  for (;;) {
    const next = skipIdentOrQuoted(sql, i);
    if (next === null) {
      return sql;
    }
    i = skipWsComments(sql, next);
    if (sql[i] === "(") {
      const after = skipBalancedParens(sql, i);
      if (after === null) {
        return sql;
      }
      i = skipWsComments(sql, after);
    }
    if (!keywordAt(sql, i, "AS")) {
      return sql;
    }
    i = skipWsComments(sql, i + 2);
    if (keywordAt(sql, i, "NOT")) {
      const afterNot = skipWsComments(sql, i + 3);
      if (keywordAt(sql, afterNot, "MATERIALIZED")) {
        i = skipWsComments(sql, afterNot + 12);
      }
    } else if (keywordAt(sql, i, "MATERIALIZED")) {
      i = skipWsComments(sql, i + 12);
    }
    if (sql[i] !== "(") {
      return sql;
    }
    const afterBody = skipBalancedParens(sql, i);
    if (afterBody === null) {
      return sql;
    }
    i = skipWsComments(sql, afterBody);
    if (sql[i] === ",") {
      i = skipWsComments(sql, i + 1);
      continue;
    }
    return sql.slice(i);
  }
}

function forEachUnquoted(sql: string, step: (slice: string, index: number) => number): void {
  let i = 0;
  let inS = false;
  let inD = false;
  let inLine = false;
  let inBlock = false;
  while (i < sql.length) {
    const c = sql[i]!;
    if (inLine) {
      if (c === "\n") {
        inLine = false;
      }
      i += 1;
      continue;
    }
    if (inBlock) {
      if (c === "*" && sql[i + 1] === "/") {
        inBlock = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
    if (inS) {
      if (c === "'") {
        if (sql[i + 1] === "'") {
          i += 2;
          continue;
        }
        inS = false;
      }
      i += 1;
      continue;
    }
    if (inD) {
      if (c === '"') {
        if (sql[i + 1] === '"') {
          i += 2;
          continue;
        }
        inD = false;
      }
      i += 1;
      continue;
    }
    if (c === "-" && sql[i + 1] === "-") {
      inLine = true;
      i += 2;
      continue;
    }
    if (c === "/" && sql[i + 1] === "*") {
      inBlock = true;
      i += 2;
      continue;
    }
    if (c === "'") {
      inS = true;
      i += 1;
      continue;
    }
    if (c === '"') {
      inD = true;
      i += 1;
      continue;
    }
    const n = Math.max(1, step(sql, i));
    i += n;
  }
}

function forEachTopLevelKeyword(sql: string, onKeyword: (index: number, kw: string) => void): void {
  let depth = 0;
  forEachUnquoted(sql, (slice, idx) => {
    const c = slice[idx]!;
    if (c === "(") {
      depth += 1;
      return 1;
    }
    if (c === ")") {
      depth = Math.max(0, depth - 1);
      return 1;
    }
    if (depth === 0 && identStart(c)) {
      let j = idx + 1;
      while (j < slice.length && identCont(slice[j]!)) {
        j += 1;
      }
      onKeyword(idx, slice.slice(idx, j));
      return j - idx;
    }
    return 1;
  });
}

function hasTopLevelKeyword(sql: string, keyword: string): boolean {
  const want = keyword.toUpperCase();
  let found = false;
  forEachTopLevelKeyword(sql, (_, kw) => {
    if (kw.toUpperCase() === want) {
      found = true;
    }
  });
  return found;
}

function firstTopLevelKeyword(sql: string): string | undefined {
  let first: string | undefined;
  forEachTopLevelKeyword(sql, (_, kw) => {
    if (first === undefined) {
      first = kw.toUpperCase();
    }
  });
  return first;
}

/**
 * Classify guest SQL kind to match the host canonical request hash.
 *
 * @param sql Canonical Bookclerk SQL (`?` placeholders).
 * @returns Cap'n `DbStatementKind` (`execute` | `select` | `returning`).
 */
export function guestStatementKind(sql: string): DbStatementKind {
  const main = sqlAfterLeadingCtes(sql);
  if (hasTopLevelKeyword(main, "RETURNING")) {
    return "returning";
  }
  const verb = firstTopLevelKeyword(main);
  if (verb === "SELECT" || verb === "VALUES") {
    return "select";
  }
  return "execute";
}

/**
 * Split Cloudflare-style `exec()` newline-separated queries.
 *
 * @param query Multi-statement SQL with one statement per line.
 * @returns Trimmed statements with trailing semicolons removed.
 */
export function splitExecQueries(query: string): string[] {
  return query
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => stripTrailingSemicolons(line).trim())
    .filter((line) => line.length > 0);
}

/**
 * Remove trailing `;` characters with a linear scan (no backtracking regex).
 *
 * @param line Already-trimmed SQL line.
 * @returns Line without its trailing semicolon run.
 */
function stripTrailingSemicolons(line: string): string {
  let end = line.length;
  while (end > 0 && line[end - 1] === ";") {
    end -= 1;
  }
  return line.slice(0, end);
}
