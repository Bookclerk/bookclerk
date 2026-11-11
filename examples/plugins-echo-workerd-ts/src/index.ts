/**
 * Echo workerd guest. `npm run build` emits the shipped `modules/` from
 * `src/`.
 *
 * Default export extends `BookclerkEntrypoint`, handles the `book_acquired`
 * event trigger and registers the `DB` binding's migrations; the `cli`
 * entrypoint is the exported `Cli` class, whose `notes-*` commands use the
 * database helpers in `notes.ts`. Run `npx bookclerk-plugin types .` to
 * regenerate `bookclerk-configuration.d.ts`.
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
  type PluginMigration,
  type QueueBatch,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import {
  MIGRATIONS,
  NOTES_BINDING,
  addNotes,
  countNotes,
  exceededLimit,
  listNotes,
} from "./notes.ts";

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
    {
      name: "notes-add",
      about: "Store a note COUNT times in the plugin's own database",
      args: [
        {
          name: "body",
          long: "body",
          kind: "string",
          required: true,
          positional: false,
        },
        {
          name: "count",
          long: "count",
          kind: "int",
          required: false,
          positional: false,
          default: "1",
        },
      ],
    },
    {
      name: "notes-list",
      about: "List the oldest LIMIT stored notes",
      args: [
        {
          name: "limit",
          long: "limit",
          kind: "int",
          required: false,
          positional: false,
          default: "100",
        },
      ],
    },
  ],
};

/** Largest `--count` `notes-add` accepts, so one command stays a modest write. */
const MAX_NOTES_PER_ADD = 10_000;

/**
 * Largest `--limit` `notes-list` accepts: the listing goes back in one CLI
 * reply, so it is bounded even though `listNotes` itself streams.
 */
const MAX_NOTES_PER_LIST = 1_000;

/**
 * Parses a whole-number CLI argument within `1..=max`.
 *
 * @param value - The argument as given.
 * @param max - Largest value accepted.
 * @returns The number, or `undefined` when `value` is not plain decimal
 *   digits in range.
 */
function wholeNumber(value: string, max: number): number | undefined {
  if (!/^[0-9]{1,6}$/.test(value)) return undefined;
  const n = Number(value);
  return n >= 1 && n <= max ? n : undefined;
}

/**
 * A CLI reply.
 *
 * @param exitCode - Process exit status (`0` success, `1` failure, `2` usage).
 * @param stdout - Text for standard output.
 * @param stderr - Text for standard error.
 * @param payload - Structured result for callers that read JSON.
 * @returns The reply.
 */
function reply(
  exitCode: number,
  stdout: string,
  stderr: string,
  payload: unknown,
): CliInvokeResult {
  return { exitCode, stdout, stderr, payload: jsonPayload(payload) };
}

/** `cli` entrypoint: `bookclerk plugins echo_workerd_ts ping --message hi`. */
export class Cli extends CliEntrypoint<Env> {
  override async describe(): Promise<CliSchema> {
    return CLI;
  }

  override async invoke(params: CliInvokeParams): Promise<CliInvokeResult> {
    const args = cliArgs(params);
    switch (params.command) {
      case "ping": {
        const message = args["message"] ?? "hi";
        return reply(0, `pong: ${message}\n`, "", { pong: message });
      }
      case "notes-add": {
        const count = wholeNumber(args["count"] ?? "1", MAX_NOTES_PER_ADD);
        if (count === undefined) {
          return reply(2, "", `--count must be 1 to ${MAX_NOTES_PER_ADD}\n`, null);
        }
        const body = args["body"] ?? "";
        let committed = 0;
        try {
          const stored = await addNotes(
            this.env.DB,
            Array.from({ length: count }, () => body),
            (n) => (committed = n),
          );
          const total = await countNotes(this.env.DB);
          return reply(0, `stored ${stored}; ${total} in total\n`, "", { stored, total });
        } catch (err) {
          return databaseFailure(err, committed);
        }
      }
      case "notes-list": {
        const limit = wholeNumber(args["limit"] ?? "100", MAX_NOTES_PER_LIST);
        if (limit === undefined) {
          return reply(2, "", `--limit must be 1 to ${MAX_NOTES_PER_LIST}\n`, null);
        }
        const lines: string[] = [];
        try {
          for await (const note of listNotes(this.env.DB)) {
            lines.push(`${note.id}: ${note.body}`);
            if (lines.length === limit) break;
          }
        } catch (err) {
          return databaseFailure(err, 0);
        }
        return reply(0, lines.map((line) => `${line}\n`).join(""), "", { count: lines.length });
      }
      default:
        return reply(2, "", `unknown command ${params.command}\n`, null);
    }
  }
}

/**
 * A CLI reply for a failed database call: a typed limit names the cap, any
 * other error is passed on so the host reports it.
 *
 * @param err - What the binding threw.
 * @param committed - Notes already stored by batches that committed before
 *   the failure; they stay stored.
 * @returns Exit status 1 with the exceeded cap.
 * @throws `err` when it is not a `limit_exceeded` error and nothing was
 *   stored; otherwise an Error that says how many notes were.
 */
function databaseFailure(err: unknown, committed: number): CliInvokeResult {
  const limit = exceededLimit(err);
  const kept = committed > 0 ? `${committed} notes were stored before the failure` : "";
  if (limit === undefined) {
    if (committed === 0) throw err;
    throw new Error(`${kept}: ${String(err)}`, { cause: err });
  }
  return reply(
    1,
    "",
    `the database refused the request: ${limit.limit} is ${limit.cap}, it needed ${limit.used}\n` +
      (kept === "" ? "" : `${kept}\n`),
    { limit, stored: committed },
  );
}

/** Default entrypoint: `event` and `queue` triggers for `[[events.consumers]]` and `[[queues.consumers]]`. */
export default class EchoPlugin extends BookclerkEntrypoint<Env> {
  override async describe(): Promise<PluginDescribe> {
    return { displayName: "Echo Integration (workerd TypeScript)" };
  }

  /**
   * The schema of each `[[databases]]` binding, applied by the host before
   * the binding is granted.
   *
   * @param binding - Binding name from `plugin.toml`.
   * @returns The binding's migrations, oldest first.
   */
  override databaseMigrations(binding: string): PluginMigration[] {
    return binding === NOTES_BINDING ? MIGRATIONS : [];
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

  override async queue(batch: QueueBatch): Promise<void> {
    for (const msg of batch.messages) {
      if (msg.contentType === "text" && msg.text() === "retry") {
        msg.retry({ delaySeconds: 1, reason: "echo retry" });
        continue;
      }
      console.log(`${PLUGIN_ID} saw queue ${batch.queue} message ${msg.id}`);
      // Echo the body onward when the host granted the producer binding.
      await this.env.ECHO_OUT?.send(msg.body, { contentType: "bytes" });
      msg.ack();
    }
  }
}
