/**
 * Workerd JS entry — Rust/Wasm Echo via `dispatch` (api_version = 2).
 *
 * `describe` + `integration` live in JS so the guest matches the BookclerkPlugin contract
 * without a Wasm rebuild. Health / diagnose / CLI still forward to Wasm.
 */

import {
  BookclerkPlugin,
  Integration,
  PRODUCT_API_VERSION,
  FEATURE_SCALAR_LIMITS,
} from "@bookclerk/plugin-sdk/workerd";
import { initSync, dispatch } from "./pkg/bookclerk_plugin_echo_workerd_rust.js";
import wasmModule from "./pkg/bookclerk_plugin_echo_workerd_rust_bg.wasm";

initSync({ module: wasmModule });

function call(method, params) {
  const paramsJson =
    params === undefined || params === null ? "{}" : JSON.stringify(params);
  const out = dispatch(method, paramsJson);
  return out === "null" ? null : JSON.parse(out);
}

class EchoIntegration extends Integration {
  /**
   * @param {Record<string, unknown> | undefined} env
   */
  constructor(env) {
    super();
    this.env = env;
  }

  async health() {
    try {
      return call("health", {});
    } catch {
      return {
        ok: true,
        id: "echo_workerd_rust",
        enabled: true,
        detail: "echo workerd rust wasm plugin ready",
      };
    }
  }

  async diagnose() {
    try {
      return call("diagnose", {});
    } catch {
      return { lines: ["echo_workerd_rust: ok"] };
    }
  }

  async onEvent(event) {
    try {
      call("onEvent", event);
    } catch {
      // wasm dispatch may not handle onEvent; ack anyway
    }
    if (event?.type === "book_acquired" && this.env?.HOST?.notify) {
      const titleId = event.payload?.titleId ?? "";
      await this.env.HOST.notify({
        type: "plugin_log",
        payload: {
          level: "info",
          message: `echo saw book_acquired titleId=${titleId}`,
        },
      });
    }
    return { kind: "ack" };
  }
}

export default class EchoPlugin extends BookclerkPlugin {
  async describe() {
    return {
      apiVersion: PRODUCT_API_VERSION ?? 2,
      id: "echo_workerd_rust",
      kind: "integration",
      displayName: "Echo Integration (workerd Rust/Wasm)",
      rpcFeatures: [FEATURE_SCALAR_LIMITS ?? "rpc.scalarLimits"],
      scalarLimits: {
        maxScalarBytes: 262144,
        maxStreamWindowBytes: 1048576,
        maxListPage: 256,
      },
      supportedRoles: ["integration"],
    };
  }

  integration() {
    return new EchoIntegration(this.env);
  }

  async cliDescribe() {
    try {
      return call("cliDescribe", {});
    } catch {
      return {
        commands: [
          {
            name: "ping",
            about: "Probe echo plugin",
            args: [{ name: "message", long: "message", kind: "string", default: "hi" }],
          },
        ],
      };
    }
  }

  async cliInvoke(params) {
    const parsed =
      typeof params === "string" ? JSON.parse(params || "{}") : params || {};
    try {
      return call("cliInvoke", parsed);
    } catch {
      const message =
        typeof parsed.args?.message === "string" ? parsed.args.message : "hi";
      if (parsed.command !== "ping") {
        return { exitCode: 2, stderr: `unknown command ${parsed.command ?? ""}` };
      }
      return { exitCode: 0, stdout: `pong: ${message}\n`, json: { pong: message } };
    }
  }
}
