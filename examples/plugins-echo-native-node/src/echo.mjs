#!/usr/bin/env node
/**
 * Reference Echo integration — native Node (SEA or `node` runner).
 *
 * Subclasses BookclerkPlugin; BookclerkPluginGuest.serve is the stdio runner.
 */

import path from "node:path";
import { pathToFileURL } from "node:url";
import { fileURLToPath } from "node:url";
import { existsSync } from "node:fs";

const here = path.dirname(fileURLToPath(import.meta.url));

function resolveSdkNative() {
  if (process.env.BOOKCLERK_PLUGIN_SDK_NATIVE) {
    return path.resolve(process.env.BOOKCLERK_PLUGIN_SDK_NATIVE);
  }
  const staged = path.resolve(here, "../sdk/native.js");
  if (existsSync(staged)) return staged;
  return path.resolve(here, "../../../packages/plugin-sdk/dist/native.js");
}

const { BookclerkPlugin, BookclerkPluginGuest } = await import(
  pathToFileURL(resolveSdkNative()).href
);

const API_VERSION = 1;
const PLUGIN_ID = "echo_native_node";
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

class EchoPlugin extends BookclerkPlugin {
  handshake() {
    return {
      apiVersion: API_VERSION,
      id: PLUGIN_ID,
      kind: KIND,
      displayName: "Echo Integration (native Node)",
      capabilities: ["health", "diagnose", "onEvent", "cli"],
      cli: CLI_SCHEMA,
    };
  }

  health() {
    return {
      id: PLUGIN_ID,
      enabled: true,
      ok: true,
      detail: "echo_native_node ready",
    };
  }

  diagnose() {
    return { lines: ["echo_native_node diagnose: ok"] };
  }

  onEvent() {}

  cliDescribe() {
    return CLI_SCHEMA;
  }

  cliInvoke(params) {
    const command = params?.command;
    if (command !== "ping") {
      throw Object.assign(new Error(`unknown command: ${command}`), {
        code: "invalid_params",
      });
    }
    const message =
      typeof params?.args?.message === "string" ? params.args.message : "hi";
    return {
      exitCode: 0,
      stdout: `pong: ${message}\n`,
      stderr: "",
    };
  }
}

await BookclerkPluginGuest.serve(new EchoPlugin());
