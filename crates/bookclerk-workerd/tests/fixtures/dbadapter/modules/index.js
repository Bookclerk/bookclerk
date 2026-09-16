/**
 * Contract fixture: `DatabaseAdapter` entrypoint whose sessions live in the
 * author isolate between `/invoke` calls (Workers RPC stubs cannot be
 * retained across HTTP requests, so `openSession` hands back an object id
 * the launcher addresses with `X-Bookclerk-Target`).
 *
 * The in-memory engine answers `SELECT 1` with one `int64` row and rejects
 * anything else with a typed `unsupported` error.
 */

import {
  AdapterDatabaseSession,
  BookclerkEntrypoint,
  DatabaseAdapterEntrypoint,
  PluginError,
} from "@bookclerk/plugin-sdk/workerd";

let opened = 0;
let closed = 0;

class MemorySession extends AdapterDatabaseSession {
  constructor() {
    super();
    this.isClosed = false;
  }

  async capabilities() {
    return {
      sqlContractVersion: 1,
      atomicBatch: true,
      returning: false,
      affectedRows: true,
      schemaMigrations: false,
      cancellation: false,
      timing: false,
      maxBinds: 32,
      maxStatements: 8,
      maxResultRows: 1000,
      maxPayloadBytes: 65536,
      maxResultBytes: 262144,
      maxCellBytes: 65536,
      maxRequestBytes: 262144,
      maxAtomicResultBytes: 262144,
    };
  }

  async execute(request) {
    if (this.isClosed) {
      throw PluginError.fromWire("unavailable", "session closed");
    }
    const statements = (request.statements || []).map((stmt) => {
      if (stmt.sql.trim().toUpperCase() !== "SELECT 1") {
        throw PluginError.fromWire("unsupported", `memory engine cannot run ${stmt.sql}`);
      }
      return {
        rows: [{ values: [{ kind: "int64", value: 1n }] }],
        columns: [{ name: "1", dbType: "int64" }],
        rowsAffected: 0,
      };
    });
    return {
      operationId: request.operationId,
      statements,
      timing: { attemptElapsedUs: 1, dbExecutionUs: 0, dbTimingSource: "none" },
    };
  }

  async close() {
    this.isClosed = true;
    closed += 1;
  }

  async bootstrap() {
    return { engine: `memory opened=${opened} closed=${closed}` };
  }
}

export class DatabaseAdapter extends DatabaseAdapterEntrypoint {
  async openSession() {
    opened += 1;
    return new MemorySession();
  }
}

export default class DbAdapterPlugin extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Database adapter contract fixture" };
  }
}
