// The demo Worker's untrusted-input boundaries (fuzzing ADR decision 6):
// the route check, the launch form, the visitor's cookies, the container's Set-Cookie lines
// and JSON answers, Turnstile's verdict, and the live status socket's
// handshake and messages. Each replays its seed corpus
// under corpus/<name>/ and runs seeded mutation fuzzing, checking every
// result against an oracle written without the module's helpers.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { URL } from "node:url";
import { classify, OPERATIONS } from "../src/routes.ts";
import {
  cookiesForContainer,
  decodeClaimTicket,
  decodeConnections,
  decodeInstances,
  decodeLaunchForm,
  MAX_CONNECTIONS,
  MAX_FORM_BYTES,
  MAX_TURNSTILE_TOKEN_CHARS,
  MAX_ID_CHARS,
  portalSessionFrom,
  readSessionToken,
  rewriteSetCookie,
  siteverifyPassed,
} from "../src/inputs.ts";
import { liveCommand, liveRequest } from "../src/live.ts";
import {
  decodeApiResponse,
  findWidget,
  MAX_DOMAINS,
  widgetKeys,
  WIDGET_NAME,
} from "../scripts/worker-secrets.ts";
import { mutate, rng } from "../test-support/fuzz.ts";

/** Mutated inputs per seed. */
const ROUNDS = 300;

/**
 * Parses JSON the way the oracles want it: anything unparsable is undefined.
 *
 * @param input - JSON text.
 * @returns The value, or undefined.
 */
function parseOrUndefined(input: string): unknown {
  try {
    return JSON.parse(input);
  } catch {
    return undefined;
  }
}

/**
 * Whether a parsed value is a JSON object (not null, not an array).
 *
 * @param v - Parsed value.
 * @returns True for an object.
 */
