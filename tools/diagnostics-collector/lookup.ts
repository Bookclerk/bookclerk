/**
 * Report lookup for the people who sent a report: `/lookup` and
 * `/lookup/delete`, opened by the report's access code (report-access.ts).
 *
 * GET  /lookup         — public page with a form for a report ID and code
 * POST /lookup         — the report, its issues and fix. A form body
 *                        (the page) gets an HTML page; a JSON body (what
 *                        Bookclerk's Settings and CLI send) gets JSON.
 * POST /lookup/delete  — deletes the stored report from B2. The dashboard's
 *                        index row and issue links stay, so the report still
 *                        counts; only the report body is gone. A form must
 *                        tick `confirm=yes`.
 *
 * A wrong code and an unknown report get the same 404, so the routes do not
 * say which reports exist. Attempts are rate limited per report ID
 * (LOOKUP_LIMIT). No cookies are involved: the code in the body is the only
 * credential, so there is nothing for another site to ride on.
 */

import { getReport } from "./dashboard/db.ts";
import { htmlResponse, type SafeHtml } from "./dashboard/html.ts";
import {
  lookupDeletedPage,
  lookupFormPage,
  lookupMessagePage,
  lookupResultPage,
  type LookupView,
} from "./dashboard/pages.ts";
import { readStoredReport, type B2Access } from "./dashboard/storage.ts";
import type { Env } from "./env.ts";
import { asObject, type JsonObject } from "./json.ts";
import {
  MAX_LOOKUP_CHARS,
  checkAccess,
  markDeleted,
  parseLookupInput,
  type AccessRow,
  type LookupInput,
} from "./report-access.ts";
import { CACHE_CONTROL } from "./cache-control.ts";
import { readCapped } from "./triage.ts";

/** Most events a JSON lookup returns, as many as a report can hold. */
const MAX_EVENTS = 500;

/** Most fields per event a JSON lookup returns. */
const MAX_FIELDS = 32;

/** One event as a JSON lookup returns it. */
export type LookupEvent = {
  /** When it happened, Unix ms, or 0 when the report had none. */
  ts_unix_ms: number;
  /** ERROR, WARN, INFO, DEBUG or TRACE. */
  level: string;
  /** Where it was logged, at most 256 characters. */
  target: string;
  /** The redacted message, at most 8000 characters. */
  message: string;
  /** Redacted `[name, value]` pairs. */
  fields: [string, string][];
};

/** The JSON answer to a lookup. */
export type LookupAnswer = {
  /** The report's ID. */
  report_id: string;
  /** When it arrived, Unix ms. */
  received_at_unix_ms: number;
  /** When its sender deleted it, Unix ms, or null. */
  deleted_at_unix_ms: number | null;
  /** The stored report, or null when deleted or unreadable. */
  report: {
    version: string;
    trigger: string;
    os: string;
    arch: string;
    distro: string;
    events: LookupEvent[];
  } | null;
  /** Issues tracking it, tracking issue first. */
  issues: { number: number; title: string; state: "open" | "closed"; url: string }[];
  /** The tracking issue's fix, or null. */
  fix: { pr_number: number | null; pr_url: string | null; fixed_in: string | null } | null;
};

/**
 * A JSON answer that is never cached.
 *
 * @param obj - Body.
 * @param status - HTTP status.
 * @returns The response.
 */
function json(obj: unknown, status: number = 200): Response {
  return new Response(JSON.stringify(obj), {
    status,
    headers: { "Content-Type": "application/json", "Cache-Control": CACHE_CONTROL.noStore },
  });
}

/**
 * `owner/name` from GITHUB_REPOSITORY.
 *
 * @param env - Worker bindings.
 * @returns The repository, or null when unset or malformed.
 */
function repository(env: Env): { owner: string; repo: string } | null {
  const m = /^([A-Za-z0-9-]{1,39})\/([A-Za-z0-9._-]{1,100})$/.exec(
    (env.GITHUB_REPOSITORY ?? "").trim(),
  );
  return m?.[1] && m[2] ? { owner: m[1], repo: m[2] } : null;
}

/**
 * Text from an untrusted value.
 *
 * @param value - Anything.
 * @param max - Longest result.
 * @returns The string cut to `max` characters, or "" for a non-string.
 */
function text(value: unknown, max: number): string {
  return typeof value === "string" ? value.slice(0, max) : "";
}

