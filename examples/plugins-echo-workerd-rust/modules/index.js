/**
 * Workerd JS entry — Rust/Wasm Echo via `dispatch` (api_version = 3).
 *
 * The default `BookclerkEntrypoint` and the exported `Cli` entrypoint live in
 * JS so the guest matches the author model without a Wasm rebuild; CLI calls
 * forward to Wasm (`cliDescribe` / `cliInvoke`) and events are echoed to it
 * best-effort.
 */

import {
  BookclerkEntrypoint,
  CliEntrypoint,
  cliArgs,
  jsonPayload,
} from "@bookclerk/plugin-sdk/workerd";
import { initSync, dispatch } from "./pkg/bookclerk_plugin_echo_workerd_rust.js";
import wasmModule from "./pkg/bookclerk_plugin_echo_workerd_rust_bg.wasm";

initSync({ module: wasmModule });

const PLUGIN_ID = "echo_workerd_rust";

/** Call one Wasm method with JSON params; returns the parsed JSON result. */
function call(method, params) {
  const paramsJson =
    params === undefined || params === null ? "{}" : JSON.stringify(params);
  const out = dispatch(method, paramsJson);
  return out === "null" ? null : JSON.parse(out);
}

/** Rust serde emits `ExtensibleConfig.payload` as base64 text; RPC wants bytes. */
function bytesFromWasmPayload(payload) {
  if (payload instanceof Uint8Array) return payload;
  if (typeof payload !== "string" || payload === "") return new Uint8Array();
  const bin = atob(payload);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

const FALLBACK_CLI = {
  commands: [
    {
      name: "ping",
      about: "Probe echo plugin",
      args: [
        {
          name: "message",
          long: "message",
          kind: "string",
          required: false,
          positional: false,
          default: "hi",
        },
      ],
    },
  ],
};

/** `cli` entrypoint forwarding to the Wasm `cliDescribe` / `cliInvoke`. */
export class Cli extends CliEntrypoint {
  async describe() {
    try {
      return call("cliDescribe", {});
    } catch {
      return FALLBACK_CLI;
    }
  }

  /**
   * @param {{ command: string, args: Array<{ name: string, value: string }> }} params
   */
  async invoke(params) {
    try {
      const result = call("cliInvoke", params);
      const payload = result?.payload ?? {};
      return {
        exitCode: result?.exitCode ?? 0,
        stdout: result?.stdout ?? "",
        stderr: result?.stderr ?? "",
        payload: {
          schemaVersion: payload.schemaVersion ?? 1,
          mediaType: payload.mediaType ?? "",
          payload: bytesFromWasmPayload(payload.payload),
        },
      };
    } catch {
      if (params?.command !== "ping") {
        return {
          exitCode: 2,
          stdout: "",
          stderr: `unknown command ${params?.command ?? ""}`,
          payload: jsonPayload(null),
        };
      }
      const { message = "hi" } = cliArgs(params);
      return {
        exitCode: 0,
        stdout: `pong: ${message}\n`,
        stderr: "",
        payload: jsonPayload({ pong: message }),
      };
    }
  }
}

/** Default entrypoint: event trigger for `[[events.consumers]]`. */
export default class EchoPlugin extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Echo Integration (workerd Rust/Wasm)" };
  }

  /**
   * @param {import("@bookclerk/plugin-sdk/workerd").EventBatch} batch
   */
  async event(batch) {
    for (const msg of batch.messages) {
      try {
        call("onEvent", { eventType: msg.type, schemaVersion: msg.schemaVersion });
      } catch {
        // wasm dispatch may not handle onEvent; ack anyway
      }
      if (msg.type === "book_acquired") {
        const titleId = msg.json()?.titleId ?? "";
        console.log(`${PLUGIN_ID} saw book_acquired titleId=${titleId}`);
      }
      msg.ack();
    }
  }
}
