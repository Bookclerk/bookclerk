#!/usr/bin/env node
/**
 * Minimal Bookclerk Echo integration guest (experimental TypeScript/Node).
 *
 * Speaks newline-delimited JSON-RPC 2.0 on stdio (jsonrpc-stdio-v1).
 * Implements handshake / health / cli.invoke ping for conformance probes.
 */

import readline from "node:readline";

const API_VERSION = 1;
const PLUGIN_ID = "echo";
const KIND = "integration";

const CLI_SCHEMA = {
  commands: [
    {
      name: "ping",
      about: "Probe echo plugin",
      args: [
        {
          name: "message",
          long: "message",
          short: "m",
          kind: "string",
          required: false,
          default: "hi",
          about: "Message to echo",
          positional: false,
        },
      ],
    },
  ],
};

function respond(id, result) {
  process.stdout.write(
    JSON.stringify({ jsonrpc: "2.0", id: id ?? null, result }) + "\n",
  );
}

function respondError(id, message) {
  process.stdout.write(
    JSON.stringify({
      jsonrpc: "2.0",
      id: id ?? null,
      error: { code: -32000, message },
    }) + "\n",
  );
}

function handle(method, params) {
  switch (method) {
    case "handshake":
      return {
        api_version: API_VERSION,
        id: PLUGIN_ID,
        kind: KIND,
        display_name: "Echo Integration (TypeScript)",
        capabilities: ["health", "cli"],
        cli: CLI_SCHEMA,
      };
    case "health":
      return {
        id: PLUGIN_ID,
        enabled: true,
        ok: true,
        detail: "echo-ts plugin ready",
      };
    case "cli.describe":
      return CLI_SCHEMA;
    case "cli.invoke": {
      const command = params?.command;
      if (command !== "ping") {
        throw new Error(`unknown command: ${command}`);
      }
      const message =
        typeof params?.args?.message === "string" ? params.args.message : "hi";
      return {
        exit_code: 0,
        stdout: `pong: ${message}\n`,
        stderr: "",
        json: { pong: message },
      };
    }
    case "shutdown":
      return null;
    default:
      throw new Error(`method not found: ${method}`);
  }
}

const rl = readline.createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
});

for await (const line of rl) {
  if (!line.trim()) continue;
  let req;
  try {
    req = JSON.parse(line);
  } catch (err) {
    process.stderr.write(`invalid request: ${err}\n`);
    continue;
  }
  const isShutdown = req.method === "shutdown";
  try {
    const result = handle(req.method, req.params ?? null);
    respond(req.id, result);
  } catch (err) {
    if (isShutdown) {
      respond(req.id, null);
    } else {
      respondError(req.id, err instanceof Error ? err.message : String(err));
    }
  }
  if (isShutdown) break;
}
