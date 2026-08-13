/**
 * Echo Fetch workerd guest module.
 *
 * Extends package `BookclerkPlugin` from `@bookclerk/plugin-sdk/workerd`
 * (injected by bookclerk-workerd). Keep in sync with `src/index.ts`.
 *
 * Probes https://www.example.com/ under the `*.example.com` allowlist.
 * Success = egress allowed a Response; HTTP status is best-effort only.
 */

import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";

const API_VERSION = 1;
const PLUGIN_ID = "echo_workerd_fetch";
const KIND = "integration";
const EXAMPLE_URL = "https://www.example.com/";

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
    {
      name: "fetch-example",
      about:
        "GET https://www.example.com/ (succeeds if allowlisted; HTTP status ignored)",
      args: [],
    },
  ],
};

/**
 * @returns {Promise<{ allowed: boolean, detail: string }>}
 */
async function probeExampleFetch() {
  try {
    const res = await fetch(EXAMPLE_URL);
    return {
      allowed: true,
      detail: `example.com fetch allowed (HTTP ${res.status})`,
    };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      allowed: false,
      detail: `example.com fetch denied or failed before response: ${message}`,
    };
  }
}

export default class EchoFetchPlugin extends BookclerkPlugin {
  /** @param {{ apiVersion: number, config?: object }} _params */
  async handshake(_params) {
    return {
      apiVersion: API_VERSION,
      id: PLUGIN_ID,
      kind: KIND,
      displayName: "Echo Fetch (workerd)",
      capabilities: ["health", "diagnose", "onEvent", "cli"],
      cli: CLI,
    };
  }

  async health() {
    return {
      ok: true,
      id: PLUGIN_ID,
      enabled: true,
      detail: "echo workerd fetch plugin ready",
    };
  }

  async diagnose() {
    const probe = await probeExampleFetch();
    return {
      lines: [
        "echo_workerd_fetch: ok",
        probe.detail,
        probe.allowed
          ? "allowlist probe: passed (response received)"
          : "allowlist probe: failed (no response — treat as deny/block)",
      ],
    };
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
            message: `echo_workerd_fetch saw book_acquired titleId=${titleId}`,
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
    if (params?.command === "ping") {
      const message =
        typeof params.args?.message === "string" ? params.args.message : "hi";
      return {
        exitCode: 0,
        stdout: `pong: ${message}\n`,
        json: { pong: message },
      };
    }
    if (params?.command === "fetch-example") {
      const probe = await probeExampleFetch();
      return {
        exitCode: probe.allowed ? 0 : 1,
        stdout: `${probe.detail}\n`,
        json: { allowed: probe.allowed, detail: probe.detail, url: EXAMPLE_URL },
      };
    }
    return {
      exitCode: 2,
      stderr: `unknown command ${params?.command ?? ""}`,
    };
  }
}
