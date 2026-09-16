/**
 * Contract fixture: `Storage` entrypoint streams + `job(controller)` copy.
 *
 * `Storage` is the named entrypoint the host calls (`plugin.toml`
 * `entrypoints = ["storage"]`); object bytes live on module-scoped
 * `sharedStore` so they survive per-invocation instances.
 *
 * Large objects are generated/consumed lazily. `count:` puts discard bytes;
 * `pattern:<n>` gets emit a repeating counter stream of `n` bytes.
 */

import {
  BookclerkEntrypoint,
  StorageEntrypoint,
  PluginError,
} from "@bookclerk/plugin-sdk/workerd";

function patternStream(size) {
  let pos = 0;
  return new ReadableStream({
    pull(controller) {
      if (pos >= size) {
        controller.close();
        return;
      }
      const n = Math.min(64 * 1024, size - pos);
      const buf = new Uint8Array(n);
      for (let i = 0; i < n; i++) {
        buf[i] = (pos + i) % 251;
      }
      pos += n;
      controller.enqueue(buf);
    },
  });
}

function bytesStream(buf) {
  return new ReadableStream({
    start(controller) {
      if (buf.byteLength > 0) controller.enqueue(buf);
      controller.close();
    },
  });
}

const sharedStore = new Map();

export class Storage extends StorageEntrypoint {
  async head(key) {
    if (key.startsWith("internal-msg:")) {
      throw PluginError.fromWire("internal", "object not_found in cache");
    }
    if (key.startsWith("unknown-code:")) {
      throw PluginError.fromWire("future_retry_policy", "try later");
    }
    if (sharedStore.has(key)) {
      const buf = sharedStore.get(key);
      return { key, size: buf.byteLength };
    }
    if (key.startsWith("pattern:")) {
      return { key, size: Number(key.slice("pattern:".length)) || 0 };
    }
    return null;
  }

  async list(options) {
    const prefix = options?.prefix || "";
    if (prefix.startsWith("overflow")) {
      const objects = [];
      for (let i = 0; i < 300; i++) {
        objects.push({ key: `o${i}`, size: 1 });
      }
      return { objects };
    }
    const objects = [];
    for (const [key, buf] of sharedStore) {
      if (key.startsWith(prefix)) {
        objects.push({ key, size: buf.byteLength });
      }
    }
    objects.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
    return { objects };
  }

  async get(key) {
    if (sharedStore.has(key)) {
      const buf = sharedStore.get(key);
      return { meta: { key, size: buf.byteLength }, body: bytesStream(buf) };
    }
    if (key.startsWith("internal-msg:")) {
      throw PluginError.fromWire("internal", "object not_found in cache");
    }
    if (key.startsWith("fail-mid:")) {
      const size = Number(key.slice("fail-mid:".length)) || 100;
      let pos = 0;
      return {
        meta: { key, size },
        body: new ReadableStream({
          pull(controller) {
            if (pos >= 16) {
              controller.error(
                Object.assign(new Error("source exploded"), { code: "internal" }),
              );
              return;
            }
            const n = Math.min(8, size - pos);
            const buf = new Uint8Array(n);
            pos += n;
            controller.enqueue(buf);
          },
        }),
      };
    }
    if (key.startsWith("pattern:")) {
      const size = Number(key.slice("pattern:".length)) || 0;
      return { meta: { key, size }, body: patternStream(size) };
    }
    throw Object.assign(new Error(`not found: ${key}`), { code: "not_found" });
  }

  async put(key, body, options) {
    const reader = body.getReader();
    const chunks = [];
    let n = 0;
    const keep = !key.startsWith("count:");
    const expected = options?.contentLength;
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      n += value.byteLength;
      if (keep) chunks.push(value);
    }
    if (expected != null && Number.isFinite(Number(expected)) && n !== Number(expected)) {
      throw PluginError.fromWire("invalid_params", `content-length ${expected} got ${n}`);
    }
    if (keep) {
      const buf = new Uint8Array(n);
      let off = 0;
      for (const c of chunks) {
        buf.set(c, off);
        off += c.byteLength;
      }
      sharedStore.set(key, buf);
    }
    return { key, bytesWritten: n };
  }

  async copy(from, to) {
    const buf = sharedStore.get(from);
    if (!buf) {
      throw Object.assign(new Error(`not found: ${from}`), { code: "not_found" });
    }
    sharedStore.set(to, buf.slice());
    return { bytesCopied: buf.byteLength };
  }

  async delete(key) {
    sharedStore.delete(key);
  }

  async commit(_key, commitToken) {
    return { key: _key, bytesWritten: 0, etag: commitToken };
  }

  async abortStage(_key, _commitToken) {}
}

export default class StreamPlugin extends BookclerkEntrypoint {
  async describe() {
    return {
      displayName: `Stream contract fixture env=${Object.keys(this.env || {}).sort().join(",")}`,
    };
  }

  async job(job) {
    const spec = job.json();
    if (spec.awaitCancel) {
      // Host cancellation reaches the author as the locally projected
      // `job.signal`; report it back as the `cancelled` outcome.
      await job.progress(0, "waiting for cancel");
      await new Promise((resolve, reject) => {
        if (job.signal.aborted) return resolve();
        job.signal.addEventListener("abort", () => resolve(), { once: true });
        setTimeout(() => reject(new Error("cancel never arrived")), 15000);
      });
      throw PluginError.fromWire("cancelled", "host cancelled the copy");
    }
    await job.progress(0, "opening");
    const opened = await job.input.open(spec.from);
    await job.progress(10, "copying");
    const put = await job.output.put(spec.to, opened.body, {
      contentType: opened.meta?.contentType,
      contentLength: opened.meta?.size || undefined,
    });
    await job.progress(100, "done");
    return {
      message: `copied ${spec.from} -> ${spec.to}`,
      bytesCopied: put.bytesWritten,
    };
  }
}
