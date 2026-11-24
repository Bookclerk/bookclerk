/**
 * Tests the `stream_copy` job helper against fake job capabilities: the
 * commit token matches the Rust and Python SDKs, the copy stages then
 * commits, and every failure path aborts the stage.
 */
import assert from "node:assert/strict";
import { register } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

// `JobController` lives beside the workerd entrypoints, so stub
// `cloudflare:workers` the way test-entrypoints.mjs does.
const STUB = `
export class WorkerEntrypoint {
  constructor(ctx, env) { this.ctx = ctx; this.env = env; }
}
export class RpcTarget {}
`;
const hooks = `
export function resolve(specifier, context, next) {
  if (specifier === "cloudflare:workers") return { url: "stub:cloudflare-workers", shortCircuit: true };
  return next(specifier, context);
}
export function load(url, context, next) {
  if (url === "stub:cloudflare-workers") {
    return { format: "module", source: ${JSON.stringify(STUB)}, shortCircuit: true };
  }
  return next(url, context);
}
`;
register(`data:text/javascript,${encodeURIComponent(hooks)}`, pathToFileURL(root + "/"));

const { streamCopy, streamCopyCommitToken, STREAM_COPY_COMMAND } = await import(
  join(dist, "stream-copy.js")
);
const { JobController } = await import(join(dist, "plugin.js"));
const { PluginError } = await import(join(dist, "errors.js"));

/** Token vectors shared with the Rust (`jobs.rs`) and Python SDK tests. */
const VECTORS = [
  ["job/a:b", "", "out/book.m4b", "sc-d860fba581892fced82f4b4bba5c6d37"],
  ["dedup-stable", "step-2", "library/title.m4b", "sc-a8f88bc8ccfa06a5848f2b02b5d6abb2"],
];

for (const [key, step, dest, want] of VECTORS) {
  assert.equal(await streamCopyCommitToken(key, step, dest), want);
}
assert.equal(
  await streamCopyCommitToken("k", "", "d"),
  await streamCopyCommitToken("k", "stream_copy", "d"),
  "an empty step id is the default step",
);
assert.notEqual(
  await streamCopyCommitToken("k", "", "a"),
  await streamCopyCommitToken("k", "", "b"),
);

/**
 * A byte stream of `bytes`.
 *
 * @param {Uint8Array} bytes - The body.
 * @returns {ReadableStream<Uint8Array>} The stream.
 */
function streamOf(bytes) {
  return new ReadableStream({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
}

/**
 * Builds a controller over fake capabilities that record every call.
 *
 * @param {object} options - What the fakes do.
 * @returns {{controller: object, calls: string[], cancel: Function}} The
 *   controller, the call log and a function that cancels the job.
 */
function harness(options = {}) {
  const calls = [];
  let cancelNow;
  const cancelled = new Promise((resolve) => {
    cancelNow = resolve;
  });
  const input = {
    async open(key) {
      calls.push(`open ${key}`);
      return {
        meta: { key, size: 3, contentType: "audio/mp4", sha256: new Uint8Array(32) },
        body: streamOf(new Uint8Array([1, 2, 3])),
      };
    },
  };
  const output = {
    async put(key, body, opts) {
      calls.push(
        `put ${key} stage=${opts.stageOnly} token=${opts.commitToken} len=${opts.contentLength}`,
      );
      const reader = body.getReader();
      let n = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        n += value.length;
      }
      if (options.putFails) throw PluginError.fromWire("internal", "put failed");
      if (options.cancelDuringPut) {
        cancelNow();
        await new Promise((resolve) => setTimeout(resolve, 0));
      }
      return { key, bytesWritten: n };
    },
    async commit(key, token) {
      calls.push(`commit ${key} ${token}`);
      if (options.commitFails) throw PluginError.fromWire("conflict", "commit failed");
      return { key, bytesWritten: 3 };
    },
    async abortStage(key, token) {
      calls.push(`abort ${key} ${token}`);
    },
  };
  const progress = {
    async report(percent, message) {
      calls.push(`progress ${percent} ${message}`);
    },
  };
  const controller = new JobController(
    {
      invocationId: "inv-1",
      commandType: options.type ?? STREAM_COPY_COMMAND,
      payloadJson: options.payload ?? JSON.stringify({ from: "src/a", to: "dst/a" }),
      idempotencyKey: "job/a:b",
      stepId: "",
    },
    {
      input: options.noInput ? null : input,
      output,
      progress,
      cancel: { wait: () => cancelled },
    },
  );
  return { controller, calls, cancel: cancelNow };
}

const token = await streamCopyCommitToken("job/a:b", "", "dst/a");

// The happy path stages under the token, then commits.
{
  const { controller, calls } = harness();
  const done = await streamCopy(controller);
  assert.deepEqual(done, { message: "copied src/a -> dst/a", bytesCopied: 3 });
  assert.deepEqual(calls, [
    "progress 0 opening source",
    "open src/a",
    "progress 10 staging",
    `put dst/a stage=true token=${token} len=3`,
    "progress 90 committing",
    `commit dst/a ${token}`,
    "progress 100 done",
  ]);
}

/**
 * Asserts `run` rejects with a plugin error of `code`.
 *
 * @param {Promise<unknown>} run - The call.
 * @param {string} code - The expected code.
 */
async function rejectsWith(run, code) {
  await assert.rejects(run, (err) => err instanceof PluginError && err.code === code);
}

// Another command, a bad payload and a missing input are refused untouched.
{
  const other = harness({ type: "transcode" });
  await rejectsWith(streamCopy(other.controller), "unsupported");
  assert.deepEqual(other.calls, []);
  await rejectsWith(streamCopy(harness({ payload: "[]" }).controller), "invalid_params");
  await rejectsWith(streamCopy(harness({ payload: "{" }).controller), "invalid_params");
  await rejectsWith(
    streamCopy(harness({ payload: '{"from":1,"to":"x"}' }).controller),
    "invalid_params",
  );
  await rejectsWith(streamCopy(harness({ noInput: true }).controller), "unavailable");
}

// A failed put or commit aborts the stage and passes the error on.
{
  const put = harness({ putFails: true });
  await rejectsWith(streamCopy(put.controller), "internal");
  assert.equal(put.calls.at(-1), `abort dst/a ${token}`);
  const commit = harness({ commitFails: true });
  await rejectsWith(streamCopy(commit.controller), "conflict");
  assert.equal(commit.calls.at(-1), `abort dst/a ${token}`);
}

// Cancellation after the stage aborts it instead of committing.
{
  const { controller, calls } = harness({ cancelDuringPut: true });
  await rejectsWith(streamCopy(controller), "cancelled");
  assert.ok(!calls.some((call) => call.startsWith("commit")), calls.join("\n"));
  assert.equal(calls.at(-1), `abort dst/a ${token}`);
}

// A job cancelled before it starts touches nothing.
{
  const { controller, calls, cancel } = harness();
  cancel();
  await new Promise((resolve) => setTimeout(resolve, 0));
  await rejectsWith(streamCopy(controller), "cancelled");
  assert.deepEqual(calls, []);
}

console.log("stream-copy: ok");