/**
 * A stored report's payload in the lookup's JSON shape.
 *
 * @param stored - The stored object (untrusted shape).
 * @returns The report.
 */
export function lookupReport(stored: JsonObject): NonNullable<LookupAnswer["report"]> {
  const p = asObject(stored.payload);
  const events = Array.isArray(p.events) ? p.events.slice(0, MAX_EVENTS) : [];
  return {
    version: text(p.version, 64),
    trigger: text(p.trigger, 32),
    os: text(p.os, 32),
    arch: text(p.arch, 32),
    distro: text(p.distro, 64),
    events: events.map((ev) => {
      const e = asObject(ev);
      const level = text(e.level, 8).toUpperCase();
      return {
        ts_unix_ms:
          typeof e.ts_unix_ms === "number" && Number.isSafeInteger(e.ts_unix_ms) && e.ts_unix_ms > 0
            ? e.ts_unix_ms
            : 0,
        level: ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"].includes(level) ? level : "INFO",
        target: text(e.target, 256),
        message: text(e.message, 8000),
        fields: (Array.isArray(e.fields) ? e.fields : [])
          .filter(
            (f: unknown): f is [string, string] =>
              Array.isArray(f) &&
              f.length === 2 &&
              typeof f[0] === "string" &&
              typeof f[1] === "string",
          )
          .slice(0, MAX_FIELDS)
          .map(([k, v]): [string, string] => [k.slice(0, 64), v.slice(0, 1024)]),
      };
    }),
  };
}

/**
 * Gathers what a lookup shows.
 *
 * @param env - Worker bindings (with the dashboard database).
 * @param db - Dashboard database.
 * @param b2 - B2 helpers.
 * @param access - The report's access row.
 * @param code - Its canonical code.
 * @returns The page's data.
 */
async function gather(
  env: Env,
  db: D1Database,
  b2: B2Access,
  access: AccessRow,
  code: string,
): Promise<LookupView> {
  const repo = repository(env);
  const [stored, found] = await Promise.all([
    access.deleted_at === null
      ? readStoredReport(b2, access.object_key).catch(() => null)
      : Promise.resolve(null),
    getReport(db, access.report_id),
  ]);
  const issues = repo
    ? (found?.issues ?? []).map((i) => ({
        number: Number(i.number),
        title: String(i.title).slice(0, 256),
        state: i.state === "open" ? ("open" as const) : ("closed" as const),
        url: `https://github.com/${repo.owner}/${repo.repo}/issues/${Number(i.number)}`,
      }))
    : [];
  let fix: LookupView["fix"] = null;
  const tracking = issues[0];
  if (repo && tracking) {
    const row = await db
      .prepare(
        "SELECT fixed_in, pr_number FROM issue_fix WHERE issue_number = ? ORDER BY recorded_at DESC LIMIT 1",
      )
      .bind(tracking.number)
      .first<{ fixed_in: string | null; pr_number: number | null }>();
    // Only the tracking issue's own pull requests: a report linked to
    // several issues lists every issue's.
    const merged = (found?.prs ?? [])
      .filter((p) => p.state === "merged" && Number(p.issue_number) === tracking.number)
      .sort((a, b) => Number(b.merged_at) - Number(a.merged_at))[0];
    const prNumber =
      row?.pr_number != null ? Number(row.pr_number) : merged ? Number(merged.pr_number) : null;
    if (row || merged) {
      fix = {
        prNumber,
        prUrl:
          prNumber === null
            ? null
            : `https://github.com/${repo.owner}/${repo.repo}/pull/${prNumber}`,
        fixedIn: row?.fixed_in ? String(row.fixed_in).slice(0, 64) : null,
      };
    }
  }
  const received = Number(asObject(stored).received_at_unix_ms);
  return {
    reportId: access.report_id,
    code,
    receivedAt: Number.isSafeInteger(received) && received > 0 ? received : access.created_at,
    deletedAt: access.deleted_at,
    stored,
    issues,
    fix,
  };
}

/**
 * A lookup's data as JSON.
 *
 * @param v - The page's data.
 * @returns The answer.
 */
export function lookupAnswer(v: LookupView): LookupAnswer {
  return {
    report_id: v.reportId,
    received_at_unix_ms: v.receivedAt,
    deleted_at_unix_ms: v.deletedAt,
    report: v.stored ? lookupReport(v.stored) : null,
    issues: v.issues,
    fix: v.fix ? { pr_number: v.fix.prNumber, pr_url: v.fix.prUrl, fixed_in: v.fix.fixedIn } : null,
  };
}

