/**
 * Workerd JS entry — Rust/Wasm Echo via package `wasmBookclerkPlugin`.
 *
 *   import { wasmBookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
 *
 * Authoring logic lives in Rust (`src/lib.rs`). Rebuild with `./build-wasm.sh`.
 */

import { wasmBookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
import { initSync, dispatch } from "./pkg/bookclerk_plugin_echo_workerd_rust.js";
import wasmModule from "./pkg/bookclerk_plugin_echo_workerd_rust_bg.wasm";

initSync({ module: wasmModule });

const Base = wasmBookclerkPlugin(dispatch);

export default class EchoPlugin extends Base {
  async onEvent(event) {
    await super.onEvent(event);
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
  }
}
