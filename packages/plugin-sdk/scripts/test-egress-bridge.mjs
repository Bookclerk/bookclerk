// Corpus replay for the egress bridge's host gate (`allowsInitial`,
// `allowsRedirect`, `allowsTcp` and `canonicalSocketHost` in
// `bridge/egress.js`) against the Rust `EgressPolicy`, the reference the
// native mediator enforces.
//
// `egress_bridge_vectors.json` holds the Rust verdicts for every probe of
// every seed in `fuzz/corpus/egress_bridge/`; the `bookclerk-workerd` test
// `egress_bridge_vectors_are_current` writes it from the shared oracle
// (`fuzz/oracles/egress_bridge.rs`) and fails when it is stale. The
// `egress_bridge` fuzz target runs the same check coverage-guided, with the
// bridge in an embedded JavaScript engine. The bridge here is the same file
// as `crates/bookclerk-workerd/bridge/egress.js` (the workerd pin sync check
// keeps the copies identical).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const bridgePath = path.join(here, "..", "bridge", "egress.js");
const vectorsPath = path.join(here, "egress_bridge_vectors.json");

/**
 * Loads the bridge's gate functions without the workerd-only socket import
 * (the source is this repository's own file).
 *
 * @returns {{ allowsInitial: Function, allowsRedirect: Function, allowsTcp: Function, canonicalSocketHost: Function }} The gate.
 */
function loadBridge() {
  const source = fs.readFileSync(bridgePath, "utf8");
  const importLine = 'import { connect } from "cloudflare:sockets";';
  if (!source.includes(importLine) || !source.includes("export default {")) {
    throw new Error("egress.js changed shape; update test-egress-bridge.mjs");
  }
  const body = source
    .replace(importLine, "const connect = () => { throw new Error('no sockets'); };")
    .replace("export default {", "const bridgeDefault = {");
  const factory = new Function(
    `${body}\nreturn { allowsInitial, allowsRedirect, allowsTcp, canonicalSocketHost };`,
  );
  return factory();
}

/**
 * The fetch hostname Node's URL parser gives a host, as workerd's gives it
 * (WHATWG host parsing, which the oracle models with the `url` crate).
 *
 * @param {string} host - Probe host as derived.
 * @returns {string | null} The hostname, or null when no URL names it.
 */
function fetchHost(host) {
  // A host never holds URL structure; the host parser refuses these too.
  if (/[@/?#\\]/.test(host)) return null;
  const literal = host.includes(":") && !host.startsWith("[") ? `[${host}]` : host;
  try {
    return new URL(`http://${literal}/`).hostname;
  } catch {
    return null;
  }
}

const gate = loadBridge();
const seeds = JSON.parse(fs.readFileSync(vectorsPath, "utf8"));
let failures = 0;
let probes = 0;

/**
 * Records one disagreement.
 *
 * @param {string} label - Seed and probe.
 * @param {string} what - The check that failed.
 * @param {unknown} detail - Values to print.
 */
function fail(label, what, detail) {
  failures += 1;
  console.error(`FAIL ${label}: ${what}`, JSON.stringify(detail));
}

for (const { seed, policy, probes: list } of seeds) {
  const maxRedirects = Number(policy.maxRedirects ?? 10);
  for (const p of list) {
    probes += 1;
    const label = `${seed} ${JSON.stringify(p.host)}:${p.port}`;
    // The Rust model of the URL parser and of `canonicalSocketHost` is the
    // one the vectors were written with; both must match the real ones,
    // except that a newer parser may accept a raw host the model refuses
    // (IDNA rules for `xn--` labels changed between Node releases). Such a
    // host reaches the gate as written, so the gate is held to the Rust
    // policy's verdicts on the raw host instead.
    const node = fetchHost(p.host);
    const socket = gate.canonicalSocketHost(p.host);
    const lenient = (seen, modelled) =>
      modelled == null && seen != null && seen === p.host.toLowerCase();
    const lenientFetch = lenient(node, p.fetch);
    const lenientSocket = lenient(socket, p.socket);
    if (node !== p.fetch && !lenientFetch) {
      fail(label, "fetch hostname", { node, rust: p.fetch });
    }
    if (socket !== p.socket && !lenientSocket) {
      fail(label, "socket hostname", { js: socket, rust: p.socket });
    }
    if (lenientFetch || lenientSocket) {
      const wide = {
        initial: lenientFetch && gate.allowsInitial(node, policy),
        redirect: lenientFetch && gate.allowsRedirect(node, p.hop, policy, maxRedirects),
        tcp: lenientSocket && gate.allowsTcp(socket, p.port, policy),
      };
      for (const key of ["initial", "redirect", "tcp"]) {
        if (wide[key] && !p.rust_raw[key]) {
          fail(label, `${key} wider than Rust on a host only Node parses`, {
            wide,
            rust: p.rust_raw,
          });
        }
      }
    }
    const js = {
      initial: p.fetch != null && gate.allowsInitial(p.fetch, policy),
      redirect: p.fetch != null && gate.allowsRedirect(p.fetch, p.hop, policy, maxRedirects),
      tcp: p.socket != null && gate.allowsTcp(p.socket, p.port, policy),
    };
    for (const key of ["initial", "redirect", "tcp"]) {
      if (js[key] && !p.rust[key]) fail(label, `${key} wider than Rust`, { js, rust: p.rust });
    }
    if (js.tcp && !p.rust_raw.tcp) fail(label, "tcp wider than Rust on the raw host", p);
    if (p.exact) {
      if (js.initial !== p.rust.initial || js.redirect !== p.rust.redirect) {
        fail(label, "fetch verdicts differ on a hostname", { js, rust: p.rust });
      }
      if (p.socket != null && js.tcp !== p.rust.tcp) {
        fail(label, "tcp verdicts differ on a hostname", { js, rust: p.rust });
      }
    }
  }
}

if (seeds.length === 0 || probes === 0) throw new Error("empty egress_bridge_vectors.json");
if (failures > 0) {
  console.error(`${failures} of ${probes} egress bridge probes disagree with the Rust policy`);
  process.exit(1);
}
console.log(
  `egress bridge: ${probes} probes across ${seeds.length} seeds agree with the Rust policy`,
);
