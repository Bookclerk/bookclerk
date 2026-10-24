// Readers for bytes the demo Worker did not produce: the visitor's cookies,
// Turnstile's verdict, and answers from the visitor's container. Sans-IO, so
// test/inputs.fuzz.test.js drives the same code the Worker runs.
//
// The container counts as untrusted: it runs the visitor's store plugins
// with best-effort isolation (Cloudflare's kernel has no Landlock), so a
// compromised guest could answer anything. Nothing it says may set the
// Worker's own cookie, name another session, or grow without bound.

/** The Worker's session cookie. `__Host-` pins it to this host, path `/`, Secure. */
export const SESSION_COOKIE = "__Host-demo";

/** bookclerkd's portal session cookie, which the visitor's browser keeps. */
export const PORTAL_COOKIE = "bookclerk_portal_session";

/** Session tokens are 16 random bytes in unpadded base64url. */
const TOKEN_RE = /^[A-Za-z0-9_-]{22}$/;

/** A cookie name (RFC 6265 token). */
const COOKIE_NAME_RE = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

/** Cookie values the Worker passes on: no quotes, separators or controls. */
const COOKIE_VALUE_RE = /^[!#$%&'()*+\-./0-9:<=>?@A-Z[\]^_`a-z{|}~]*$/;

/** Longest cookie header or Set-Cookie line the readers look at. */
export const MAX_COOKIE_CHARS = 8192;

/** Most store connections teardown signs out; more is not a real library. */
export const MAX_CONNECTIONS = 50;

/** Longest store source or account id teardown accepts. */
export const MAX_ID_CHARS = 200;

/** Longest JSON body the decoders parse. */
export const MAX_BODY_CHARS = 256 * 1024;

/**
 * Splits a `Cookie` header into name/value pairs, in order.
 *
 * @param {string | null | undefined} header Raw `Cookie` header.
 * @returns {Array<[string, string]>} Pairs with whitespace trimmed; empty for
 *   a missing or oversized header.
 */
export function cookiePairs(header) {
  if (typeof header !== "string" || header.length > MAX_COOKIE_CHARS) return [];
  const out = [];
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq <= 0) continue;
    out.push([part.slice(0, eq).trim(), part.slice(eq + 1).trim()]);
  }
  return out;
}

/**
 * Reads the visitor's session token from a `Cookie` header.
 *
 * @param {string | null | undefined} header Raw `Cookie` header.
 * @returns {string | null} The first well-formed `__Host-demo` value, or null.
 */
export function readSessionToken(header) {
  for (const [name, value] of cookiePairs(header)) {
    if (name === SESSION_COOKIE && TOKEN_RE.test(value)) return value;
  }
  return null;
}

/**
 * Rebuilds a `Cookie` header for the container without the Worker's own
 * cookies, so the visitor's demo session token never reaches the container.
 *
 * @param {string | null | undefined} header Raw `Cookie` header.
 * @returns {string} The remaining pairs, `; `-joined (empty when none).
 */
export function cookiesForContainer(header) {
  return cookiePairs(header)
    .filter(([name]) => COOKIE_NAME_RE.test(name) && !reservedCookieName(name))
    .filter(([, value]) => COOKIE_VALUE_RE.test(value))
    .map(([name, value]) => `${name}=${value}`)
    .join("; ");
}

/**
 * Whether a cookie name belongs to the Worker rather than the container:
 * the session cookie and every `__Host-` / `__Secure-` name.
 *
 * @param {string} name Cookie name.
 * @returns {boolean} True when the container may not set or read it.
 */
function reservedCookieName(name) {
  const lower = name.toLowerCase();
  return lower.startsWith("__host-") || lower.startsWith("__secure-");
}

/**
 * Rewrites one `Set-Cookie` line from the container for the browser.
 *
 * The container answers over plain HTTP inside its VM, so its cookies lack
 * `Secure`; the Worker adds it. `Domain` is dropped so a cookie stays on the
 * demo host. Lines that are malformed, oversized, or name a Worker cookie
 * are dropped.
 *
 * @param {string} line One `Set-Cookie` header value.
 * @returns {string | null} The rewritten line, or null to drop it.
 */
export function rewriteSetCookie(line) {
  if (typeof line !== "string" || line.length > MAX_COOKIE_CHARS) return null;
  const parts = line.split(";").map((p) => p.trim());
  const first = parts.shift() ?? "";
  const eq = first.indexOf("=");
  if (eq <= 0) return null;
  const name = first.slice(0, eq);
  const value = first.slice(eq + 1);
  if (!COOKIE_NAME_RE.test(name) || reservedCookieName(name) || !COOKIE_VALUE_RE.test(value)) return null;
  const attrs = [];
  for (const attr of parts) {
    if (attr === "") continue;
    const key = attr.split("=", 1)[0].trim().toLowerCase();
    if (key === "domain" || key === "secure") continue;
    // Attribute values with controls could split the header; drop the cookie.
    if (/[\u0000-\u001f\u007f]/.test(attr)) return null;
    attrs.push(attr);
  }
  attrs.push("Secure");
  return [`${name}=${value}`, ...attrs].join("; ");
}

