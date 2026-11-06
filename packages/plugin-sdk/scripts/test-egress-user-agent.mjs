// Replay and seeded mutation fuzzing for the egress bridge's User-Agent
// decision (`keepsOwnUserAgent` / `withUserAgent` in `bridge/egress.js`),
// checked against an independent oracle. The bridge is the same file as
// `crates/bookclerk-workerd/bridge/egress.js` (the workerd pin sync check
// keeps the copies identical).
//
// `FUZZ_ITERATIONS` sets the number of mutated cases (default 4000) and
// `FUZZ_SEED` the generator seed (default: a fixed seed, so PR runs repeat).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const bridgePath = path.join(here, "..", "bridge", "egress.js");
const corpusDir = path.join(here, "egress_user_agent_corpus");

/**
 * Loads the bridge's User-Agent helpers without the workerd-only socket
 * import (the source is this repository's own file).
 *
 * @returns {{ keepsOwnUserAgent: Function, withUserAgent: Function }} The helpers.
 */
function loadBridge() {
  const source = fs.readFileSync(bridgePath, "utf8");
  const importLine = 'import { connect } from "cloudflare:sockets";';
  if (!source.includes(importLine) || !source.includes("export default {")) {
    throw new Error("egress.js changed shape; update test-egress-user-agent.mjs");
  }
  const body = source
    .replace(importLine, "const connect = () => { throw new Error('no sockets'); };")
    .replace("export default {", "const bridgeDefault = {");
  const factory = new Function(`${body}\nreturn { keepsOwnUserAgent, withUserAgent };`);
  return factory();
}

const FALLBACK = "bookclerk (+https://bookclerk.app)";

/**
 * Oracle normalization: trimmed once, trailing dots dropped, lowercased; a
 * token holding `%` or non-ASCII, or left empty, never matches.
 *
 * @param {unknown} value - Host or pattern as given.
 * @returns {string | null} The token, or `null` when it never matches.
 */
function oracleToken(value) {
  if (typeof value !== "string") return null;
  let token = value.trim();
  while (token.endsWith(".")) token = token.slice(0, -1);
  token = token.toLowerCase();
  if (token === "" || token.includes("%")) return null;
  if ([...token].some((ch) => ch.codePointAt(0) > 0x7f)) return null;
  return token;
}

/**
 * Independent oracle for `keepsOwnUserAgent`: a `*` pattern never matches,
 * `*.suffix` matches the suffix and every name whose labels end with its
 * labels, and any other pattern matches exactly.
 *
 * @param {unknown} host - Request host of the hop.
 * @param {unknown[] | undefined} patterns - The policy's own-agent hosts.
 * @returns {boolean} Whether the guest's agent may go to `host`.
 */
function oracleKeeps(host, patterns) {
  const h = oracleToken(host);
  if (h === null) return false;
  const hostLabels = h.split(".");
  for (const raw of patterns ?? []) {
    const p = oracleToken(raw);
    if (p === null || p === "*") continue;
    if (p.startsWith("*.")) {
      const suffix = p.slice(2).split(".");
      if (hostLabels.length < suffix.length) continue;
      const tail = hostLabels.slice(hostLabels.length - suffix.length);
      if (tail.every((label, i) => label === suffix[i])) return true;
    } else if (h === p) {
      return true;
    }
  }
  return false;
}

/**
 * The agent the oracle expects on the hop.
 *
 * @param {object} c - Case: `host`, `patterns`, `userAgent` and `own`.
 * @returns {string} The guest's agent on a kept host, else the policy's or the fallback.
 */
function oracleAgent(c) {
  if (c.own != null && oracleKeeps(c.host, c.patterns)) return c.own;
  return typeof c.userAgent === "string" && c.userAgent ? c.userAgent : FALLBACK;
}

const { keepsOwnUserAgent, withUserAgent } = loadBridge();

/**
 * Runs one case through the bridge and exits non-zero on any disagreement.
 *
 * @param {object} c - Case: `host`, `patterns`, `userAgent` and `own`.
 * @param {string} label - Name printed on failure.
 */
