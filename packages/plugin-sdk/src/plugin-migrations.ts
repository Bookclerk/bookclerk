/**
 * Resource bounds for plugin-owned `databaseMigrations` registration.
 *
 * Semantic BookclerkSQL proof stays on the host. This module only rejects
 * pathological list / SQL sizes before Cap'n Proto or JSON encoding.
 */

import {
  MAX_LIST_PAGE,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  MAX_SCALAR_BYTES,
} from "./abi.js";
import type { PluginMigration, PluginMigrationOp } from "./plugin.js";

const utf8 = new TextEncoder();

function utf8Bytes(value: string): number {
  return utf8.encode(value).byteLength;
}

function opSql(op: PluginMigrationOp): string {
  if (op && typeof op === "object") {
    if ("schema" in op && typeof op.schema === "string") {
      return op.schema;
    }
    if ("data" in op && typeof op.data === "string") {
      return op.data;
    }
  }
  return "";
}

/**
 * Rejects a plugin migration registration that exceeds ABI resource limits.
 *
 * @param migrations - Ordered `databaseMigrations` registration.
 * @returns The same array when within limits.
 * @throws Error with `code` / `wireCode` `payload_too_large` when a limit is exceeded.
 */
export function requirePluginMigrationRegistration(
  migrations: PluginMigration[],
): PluginMigration[] {
  if (!Array.isArray(migrations)) {
    return [];
  }
  if (migrations.length > MAX_LIST_PAGE) {
    throw migrationTooLarge(
      `plugin migration count ${migrations.length} exceeds maxListPage (${MAX_LIST_PAGE})`,
    );
  }
  let total = 0;
  let totalOps = 0;
  for (const migration of migrations) {
    const ops = Array.isArray(migration.operations) ? migration.operations : [];
    if (ops.length > MAX_PLUGIN_MIGRATION_OPS) {
      throw migrationTooLarge(
        `plugin migration \`${migration.id}\` has ${ops.length} operations; exceeds maxPluginMigrationOps (${MAX_PLUGIN_MIGRATION_OPS})`,
      );
    }
    totalOps += ops.length;
    if (totalOps > MAX_PLUGIN_MIGRATION_TOTAL_OPS) {
      throw migrationTooLarge(
        `plugin migration registration has ${totalOps} operations; exceeds maxPluginMigrationTotalOps (${MAX_PLUGIN_MIGRATION_TOTAL_OPS})`,
      );
    }
    total += utf8Bytes(String(migration.id ?? ""));
    if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
      throw migrationTooLarge(
        `plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`,
      );
    }
    for (const op of ops) {
      const n = utf8Bytes(opSql(op));
      if (n > MAX_SCALAR_BYTES) {
        throw migrationTooLarge(
          `plugin migration \`${migration.id}\` SQL is ${n} bytes; exceeds maxScalarBytes (${MAX_SCALAR_BYTES})`,
        );
      }
      total += n;
      if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
        throw migrationTooLarge(
          `plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`,
        );
      }
    }
  }
  return migrations;
}

function migrationTooLarge(message: string): Error {
  const err = new Error(message) as Error & { code: string; wireCode: string };
  err.code = "payload_too_large";
  err.wireCode = "payload_too_large";
  return err;
}
