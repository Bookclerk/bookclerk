/**
 * Echo Fetch workerd guest module (api_version = 3).
 *
 * Default export extends `BookclerkEntrypoint` from
 * `@bookclerk/plugin-sdk/workerd` (injected by bookclerk-workerd); the `cli`
 * entrypoint is the exported `Cli` class.
 *
 * Probes https://www.example.com/ under the `*.example.com` allowlist.
 * Success = egress allowed a Response; HTTP status is best-effort only.
 */

import {
  BookclerkEntrypoint,
  CliEntrypoint,
  cliArgs,
  jsonPayload,
} from "@bookclerk/plugin-sdk/workerd";

const PLUGIN_ID = "echo_workerd_fetch";
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
          required: false,
          positional: false,
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

/** `cli` entrypoint: `ping` and `fetch-example`. */
export class Cli extends CliEntrypoint {
  async describe() {
    return CLI;
  }

  /**
   * @param {{ command: string, args: Array<{ name: string, value: string }> }} params
   */
  async invoke(params) {
    if (params?.command === "ping") {
      const { message = "hi" } = cliArgs(params);
      return {
        exitCode: 0,
        stdout: `pong: ${message}\n`,
        stderr: "",
        payload: jsonPayload({ pong: message }),
      };
    }
    if (params?.command === "fetch-example") {
      const probe = await probeExampleFetch();
      return {
        exitCode: probe.allowed ? 0 : 1,
        stdout: `${probe.detail}\n`,
        stderr: "",
        payload: jsonPayload({ allowed: probe.allowed, detail: probe.detail, url: EXAMPLE_URL }),
      };
    }
    return {
      exitCode: 2,
      stdout: "",
      stderr: `unknown command ${params?.command ?? ""}`,
      payload: jsonPayload(null),
    };
  }
}

/** Default entrypoint: event trigger for `[[events.consumers]]`. */
export default class EchoFetchPlugin extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Echo Fetch (workerd)" };
  }

  /**
   * @param {import("@bookclerk/plugin-sdk/workerd").EventBatch} batch
   */
  async event(batch) {
    for (const msg of batch.messages) {
      if (msg.type === "book_acquired") {
        const titleId = msg.json()?.titleId ?? "";
        console.log(`${PLUGIN_ID} saw book_acquired titleId=${titleId}`);
      }
      msg.ack();
    }
  }
}
