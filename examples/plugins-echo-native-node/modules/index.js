/**
 * Echo workerd guest (id `echo_native_node`, api_version = 3).
 *
 * This example validates workerd hosting, not a Node Cap'n Proto stack.
 * Pattern matches `plugins-echo-workerd-ts`: the default export extends
 * `BookclerkEntrypoint` (event trigger) and `Cli` is the `cli` entrypoint.
 */

import {
  BookclerkEntrypoint,
  CliEntrypoint,
  cliArgs,
  jsonPayload,
} from "@bookclerk/plugin-sdk/workerd";

const PLUGIN_ID = "echo_native_node";

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

/** `cli` entrypoint: `bookclerk plugins echo_native_node ping --message hi`. */
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
    return { displayName: "Echo Integration (native Node)" };
  }

  /**
   * @param {import("@bookclerk/plugin-sdk/workerd").EventBatch} batch
   */
  async event(batch) {
    for (const msg of batch.messages) {
      if (msg.type === "book_acquired") {
        console.log(`${PLUGIN_ID} saw book_acquired`);
      }
      msg.ack();
    }
  }
}
