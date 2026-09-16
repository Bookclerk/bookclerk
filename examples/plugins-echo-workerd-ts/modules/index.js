/**
 * Echo workerd guest module (api_version = 3).
 *
 * Default export extends `BookclerkEntrypoint` from
 * `@bookclerk/plugin-sdk/workerd` (injected by bookclerk-workerd) and handles
 * the `book_acquired` event trigger declared in `plugin.toml`. The `cli`
 * entrypoint is the exported `Cli` class. Keep in sync with `src/index.ts`.
 */

import {
  BookclerkEntrypoint,
  CliEntrypoint,
  cliArgs,
  jsonPayload,
} from "@bookclerk/plugin-sdk/workerd";

const PLUGIN_ID = "echo_workerd_ts";

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
  ],
};

/** `cli` entrypoint: `bookclerk plugins echo_workerd_ts ping --message hi`. */
export class Cli extends CliEntrypoint {
  async describe() {
    return CLI;
  }

  /**
   * @param {{ command: string, args: Array<{ name: string, value: string }> }} params
   */
  async invoke(params) {
    if (params?.command !== "ping") {
      return {
        exitCode: 2,
        stdout: "",
        stderr: `unknown command ${params?.command ?? ""}`,
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
export default class EchoPlugin extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Echo Integration (workerd TypeScript)" };
  }

  /**
   * @param {import("@bookclerk/plugin-sdk/workerd").EventBatch} batch
   */
  async event(batch) {
    for (const msg of batch.messages) {
      switch (msg.type) {
        case "book_acquired": {
          const titleId = msg.json()?.titleId ?? "";
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
