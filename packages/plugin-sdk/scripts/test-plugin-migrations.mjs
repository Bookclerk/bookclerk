import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const {
  MAX_LIST_PAGE,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  MAX_SCALAR_BYTES,
} = await import(join(dist, "abi.js"));
const { requirePluginMigrationRegistration } = await import(
  join(dist, "plugin-migrations.js")
);

function mig(id, sql = "x", ops = 1) {
  return {
    id,
    operations: Array.from({ length: ops }, () => ({ schema: sql })),
  };
}

function tooLarge(fn) {
  try {
    fn();
    assert.fail("expected payload_too_large");
  } catch (err) {
    assert.equal(err.code, "payload_too_large");
    return err;
  }
}

requirePluginMigrationRegistration(
  Array.from({ length: MAX_LIST_PAGE }, (_, i) => mig(`m${String(i).padStart(3, "0")}`)),
);

{
  const err = tooLarge(() =>
    requirePluginMigrationRegistration(
      Array.from({ length: MAX_LIST_PAGE + 1 }, (_, i) =>
        mig(`m${String(i).padStart(3, "0")}`),
      ),
    ),
  );
  assert.match(err.message, /maxListPage/);
}

requirePluginMigrationRegistration([mig("ops", "x", MAX_PLUGIN_MIGRATION_OPS)]);

{
  const err = tooLarge(() =>
    requirePluginMigrationRegistration([mig("ops", "x", MAX_PLUGIN_MIGRATION_OPS + 1)]),
  );
  assert.match(err.message, /maxPluginMigrationOps/);
}

{
  const per = Math.floor(MAX_PLUGIN_MIGRATION_OPS / 2);
  const n = MAX_PLUGIN_MIGRATION_TOTAL_OPS / per;
  requirePluginMigrationRegistration(
    Array.from({ length: n }, (_, i) => mig(`t${String(i).padStart(3, "0")}`, "x", per)),
  );
}

{
  const per = Math.floor(MAX_PLUGIN_MIGRATION_OPS / 2);
  const n = MAX_PLUGIN_MIGRATION_TOTAL_OPS / per;
  const migrations = Array.from({ length: n }, (_, i) =>
    mig(`t${String(i).padStart(3, "0")}`, "x", per),
  );
  migrations.push(mig("extra", "x", 1));
  const err = tooLarge(() => requirePluginMigrationRegistration(migrations));
  assert.match(err.message, /maxPluginMigrationTotalOps/);
}

requirePluginMigrationRegistration([mig("", "x".repeat(MAX_SCALAR_BYTES))]);

{
  const err = tooLarge(() =>
    requirePluginMigrationRegistration([mig("sql", "x".repeat(MAX_SCALAR_BYTES + 1))]),
  );
  assert.match(err.message, /maxScalarBytes/);
}

{
  const id = "a";
  const sql = "x".repeat(MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES - id.length);
  requirePluginMigrationRegistration([mig(id, sql)]);
}

{
  const half = Math.floor(MAX_SCALAR_BYTES / 2) + 1;
  const sql = "x".repeat(half);
  const err = tooLarge(() =>
    requirePluginMigrationRegistration([mig("a", sql), mig("b", sql)]),
  );
  assert.match(err.message, /maxPluginMigrationRegistrationBytes/);
}

console.log("plugin migration registration N/N+1 bounds ok");