function check(c, label) {
  const policy = { ownUserAgentHosts: c.patterns, userAgent: c.userAgent };
  const keeps = keepsOwnUserAgent(c.host, policy);
  if (keeps !== oracleKeeps(c.host, c.patterns)) {
    console.error(`FAIL ${label}: keepsOwnUserAgent`, JSON.stringify(c), keeps);
    process.exit(1);
  }
  const headers = c.own == null ? {} : { "user-agent": c.own };
  const request = new Request("https://example.invalid/", { headers });
  const sent = withUserAgent(request, c.host, policy).headers.get("user-agent");
  if (sent !== oracleAgent(c)) {
    console.error(`FAIL ${label}: withUserAgent`, JSON.stringify(c), sent);
    process.exit(1);
  }
}

// Replay every corpus case.
const corpus = [];
for (const name of fs.readdirSync(corpusDir).sort()) {
  if (!name.endsWith(".json")) continue;
  const c = JSON.parse(fs.readFileSync(path.join(corpusDir, name), "utf8"));
  check(c, name);
  corpus.push(c);
}
if (corpus.length === 0) throw new Error("empty egress_user_agent_corpus");

// Seeded mutation: xorshift32, so a failing seed replays exactly.
let state = Number(process.env.FUZZ_SEED ?? 0x5eed1e55) >>> 0 || 1;
/**
 * Advances the xorshift32 generator.
 *
 * @returns {number} The next unsigned 32-bit value.
 */
function next() {
  state ^= state << 13;
  state >>>= 0;
  state ^= state >>> 17;
  state ^= state << 5;
  state >>>= 0;
  return state;
}
/**
 * Picks one item at random.
 *
 * @param {unknown[]} items - Non-empty list.
 * @returns {unknown} One of `items`.
 */
const pick = (items) => items[next() % items.length];
const PIECES = ["*", "*.", ".", "..", " ", "%", "A", "a", "b", "-", "1", "é", "ｅ", ",", "\t", "x"];

/**
 * One text mutation: insert, delete, upper-case or wrap.
 *
 * @param {unknown} value - Host or pattern to mutate.
 * @returns {string} The mutated text.
 */
function mutateString(value) {
  const s = typeof value === "string" ? value : "";
  const at = s.length === 0 ? 0 : next() % (s.length + 1);
  switch (next() % 4) {
    case 0:
      return s.slice(0, at) + pick(PIECES) + s.slice(at);
    case 1:
      return s.slice(0, at) + s.slice(at + 1);
    case 2:
      return s.toUpperCase();
    default:
      return `${pick(PIECES)}${s}${pick(["", ".", ". ", " "])}`;
  }
}

/**
 * One case mutation: host, a pattern, an added pattern, the agents.
 *
 * @param {object} c - Case to start from (not changed).
 * @returns {object} The mutated case.
 */
function mutate(c) {
  const out = { ...c, patterns: [...(c.patterns ?? [])] };
  switch (next() % 6) {
    case 0:
      out.host = mutateString(out.host);
      break;
    case 1:
      if (out.patterns.length > 0) {
        const i = next() % out.patterns.length;
        out.patterns[i] = mutateString(out.patterns[i]);
      }
      break;
    case 2:
      out.patterns.push(pick(["*", " * ", "*.", `*.${oracleToken(out.host) ?? "x"}`, out.host]));
      break;
    case 3:
      out.host = pick(corpus).host;
      break;
    case 4:
      out.own = out.own == null ? "Store/1.0" : null;
      break;
    default:
      out.userAgent = pick(["", "bookclerk/9.9.9 (+https://bookclerk.app)", undefined]);
  }
  return out;
}

const iterations = Number(process.env.FUZZ_ITERATIONS ?? 4000);
let current = pick(corpus);
for (let i = 0; i < iterations; i++) {
  current = next() % 8 === 0 ? pick(corpus) : mutate(current);
  check(current, `mutation ${i}`);
}
console.log(`ok egress user-agent: ${corpus.length} corpus cases, ${iterations} mutations`);
