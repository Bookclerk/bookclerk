// Round-trip Cap'n codecs for nullable Entrypoints interface fields.
// Generated write must skip exportCap(null); read must omit null caps.
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const { EntrypointsCodec, encodeMessage, decodeMessage } = await import(
  join(dist, "generated-wire.js")
);

function capTable() {
  /** @type {unknown[]} */
  const slots = [];
  return {
    exportCap(value) {
      if (typeof value !== "string" || !value) {
        throw new Error("exportCap requires a non-empty object id");
      }
      const i = slots.length;
      slots.push(value);
      return i;
    },
    importCap(index) {
      if (index === null || index === undefined) return null;
      return slots[index] ?? null;
    },
  };
}

function roundTrip(value) {
  const caps = capTable();
  const bytes = encodeMessage(EntrypointsCodec, value, caps);
  return decodeMessage(EntrypointsCodec, bytes, caps);
}

function assertAbsent(decoded, names) {
  for (const name of names) {
    assert.equal(decoded[name], undefined, `${name} must stay absent`);
  }
}

const ALL = [
  "eventConsumer",
  "jobRunner",
  "storefront",
  "storage",
  "databaseAdapter",
  "remoteLibrary",
  "cli",
  "oidc",
];

{
  const decoded = roundTrip({});
  assertAbsent(decoded, ALL);
}

{
  const decoded = roundTrip({ storefront: "sf" });
  assert.equal(decoded.storefront, "sf");
  assertAbsent(
    decoded,
    ALL.filter((n) => n !== "storefront"),
  );
}

{
  const decoded = roundTrip({
    storefront: "sf",
    cli: "cli",
    eventConsumer: "ev",
  });
  assert.equal(decoded.storefront, "sf");
  assert.equal(decoded.cli, "cli");
  assert.equal(decoded.eventConsumer, "ev");
  assertAbsent(decoded, ["jobRunner", "storage", "databaseAdapter", "remoteLibrary", "oidc"]);
}

{
  const all = Object.fromEntries(ALL.map((n) => [n, n]));
  const decoded = roundTrip(all);
  for (const name of ALL) {
    assert.equal(decoded[name], name);
  }
}

{
  const decoded = roundTrip({ eventConsumer: "ev", jobRunner: "job" });
  assert.equal(decoded.eventConsumer, "ev");
  assert.equal(decoded.jobRunner, "job");
  assertAbsent(
    decoded,
    ALL.filter((n) => n !== "eventConsumer" && n !== "jobRunner"),
  );
}

{
  const decoded = roundTrip({ cli: "cli-only" });
  assert.equal(decoded.cli, "cli-only");
  assertAbsent(
    decoded,
    ALL.filter((n) => n !== "cli"),
  );
}

{
  const caps = capTable();
  assert.doesNotThrow(() => encodeMessage(EntrypointsCodec, { storefront: null }, caps));
  const decoded = decodeMessage(
    EntrypointsCodec,
    encodeMessage(EntrypointsCodec, { storefront: null }, capTable()),
    capTable(),
  );
  assert.equal(decoded.storefront, undefined);
}

console.log("ok: Entrypoints nullable interface codec round-trips");