/**
 * Handles the lookup routes.
 *
 * @param request - The request.
 * @param env - Worker bindings.
 * @param path - Path without a trailing slash.
 * @param b2 - B2 helpers.
 * @param now - Unix ms.
 * @returns The answer, or null when the path is not a lookup route.
 */
export async function lookupFetch(
  request: Request,
  env: Env,
  path: string,
  b2: B2Access,
  now: number = Date.now(),
): Promise<Response | null> {
  if (path !== "/lookup" && path !== "/lookup/delete") return null;
  const method = request.method;
  const db = env.DASHBOARD_DB;
  if (path === "/lookup" && (method === "GET" || method === "HEAD")) {
    if (!db) {
      return page(
        lookupMessagePage("Lookup unavailable", "This collector does not offer report lookup."),
        503,
      );
    }
    return page(lookupFormPage());
  }
  if (method !== "POST") return null;

  const contentType = request.headers.get("content-type");
  const form = (contentType ?? "").toLowerCase().startsWith("application/x-www-form-urlencoded");
  const refuse = (status: number, error: string, title: string, message: string): Response =>
    form ? page(lookupMessagePage(title, message), status) : json({ error }, status);
  if (!db) {
    return refuse(
      503,
      "lookup is not configured",
      "Lookup unavailable",
      "This collector does not offer report lookup.",
    );
  }
  const body = await readCapped(request, MAX_LOOKUP_CHARS * 4);
  const input = body === null ? null : parseLookupInput(contentType, body);
  if (!input) {
    if (form && path === "/lookup") {
      return page(
        lookupFormPage(
          "That is not a report ID and access code. A report ID looks like 3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f and a code like K7QM-3XRD-PW9T-H2CF.",
        ),
        400,
      );
    }
    return refuse(
      400,
      'expected {"report_id": "<uuid>", "access_code": "<code>"}',
      "Request refused",
      path === "/lookup/delete"
        ? "Tick the box to confirm, then press Delete report again."
        : "That is not a report ID and access code.",
    );
  }
  if (env.LOOKUP_LIMIT) {
    const { success } = await env.LOOKUP_LIMIT.limit({ key: input.report_id });
    if (!success) {
      return refuse(
        429,
        "too many attempts; wait a minute",
        "Too many attempts",
        "Too many attempts for this report. Wait a minute and try again.",
      );
    }
  }
  const access = await checkAccess(db, input);
  if (!access) {
    if (form && path === "/lookup") {
      return page(
        lookupFormPage("No report matches that ID and access code.", {
          reportId: input.report_id,
        }),
        404,
      );
    }
    return refuse(
      404,
      "not found",
      "Report not found",
      "No report matches that ID and access code.",
    );
  }
  if (path === "/lookup") {
    const view = await gather(env, db, b2, access, input.access_code);
    return form ? page(lookupResultPage(view)) : json(lookupAnswer(view));
  }
  return deleteReport(db, b2, access, input, form, now);
}

/**
 * Deletes a report's stored copy.
 *
 * @param db - Dashboard database.
 * @param b2 - B2 helpers.
 * @param access - The report's access row.
 * @param input - The request (a form must have ticked the confirmation).
 * @param form - Whether the request came from the page.
 * @param now - Unix ms.
 * @returns The answer.
 */
async function deleteReport(
  db: D1Database,
  b2: B2Access,
  access: AccessRow,
  input: LookupInput,
  form: boolean,
  now: number,
): Promise<Response> {
  if (form && !input.confirm) {
    return page(
      lookupMessagePage("Not deleted", "Tick the box to confirm, then press Delete report again."),
      400,
    );
  }
  if (access.deleted_at === null) {
    // B2 first: if it fails, nothing is marked and the sender can try again.
    await b2.deleteObject(await b2.authorize(), access.object_key);
    await markDeleted(db, access.report_id, now);
  }
  const deletedAt = access.deleted_at ?? now;
  return form
    ? page(lookupDeletedPage(access.report_id))
    : json({ report_id: access.report_id, deleted_at_unix_ms: deletedAt });
}

/**
 * An HTML page with the dashboard's security headers.
 *
 * @param body - The document.
 * @param status - HTTP status.
 * @returns The response.
 */
function page(body: SafeHtml, status: number = 200): Response {
  return htmlResponse(body, status);
}
