/**
 * Echo workerd guest (id `echo_native_node`, api_version = 2).
 *
 * This example now validates workerd hosting, not a Node Cap'n Proto stack.
 * Pattern matches `plugins-echo-workerd-ts`.
 */

import {
  BookclerkPlugin,
  Integration,
  PRODUCT_API_VERSION,
  FEATURE_SCALAR_LIMITS,
} from "@bookclerk/plugin-sdk/workerd";

const PLUGIN_ID = "echo_native_node";
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
      detail: "echo_native_node ready",
    };
  }

  async diagnose() {
    return { lines: ["echo_native_node diagnose: ok"] };
  }

  async onEvent(_event) {
    return { kind: "ack" };
  }
}

export default class EchoPlugin extends BookclerkPlugin {
  async describe() {
    return {
      apiVersion: PRODUCT_API_VERSION ?? 2,
      id: PLUGIN_ID,
      kind: KIND,
      displayName: "Echo Integration (native Node)",
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