function plainObject(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/**
 * Seed inputs of one corpus, keyed by file name.
 *
 * @param name - Corpus directory under corpus/.
 * @returns File names and contents.
 */
function seeds(name: string): Array<[string, string]> {
  const url = new URL(`../corpus/${name}/`, import.meta.url);
  return readdirSync(url)
    .sort()
    .map((f) => [f, readFileSync(new URL(f, url), "utf8")]);
}

/**
 * Replays a corpus, then fuzzes each seed with a fixed PRNG so a failure
 * replays from its label.
 *
 * @param name - Corpus name (also the PRNG seed's source).
 * @param fragments - Fragments the mutator splices in.
 * @param check - Oracle check.
 */
function fuzz(name: string, fragments: string[], check: (input: string, label: string) => void) {
  const all = seeds(name);
  assert.ok(all.length > 0, `corpus/${name} is empty`);
  for (const [file, text] of all) check(text, `${name}/${file}`);
  let seed = 0;
  for (const ch of name) seed = (seed * 31 + ch.charCodeAt(0)) >>> 0;
  const rand = rng(seed);
  for (const [file, text] of all) {
    for (let i = 0; i < ROUNDS; i++)
      check(mutate(rand, text, fragments), `${name}/${file} mutation ${i}`);
  }
}

/**
 * Oracle's own percent-decoder for one segment: %XX runs as UTF-8.
 *
 * @param seg - Raw segment.
 * @returns Decoded text, or null when an escape is broken.
 */
function oracleDecode(seg: string): string | null {
  const bytes = [];
  for (let i = 0; i < seg.length;) {
    if (seg[i] === "%") {
      const hex = seg.slice(i + 1, i + 3);
      if (!/^[0-9A-Fa-f]{2}$/.test(hex)) return null;
      bytes.push(parseInt(hex, 16));
      i += 3;
    } else {
      for (const b of new TextEncoder().encode(seg[i])) bytes.push(b);
      i += 1;
    }
  }
  try {
    return new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(
      new Uint8Array(bytes),
    );
  } catch {
    return null;
  }
}

/**
 * Oracle's own form-urlencoded reader: the first value of `name`, splitting
 * the bytes on `&` and the first `=`, with `+` as a space and `%XX` escapes
 * decoded (a broken escape stays as written) before UTF-8 decoding.
 *
 * @param bytes - Form body.
 * @param name - Field name.
 * @returns The value, or null when the field is absent.
 */
function oracleFormField(bytes: Uint8Array, name: string): string | null {
  const hex = (b: number | undefined): boolean =>
    b !== undefined &&
    ((b >= 0x30 && b <= 0x39) || (b >= 0x41 && b <= 0x46) || (b >= 0x61 && b <= 0x66));
  const decode = (part: Uint8Array): string => {
    const out: number[] = [];
    for (let i = 0; i < part.length; i++) {
      const b = part[i]!;
      if (b === 0x2b) out.push(0x20);
      else if (b === 0x25 && i + 2 < part.length && hex(part[i + 1]) && hex(part[i + 2])) {
        out.push(parseInt(String.fromCharCode(part[i + 1]!, part[i + 2]!), 16));
        i += 2;
      } else out.push(b);
    }
    return new TextDecoder("utf-8", { fatal: false, ignoreBOM: true }).decode(new Uint8Array(out));
  };
  let start = 0;
  for (let i = 0; i <= bytes.length; i++) {
    if (i < bytes.length && bytes[i] !== 0x26) continue;
    const seq = bytes.subarray(start, i);
    start = i + 1;
    if (seq.length === 0) continue;
    const eq = seq.indexOf(0x3d);
    const key = decode(eq === -1 ? seq : seq.subarray(0, eq));
    if (key === name) return eq === -1 ? "" : decode(seq.subarray(eq + 1));
  }
  return null;
}

/** Each listed operation as a method and an anchored pattern over decoded segments. */
const ORACLE_OPS = OPERATIONS.map(([method, template, verdict]) => ({
  method,
  verdict,
  parts: template.split("/").slice(1),
}));

describe("demo input boundaries", () => {
  it("route check never forwards a refused operation", () => {
    const VERDICTS = new Set(["allow", "signin", "deny"]);
    fuzz(
      "route",
      [
        "%",
        "%2e",
        "%2E",
        "%2f",
        "%5c",
        "%61",
        "/",
        "//",
        ".",
        "..",
        "acquire",
        "api",
        "library",
        "?",
        "#",
        "%00",
        "%C0%AF",
        "€",
        "{",
        "}",
      ],
      (input, label) => {
        const sp = input.indexOf(" ");
        const method = sp === -1 ? "GET" : input.slice(0, sp);
        const path = sp === -1 ? input : input.slice(sp + 1);
        const got = classify(method, path);
        assert.ok(VERDICTS.has(got.verdict), label);
        if (got.verdict === "deny") return;
        // Forwarded: the path must be a clean absolute path ...
        assert.ok(path.startsWith("/"), label);
        const raw = path.split("/").slice(1);
        const segs = raw.map(oracleDecode);
        assert.ok(
          segs.every(
            (s) => s !== null && s !== "." && s !== ".." && !/[/\\\u0000-\u001f\u007f]/.test(s),
          ),
          label,
        );
        assert.ok(
          raw.slice(0, -1).every((s) => s !== ""),
          label,
        );
        // ... and match no refused operation under the oracle's decoding.
        let m = method.toUpperCase();
        if (m === "HEAD") m = "GET";
        const matches = ORACLE_OPS.filter(
          (op) =>
            op.method === m &&
            op.parts.length === segs.length &&
            op.parts.every((p, i) => (p.startsWith("{") ? segs[i] !== "" : p === segs[i])),
        );
        assert.ok(
          matches.every((op) => op.verdict !== "deny"),
          label,
        );
        // A write reaches the container only through a listed operation.
        if (m !== "GET") assert.ok(matches.length > 0, label);
        // A sign-in start is always counted.
        if (matches.some((op) => op.verdict === "signin"))
          assert.equal(got.verdict, "signin", label);
      },
    );
  });

  it("session cookie reader takes only a well-formed __Host-demo value", () => {
    fuzz(
      "cookie",
      [";", "=", " ", "__Host-demo=", "__host-demo=", "A", "-", "_", "+", "/", "\t", "é"],
      (input, label) => {
        let expected: string | null = null;
        if (input.length <= 8192) {
          for (const part of input.split(";")) {
            const i = part.indexOf("=");
            if (i <= 0) continue;
            if (part.slice(0, i).trim() !== "__Host-demo") continue;
            const v = part.slice(i + 1).trim();
            if (v.length === 22 && [...v].every((c) => /[A-Za-z0-9_-]/.test(c))) {
              expected = v;
              break;
            }
          }
        }
        assert.equal(readSessionToken(input), expected, label);
        const forwarded = cookiesForContainer(input);
        for (const pair of forwarded === "" ? [] : forwarded.split("; ")) {
          const name = pair.slice(0, pair.indexOf("="));
          assert.ok(!/^__(host|secure)-/i.test(name), label);
          assert.ok(input.includes(name), label);
        }
      },
    );
  });

  it("Set-Cookie rewrite keeps cookies on this host, Secure, and off Worker names", () => {
    fuzz(
      "set_cookie",
      [
        ";",
        "=",
        " ",
        "Domain=x",
        "domain=",
        "Secure",
        "__Host-",
        "__Secure-",
        '"',
        ",",
        "\u0001",
        "\r\n",
      ],
      (input, label) => {
        const out = rewriteSetCookie(input);
        if (out === null) return;
        const parts = out.split("; ");
        const [name] = parts[0]!.split("=", 1);
        assert.ok(
          input.trimStart().startsWith(parts[0]!) || input.split(";")[0]!.trim() === parts[0],
          label,
        );
        assert.ok(!/^__(host|secure)-/i.test(name!), label);
        assert.equal(parts.at(-1), "Secure", label);
        // Attributes only: the cookie itself may be named "domain" or "secure".
        const attrs = parts.slice(1);
        assert.equal(attrs.filter((p) => p.toLowerCase() === "secure").length, 1, label);
        assert.ok(!attrs.some((p) => /^\s*domain\s*(=|$)/i.test(p)), label);
        assert.ok(!/[\u0000-\u001f\u007f]/.test(out), label);
      },
    );
  });

  it("portal session reader takes exactly one well-formed session", () => {
    fuzz("set_cookie", ["bookclerk_portal_session=", ";", "a", "\n", " "], (input, label) => {
      const lines = input.split("\n");
      const values = lines
        .filter((l) => l.length <= 8192)
        .map((l) => l.split(";")[0]!.trim())
        .filter((f) => f.startsWith("bookclerk_portal_session="))
        .map((f) => f.slice("bookclerk_portal_session=".length));
      const expected =
        values.length === 1 && /^[A-Za-z0-9._~-]{16,512}$/.test(values[0]!) ? values[0] : null;
      assert.equal(portalSessionFrom(lines), expected, label);
    });
  });

  it("connections decoder yields only bounded, distinct, well-formed ids", () => {
    fuzz(
      "connections",
      [
        "{",
        "}",
        "[",
        "]",
        ",",
        ":",
        '"',
        '"source"',
        '"account_id"',
        '"x"',
        "null",
        "1",
        "\\u0000",
        "é",
      ],
      (input, label) => {
        const got = decodeConnections(input);
        const v = parseOrUndefined(input);
        const shaped = plainObject(v) && Array.isArray(v.connections);
        if (!shaped) {
          assert.equal(got, null, label);
          return;
        }
        const ok = (s: unknown): s is string =>
          typeof s === "string" &&
          s.length >= 1 &&
          s.length <= MAX_ID_CHARS &&
          !/[\u0000-\u001f\u007f]/.test(s);
        const expected: Array<{ source: string; accountId: string }> = [];
        const keys = new Set<string>();
        const connections: unknown[] =
          plainObject(v) && Array.isArray(v.connections) ? v.connections : [];
        for (const c of connections) {
          if (expected.length === MAX_CONNECTIONS) break;
          if (!plainObject(c) || !ok(c.source) || !ok(c.account_id)) continue;
          const k = `${c.source}\u0000${c.account_id}`;
          if (keys.has(k)) continue;
          keys.add(k);
          expected.push({ source: c.source, accountId: c.account_id });
        }
        assert.deepEqual(got, expected, label);
      },
    );
  });

  it("plugin instance reader yields only wanted default instances with well-formed ids", () => {
    const wanted = ["audible", "libro", "chirp", "graphicaudio", "__proto__"];
    fuzz(
      "plugin_instances",
      [
        "{",
        "}",
        "[",
        "]",
        ",",
        ":",
        '"',
        "true",
        "false",
        "null",
        '"audible"',
        '"is_default"',
        '"node_id"',
        "\\u0001",
      ],
      (input, label) => {
        const v = parseOrUndefined(input);
        const ok = (s: unknown): s is string =>
          typeof s === "string" &&
          s.length >= 1 &&
          s.length <= MAX_ID_CHARS &&
          !/[\u0000-\u001f\u007f]/.test(s);
        const got = decodeInstances(input, wanted);
        if (!(plainObject(v) && ok(v.node_id) && Array.isArray(v.instances))) {
          assert.equal(got, null, label);
          return;
        }
        const expected = new Map<string, string>();
        for (const i of v.instances as unknown[]) {
          if (!plainObject(i)) continue;
          if (
            i.is_default !== true ||
            !ok(i.plugin_instance_id) ||
            typeof i.alias !== "string" ||
            !wanted.includes(i.alias) ||
            expected.has(i.alias)
          )
            continue;
          expected.set(i.alias, i.plugin_instance_id);
        }
        assert.equal(got!.nodeId, v.node_id, label);
        assert.deepEqual(got!.instances, expected, label);
      },
    );
  });

  it("launch form yields the first Turnstile field, bounded", () => {
    const check = (bytes: Uint8Array, label: string): void => {
      const value =
        bytes.length > MAX_FORM_BYTES ? null : oracleFormField(bytes, "cf-turnstile-response");
      let ascii = value !== null && value.length >= 1 && value.length <= MAX_TURNSTILE_TOKEN_CHARS;
      for (let i = 0; ascii && i < value!.length; i++) {
        const c = value!.charCodeAt(i);
        ascii = c > 0x20 && c < 0x7f;
      }
      const expected = ascii ? value : null;
      assert.equal(decodeLaunchForm(bytes), expected, label);
    };
    fuzz(
      "launch_form",
      [
        "&",
        "=",
        "+",
        "%",
        "%2",
        "%2B",
        "%26",
        "%3D",
        "%zz",
        "%E2%82%AC",
        "%FF",
        "\ufeff",
        "cf-turnstile-response",
        "cf-turnstile-response=",
        "€",
      ],
      (input, label) => {
        check(new TextEncoder().encode(input), label);
      },
    );
    // Size bounds: a token just at and just over its limit, and a body just
    // over the form limit.
    const enc = (t: string): Uint8Array => new TextEncoder().encode(t);
    check(enc(`cf-turnstile-response=${"a".repeat(MAX_TURNSTILE_TOKEN_CHARS)}`), "token at limit");
    check(
      enc(`cf-turnstile-response=${"a".repeat(MAX_TURNSTILE_TOKEN_CHARS + 1)}`),
      "token over limit",
    );
    const big = enc(`cf-turnstile-response=t&pad=${"a".repeat(MAX_FORM_BYTES)}`);
    assert.equal(decodeLaunchForm(big), null, "body over limit");
    assert.equal(decodeLaunchForm(enc("cf-turnstile-response=t")), "t", "small body");
  });

  it("deploy helper reads Cloudflare answers into well-formed widget data", () => {
    const printable = (v: unknown, max: number): v is string => {
      if (typeof v !== "string" || v.length < 1 || v.length > max) return false;
      for (let i = 0; i < v.length; i++) {
        const c = v.charCodeAt(i);
        if (c <= 0x20 || c >= 0x7f) return false;
      }
      return true;
    };
    fuzz(
      "cf_api",
      [
        "{",
        "}",
        "[",
        "]",
        ",",
        ":",
        '"',
        "true",
        "false",
        "null",
        "1.5",
        "-1",
        '"bookclerk-demo"',
        '"sitekey"',
        '"secret"',
        '"domains"',
        '"mode"',
        '"invisible"',
        '"success"',
        '"errors"',
        '"code"',
        " ",
        "\\u0000",
      ],
      (input, label) => {
        const v = parseOrUndefined(input);
        for (const status of [200, 500]) {
          const got = decodeApiResponse(status, input);
          if (status === 200 && plainObject(v) && v.success === true) {
            assert.deepEqual(got, { ok: true, result: v.result }, label);
            continue;
          }
          assert.equal(got.ok, false, label);
          const codes: number[] = [];
          if (plainObject(v) && Array.isArray(v.errors)) {
            for (const e of v.errors as unknown[])
              if (
                plainObject(e) &&
                typeof e.code === "number" &&
                Number.isInteger(e.code) &&
                Math.abs(e.code) <= Number.MAX_SAFE_INTEGER
              )
                codes.push(e.code);
          }
          assert.equal(got.codes, codes.slice(0, 10).join(","), label);
        }
        const result = plainObject(v) ? v.result : v;
        // Widget lookup: the first well-named widget with a usable site key.
        let expected: { sitekey: string; domains: string[]; mode: string } | null = null;
        if (Array.isArray(result)) {
          for (const w of result as unknown[]) {
            if (!plainObject(w) || w.name !== WIDGET_NAME || !printable(w.sitekey, 512)) continue;
            const seen = new Set<string>();
            for (const d of Array.isArray(w.domains) ? (w.domains as unknown[]) : []) {
              if (seen.size < MAX_DOMAINS && printable(d, 253)) seen.add(d);
            }
            const mode =
              ["managed", "non-interactive", "invisible"].includes(String(w.mode)) &&
              typeof w.mode === "string"
                ? w.mode
                : "managed";
            expected = { sitekey: w.sitekey, domains: [...seen], mode };
            break;
          }
        }
        assert.deepEqual(findWidget(result), expected, label);
        // Keys: both present and printable, or nothing.
        const keys =
          plainObject(result) && printable(result.sitekey, 512) && printable(result.secret, 512)
            ? { siteKey: result.sitekey, secret: result.secret }
            : null;
        assert.deepEqual(widgetKeys(result), keys, label);
      },
    );
  });

  it("Siteverify passes only success for this host and action", () => {
    const expect = { hostname: "demo.example.test", action: "demo_start" };
    fuzz(
      "siteverify",
      [
        "true",
        "false",
        '"true"',
        "1",
        "null",
        '"demo.example.test"',
        '"demo_start"',
        ",",
        "}",
        "{",
      ],
      (input, label) => {
        const v = parseOrUndefined(input);
        const expected =
          plainObject(v) &&
          v.success === true &&
          v.hostname === expect.hostname &&
          v.action === expect.action;
        assert.equal(siteverifyPassed(input, expect), expected, label);
      },
    );
  });

  it("claim ticket reader takes only a well-formed claim ticket", () => {
    fuzz(
      "claim_ticket",
      ["true", "false", '"', "-", "a", "{", "}", ",", ":", '"claim_ticket"', '"ok"'],
      (input, label) => {
        const v = parseOrUndefined(input);
        const t = plainObject(v) && v.ok === true ? v.claim_ticket : undefined;
        const expected =
          typeof t === "string" &&
          t.length >= 16 &&
          t.length <= 512 &&
          [...t].every((c) => /[A-Za-z0-9._~-]/.test(c))
            ? t
            : null;
        assert.equal(decodeClaimTicket(input), expected, label);
      },
    );
  });
  it("live status socket opens only for a same-origin WebSocket with a session", () => {
    const own = "https://demo.example.test";
    fuzz(
      "live_request",
      [
        "\n",
        "GET",
        "POST",
        "websocket",
        "WEBSOCKET",
        "\u212A",
        "\u017F",
        " ",
        own,
        "https://evil.example.test",
        "null",
        "0",
        "1",
      ],
      (input, label) => {
        // Lines: method, Upgrade, Origin, session flag; a missing line is an absent header.
        const [method = "", upgrade, origin, session] = input.split("\n");
        const r = {
          method,
          upgrade: upgrade ?? null,
          origin: origin ?? null,
          ownOrigin: own,
          hasSession: session === "1",
        };
        // Oracle: ASCII case-insensitive "websocket", letter by letter.
        const word = "websocket";
        const isWebSocket =
          r.upgrade !== null &&
          r.upgrade.length === word.length &&
          [...word].every((c, i) => {
            const got = r.upgrade!.charCodeAt(i);
            return got === c.charCodeAt(0) || got === c.charCodeAt(0) - 32;
          });
        const expected =
          method !== "GET" || !isWebSocket
            ? "not_websocket"
            : r.origin !== own
              ? "cross_origin"
              : r.hasSession
                ? "open"
                : "no_session";
        assert.equal(liveRequest(r), expected, label);
      },
    );
  });

  it("live status socket acts only on the exact word sync", () => {
    fuzz(
      "live_message",
      ["s", "y", "n", "c", "sync", "\n", " ", "\u0000", "{", "}"],
      (input, label) => {
        const expected =
          input.length === 4 && [115, 121, 110, 99].every((code, i) => input.charCodeAt(i) === code)
            ? "sync"
            : null;
        assert.equal(liveCommand(input), expected, label);
        // Binary frames are never a command.
        const bytes = new TextEncoder().encode(input);
        const frame = new ArrayBuffer(bytes.byteLength);
        new Uint8Array(frame).set(bytes);
        assert.equal(liveCommand(frame), null, label);
      },
    );
  });
});
