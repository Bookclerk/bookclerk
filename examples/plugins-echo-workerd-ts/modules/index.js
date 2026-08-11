/**
 * Echo workerd guest module.
 *
 * Extends package `BookclerkPlugin` from `@bookclerk/plugin-sdk/workerd`
 * (injected by bookclerk-workerd). Keep in sync with `src/index.ts`.
 */

import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";

const API_VERSION = 1;
const PLUGIN_ID = "echo_workerd_ts";
const KIND = "integration";

const CLI = {
  commands: [
    {
      name: "ping",
      about: "Probe echo plugin",
      args: [
        {
          name: "message",
          long: "message",
          kind: "string",
          default: "hi",
        },
      ],
    },
  ],
};

export default class EchoPlugin extends BookclerkPlugin {
  /** @param {{ apiVersion: number, config?: object }} _params */
  async handshake(_params) {
    return {
      apiVersion: API_VERSION,
      id: PLUGIN_ID,
      kind: KIND,
      displayName: "Echo Integration (workerd TypeScript)",
      capabilities: ["health", "diagnose", "onEvent", "cli"],
      cli: CLI,
    };
  }

  async health() {
    return {
      ok: true,
      id: PLUGIN_ID,
      enabled: true,
      detail: "echo workerd plugin ready",
    };
  }

  async diagnose() {
    return { lines: ["echo: ok"] };
  }

  /**
   * @param {{ type: string, payload?: { titleId?: string } }} event
   */
  async onEvent(event) {
    if (event?.type === "book_acquired") {
      const titleId = event.payload?.titleId ?? "";
      if (this.env?.HOST?.notify) {
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

  async cliDescribe() {
    return CLI;
  }

  /**
   * @param {{ command: string, args?: Record<string, unknown> }} params
   */
  async cliInvoke(params) {
    if (params?.command !== "ping") {
      return {
        exitCode: 2,
        stderr: `unknown command ${params?.command ?? ""}`,
      };
    }
    const message =
      typeof params.args?.message === "string" ? params.args.message : "hi";
    return {
      exitCode: 0,
      stdout: `pong: ${message}\n`,
      json: { pong: message },
    };
  }
}
