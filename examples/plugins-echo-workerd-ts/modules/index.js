/**
 * Echo workerd guest module (api_version = 2).
 *
 * Extends package `BookclerkPlugin` from `@bookclerk/plugin-sdk/workerd`
 * (injected by bookclerk-workerd). Keep in sync with `src/index.ts`.
 */

import {
  BookclerkPlugin,
  Integration,
  PRODUCT_API_VERSION,
  FEATURE_SCALAR_LIMITS,
} from "@bookclerk/plugin-sdk/workerd";

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

class EchoIntegration extends Integration {
  /**
   * @param {Record<string, unknown> | undefined} env
   */
  constructor(env) {
    super();
    this.env = env;
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
   * @param {{ type?: string, eventType?: string, payload?: { titleId?: string } | Uint8Array }} event
   */
  async onEvent(event) {
    const type = event?.type || event?.eventType || "";
    let titleId = "";
    const payload = event?.payload;
    if (payload && typeof payload === "object" && "titleId" in payload) {
      titleId = payload.titleId ?? "";
    }
    if (type === "book_acquired" && this.env?.HOST?.notify) {
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
      id: PLUGIN_ID,
      kind: KIND,
      displayName: "Echo Integration (workerd TypeScript)",
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
    return CLI;
  }

  /**
   * @param {string | { command: string, args?: Record<string, unknown> }} params
   */
  async cliInvoke(params) {
    const parsed =
      typeof params === "string" ? JSON.parse(params || "{}") : params || {};
    if (parsed?.command !== "ping") {
      return {
        exitCode: 2,
        stderr: `unknown command ${parsed?.command ?? ""}`,
      };
    }
    const message =
      typeof parsed.args?.message === "string" ? parsed.args.message : "hi";
    return {
      exitCode: 0,
      stdout: `pong: ${message}\n`,
      json: { pong: message },
    };
  }
}
