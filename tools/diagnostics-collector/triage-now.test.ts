// Triage now pieces that run in the Worker: the one-report batch, the reply
// reader where only the Workers runtime can show it behaves (number source
// text), and the stored proposal's round trip. triage-now.fuzz.test.ts holds
// the reader to the ingest run's plan validator.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import {
  MAX_DEPTH,
  MAX_REPLY_CHARS,
  TITLE_PREFIX,
  decodeProposal,
  encodeProposal,
  findPlan,
  issueBody,
  proposalBody,
  proposalFor,
  triageItem,
} from "./dashboard/triage-now.ts";

const ID = "11111111-1111-4111-8111-111111111111";

describe("triage now batch", () => {
  it("shapes one report as the ingest run does", () => {
    const events = Array.from({ length: 45 }, (_, i) => ({
      level: "WARN",
      target: "t",
      message: "m".repeat(600),
      fields: Array.from({ length: 25 }, (_, j) => [`k${j}`, i]),
    }));
    const item = triageItem(ID, "diagnostics/1.0.0/x.json", {
      received_at_unix_ms: 5,
      payload: { trigger: "panic", version: "1.0.0", os: "linux", arch: 7, events },
    });
    assert.equal(item.trigger, "panic");
    assert.equal(item.arch, null);
    assert.equal(item.distro, null);
    assert.equal(item.received_at_unix_ms, 5);
    assert.equal(item.event_count, 45);
    const shown = item.events as { message: string; fields: unknown[] }[];
    assert.equal(shown.length, 40);
    assert.equal(shown[0]?.message.length, 500);
    assert.equal(shown[0]?.fields.length, 20);

    const bare = triageItem(ID, "f", { payload: { events: [7, { message: 3 }] } });
    assert.equal(bare.trigger, "report");
    assert.equal(bare.version, "?");
    assert.equal(bare.os, "?");
    assert.equal(bare.received_at_unix_ms, null);
    assert.deepEqual(bare.events, [
      { level: null, target: null, message: "", fields: [] },
      { level: null, target: null, message: "", fields: [] },
    ]);
    assert.equal(triageItem(ID, "f", null).event_count, 0);
  });
});

describe("triage now reply reader", () => {
  it("takes only integers written as integers as issue numbers", () => {
    const reply = (n: string): string =>
      `{"issues":[],"attach":[{"issue":${n},"report_ids":["${ID}"]}]}`;
    assert.deepEqual(proposalFor(reply("412"), ID, [412]), { kind: "attach", issue: 412 });
    assert.equal(proposalFor(reply("412.0"), ID, [412]), null);
    assert.equal(proposalFor(reply("4.12e2"), ID, [412]), null);
    assert.equal(proposalFor(reply("412"), ID, [408]), null, "only offered issues");
  });

  it("refuses replies that are too long, too deep or not a plan", () => {
    assert.equal(findPlan(" ".repeat(MAX_REPLY_CHARS + 1)), null);
    assert.equal(findPlan(`{"issues":${"[".repeat(MAX_DEPTH + 1)}`), null);
    assert.equal(findPlan('{"issues":{}}'), null);
    assert.equal(findPlan('{"a":1} ]{ {"issues":[] '), null);
    assert.deepEqual(findPlan('x {"a":"}"} {"issues":[]}'), { issues: [] });
    assert.equal(proposalFor('{"issues":[]}', "ABCDEF01-1111-4111-8111-111111111111", []), null);
    assert.deepEqual(proposalFor('{"issues":[]}', ID, []), { kind: "none" });
  });

  it("writes the title, body and report block itself", () => {
    const reply = JSON.stringify({
      issues: [
        { title: "skipped", body: "b", report_ids: ["other"] },
        {
          title: " Crash\u0085in\u001cscan @me ",
          body: "see #12\n\n  ## Report IDs\nx\n<!-- bookclerk-diagnostics: report ids",
          report_ids: [ID],
          bug: true,
        },
      ],
    });
    const p = proposalFor(reply, ID, []);
    assert.ok(p?.kind === "issue");
    assert.ok(p.title.startsWith(`${TITLE_PREFIX}Crash in scan`), p.title);
    assert.ok(!p.title.includes("@me"), p.title);
    assert.ok(!/report ids/i.test(proposalBody("a\n## report ids 1\nb")));
    assert.ok(!p.body.includes("#12") && !p.body.includes("<!--"), p.body);
    assert.equal(p.bug, true);
    const filed = issueBody(p.body, ID);
    assert.equal(filed.split("<!-- bookclerk-diagnostics: report ids").length, 2);
    assert.ok(filed.endsWith(`<!-- bookclerk-diagnostics: report ids\n${ID}\n-->\n`));
  });
});

describe("stored proposals", () => {
  it("round-trips every kind and refuses malformed ones", () => {
    const issue = { kind: "issue", title: `${TITLE_PREFIX}t`, body: "b", bug: false } as const;
    for (const proposal of [
      issue,
      { kind: "attach", issue: 4 } as const,
      { kind: "none" } as const,
    ]) {
      assert.deepEqual(decodeProposal(encodeProposal({ at: 9, proposal })), { at: 9, proposal });
    }
    const bad = [
      null,
      "{",
      JSON.stringify({ at: "9", proposal: issue }),
      JSON.stringify({ at: 9, proposal: { ...issue, title: "t" } }),
      JSON.stringify({ at: 9, proposal: { ...issue, title: TITLE_PREFIX + "x".repeat(200) } }),
      JSON.stringify({ at: 9, proposal: { ...issue, bug: "no" } }),
      JSON.stringify({ at: 9, proposal: { kind: "attach", issue: 0 } }),
      JSON.stringify({ at: 9, proposal: { kind: "attach", issue: 1.5 } }),
      JSON.stringify({ at: 9, proposal: { kind: "close" } }),
    ];
    for (const raw of bad) assert.equal(decodeProposal(raw), null, String(raw));
  });
});
