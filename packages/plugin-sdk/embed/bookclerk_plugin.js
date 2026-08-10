/**
 * Workerd runtime for `@bookclerk/plugin-sdk` / `@bookclerk/plugin-sdk/workerd`.
 *
 * Authors import the package — never a relative embed path:
 *
 *   import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
 *   // or: import { BookclerkPlugin } from "@bookclerk/plugin-sdk";
 *
 *   import { wasmBookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"; // Rust/Wasm glue
 *
 * `bookclerk-workerd` injects this module into the isolate under those names.
 * Native guests use `@bookclerk/plugin-sdk/native` (`BookclerkPluginGuest`) instead.
 */

import { WorkerEntrypoint } from "cloudflare:workers";

function unsupported(method) {
  return Object.assign(new Error(`${method} not implemented`), {
    code: "unsupported",
  });
}

export class BookclerkPlugin extends WorkerEntrypoint {
  /** Required by workerd when the entrypoint is not HTTP-facing. */
  async fetch() {
    return new Response(null, { status: 404 });
  }

  /** Identity, capabilities, CLI schema, brand — required. */
  async handshake(_params) {
    throw unsupported("handshake");
  }

  async shutdown() {}

  async health() {
    return { ok: true };
  }

  async diagnose() {
    return { lines: [] };
  }

  async onEvent(_event) {
    throw unsupported("onEvent");
  }

  async cliDescribe() {
    return { commands: [] };
  }

  async cliInvoke(_params) {
    throw unsupported("cliInvoke");
  }
}

/**
 * BookclerkPlugin subclass that forwards Workers RPC methods to a Wasm
 * `dispatch(method, paramsJson) -> resultJson` export (wasm-bindgen).
 *
 * @param {(method: string, paramsJson: string) => string} dispatch
 * @returns {typeof BookclerkPlugin}
 */
export function wasmBookclerkPlugin(dispatch) {
  return class WasmBookclerkPlugin extends BookclerkPlugin {
    #call(method, params) {
      const paramsJson =
        params === undefined || params === null ? "{}" : JSON.stringify(params);
      const out = dispatch(method, paramsJson);
      return out === "null" ? null : JSON.parse(out);
    }

    async handshake(params) {
      return this.#call("handshake", params);
    }

    async shutdown() {
      this.#call("shutdown", {});
    }

    async health() {
      return this.#call("health", {});
    }

    async diagnose() {
      return this.#call("diagnose", {});
    }

    async onEvent(event) {
      this.#call("onEvent", event);
    }

    async cliDescribe() {
      return this.#call("cliDescribe", {});
    }

    async cliInvoke(params) {
      return this.#call("cliInvoke", params);
    }
  };
}