/**
 * Finds bookclerkd's portal session in the container's `Set-Cookie` lines.
 *
 * @param {string[]} lines `Set-Cookie` header values from one response.
 * @returns {string | null} The session value when exactly one well-formed,
 *   non-empty `bookclerk_portal_session` cookie is set; otherwise null.
 */
export function portalSessionFrom(lines) {
  if (!Array.isArray(lines)) return null;
  const found = [];
  for (const line of lines) {
    if (typeof line !== "string" || line.length > MAX_COOKIE_CHARS) continue;
    const first = line.split(";", 1)[0].trim();
    const eq = first.indexOf("=");
    if (eq <= 0 || first.slice(0, eq) !== PORTAL_COOKIE) continue;
    found.push(first.slice(eq + 1));
  }
  if (found.length !== 1) return null;
  const value = found[0];
  return /^[A-Za-z0-9._~-]{16,512}$/.test(value) ? value : null;
}

/**
 * Parses JSON text no longer than {@link MAX_BODY_CHARS}.
 *
 * @param {unknown} text Body text.
 * @returns {unknown} The parsed value, or undefined when it is not JSON.
 */
function parseJson(text) {
  if (typeof text !== "string" || text.length > MAX_BODY_CHARS) return undefined;
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/**
 * Whether a value is a plain (non-array) object.
 *
 * @param {unknown} v Value.
 * @returns {v is Record<string, unknown>} True for a JSON object.
 */
function isObject(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/**
 * Whether a string is a usable store id: 1 to {@link MAX_ID_CHARS}
 * characters without controls.
 *
 * @param {unknown} s Value.
 * @returns {s is string} True when it can be put in a revoke URL.
 */
function isId(s) {
  return typeof s === "string" && s.length > 0 && s.length <= MAX_ID_CHARS && !/[\u0000-\u001f\u007f]/.test(s);
}

/**
 * Decodes `GET /api/portal/connections` into the accounts teardown signs out.
 *
 * @param {string} text Response body.
 * @returns {Array<{source: string, accountId: string}> | null} The linked
 *   accounts (deduplicated, at most {@link MAX_CONNECTIONS}), or null when
 *   the body is not the expected shape.
 */
export function decodeConnections(text) {
  const body = parseJson(text);
  if (!isObject(body) || !Array.isArray(body.connections)) return null;
  const out = [];
  const seen = new Set();
  for (const c of body.connections) {
    if (!isObject(c) || !isId(c.source) || !isId(c.account_id)) continue;
    const key = JSON.stringify([c.source, c.account_id]);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ source: c.source, accountId: c.account_id });
    if (out.length === MAX_CONNECTIONS) break;
  }
  return out;
}

/**
 * Reads this node's id and the default instance of each wanted plugin from
 * `GET /api/config/plugin-instances`.
 *
 * @param {string} text Response body.
 * @param {string[]} aliases Plugin ids (manifest `id`) to look for.
 * @returns {{nodeId: string, instances: Map<string, string>} | null} The
 *   node id and, per alias found, the id of its default instance (the first
 *   well-formed entry wins); null when the body is not an object with a
 *   well-formed `node_id` and an `instances` array.
 */
export function decodeInstances(text, aliases) {
  const body = parseJson(text);
  if (!isObject(body) || !isId(body.node_id) || !Array.isArray(body.instances)) return null;
  const wanted = new Set(aliases);
  // A Map, not an object: an alias such as "__proto__" stays a plain key.
  /** @type {Map<string, string>} */
  const instances = new Map();
  for (const i of body.instances) {
    if (!isObject(i) || i.is_default !== true || !isId(i.plugin_instance_id)) continue;
    if (typeof i.alias !== "string" || !wanted.has(i.alias) || instances.has(i.alias)) continue;
    instances.set(i.alias, i.plugin_instance_id);
  }
  return { nodeId: body.node_id, instances };
}

/**
 * Reads the claim ticket from `POST /api/auth/bootstrap`.
 *
 * @param {string} text Response body.
 * @returns {string | null} The ticket when it is 16 to 512 URL-safe
 *   characters; otherwise null.
 */
export function decodeBootstrap(text) {
  const body = parseJson(text);
  if (!isObject(body) || body.ok !== true) return null;
  const t = body.claim_ticket;
  return typeof t === "string" && /^[A-Za-z0-9._~-]{16,512}$/.test(t) ? t : null;
}

/**
 * Whether a Turnstile Siteverify answer passes for this site and action.
 *
 * @param {string} text Siteverify response body.
 * @param {{hostname: string, action: string}} expect The demo's hostname and
 *   the widget action the launch page sets.
 * @returns {boolean} True only for `success: true` with the same hostname
 *   and action.
 */
export function siteverifyPassed(text, expect) {
  const body = parseJson(text);
  return (
    isObject(body) &&
    body.success === true &&
    typeof body.hostname === "string" &&
    body.hostname === expect.hostname &&
    typeof body.action === "string" &&
    body.action === expect.action
  );
}
