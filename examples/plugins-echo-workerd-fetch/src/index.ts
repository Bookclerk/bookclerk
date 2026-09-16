/**
 * Echo Fetch workerd guest (typed source; `modules/index.js` is the shipped
 * module). Requests outbound `*.example.com` and probes
 * `https://www.example.com/` from the `fetch-example` CLI command.
 */

import {
  BookclerkEntrypoint,
  CliEntrypoint,
  cliArgs,
  jsonPayload,
  type CliInvokeParams,
  type CliInvokeResult,
  type CliSchema,
  type EventBatch,
  type PluginDescribe,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";

const PLUGIN_ID = "echo_workerd_fetch";
const EXAMPLE_URL = "https://www.example.com/";

const CLI: CliSchema = {
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
 * Probe `www.example.com` under the `*.example.com` consent allowlist.
 *
 * Success means egress allowed the request (a Response was returned). HTTP
 * status is best-effort and must not fail the probe — example.com may return
 * any status or be intermittently unreachable at the origin while still
 * proving the allowlist hop worked.
 */
async function probeExampleFetch(): Promise<{ allowed: boolean; detail: string }> {
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
export class Cli extends CliEntrypoint<Env> {
  override async describe(): Promise<CliSchema> {
    return CLI;
  }

  override async invoke(params: CliInvokeParams): Promise<CliInvokeResult> {
    if (params.command === "ping") {
      const { message = "hi" } = cliArgs(params);
      return {
        exitCode: 0,
        stdout: `pong: ${message}\n`,
        stderr: "",
        payload: jsonPayload({ pong: message }),
      };
    }
    if (params.command === "fetch-example") {
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
      stderr: `unknown command ${params.command}`,
      payload: jsonPayload(null),
    };
  }
}

/** Default entrypoint: event trigger for `[[events.consumers]]`. */
export default class EchoFetchPlugin extends BookclerkEntrypoint<Env> {
  override async describe(): Promise<PluginDescribe> {
    return { displayName: "Echo Fetch (workerd)" };
  }

  override async event(batch: EventBatch): Promise<void> {
    for (const msg of batch.messages) {
      if (msg.type === "book_acquired") {
        const titleId = msg.json<{ titleId?: string }>().titleId ?? "";
        console.log(`${PLUGIN_ID} saw book_acquired titleId=${titleId}`);
      }
      msg.ack();
    }
  }
}
