/**
 * Echo workerd guest (typed source; `modules/index.js` is the shipped module).
 *
 * Default export extends `BookclerkEntrypoint` and handles the `book_acquired`
 * event trigger; the `cli` entrypoint is the exported `Cli` class. Run
 * `npx bookclerk-plugin types .` to regenerate `bookclerk-configuration.d.ts`.
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

const PLUGIN_ID = "echo_workerd_ts";

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
  ],
};

/** `cli` entrypoint: `bookclerk plugins echo_workerd_ts ping --message hi`. */
export class Cli extends CliEntrypoint<Env> {
  override async describe(): Promise<CliSchema> {
    return CLI;
  }

  override async invoke(params: CliInvokeParams): Promise<CliInvokeResult> {
    if (params.command !== "ping") {
      return {
        exitCode: 2,
        stdout: "",
        stderr: `unknown command ${params.command}`,
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

/** Default entrypoint: event trigger for `[[events.consumers]]`. */
export default class EchoPlugin extends BookclerkEntrypoint<Env> {
  override async describe(): Promise<PluginDescribe> {
    return { displayName: "Echo Integration (workerd TypeScript)" };
  }

  override async event(batch: EventBatch): Promise<void> {
    for (const msg of batch.messages) {
      switch (msg.type) {
        case "book_acquired": {
          const titleId = msg.json<{ titleId?: string }>().titleId ?? "";
          console.log(`${PLUGIN_ID} saw book_acquired titleId=${titleId}`);
          msg.ack();
          break;
        }
        case "test_retry":
          msg.retry({ delaySeconds: 1, reason: "echo retry" });
          break;
        case "test_reject":
          msg.reject("echo reject");
          break;
        case "test_dead_letter":
          msg.deadLetter("echo dead letter");
          break;
        case "test_suspend":
          msg.suspend({ checkpoint: { n: 1 }, wakeAt: 1 });
          break;
        default:
          msg.ack();
      }
    }
  }
}
