/**
 * ABI v2 contract fixture: Destination streams + JobHandler copy.
 *
 * Role objects extend RpcTarget so they can cross the adapter isolate.
 * Object bytes live on module-scoped `sharedMem` so they survive dest drop.
 *
 * Large objects are generated/consumed lazily. `count:` puts discard bytes;
 * `pattern:<n>` gets emit a repeating counter stream of `n` bytes.
 */

import {
  BookclerkPlugin,
  Destination,
  Source,
  JobHandler,
  PluginError,
  PRODUCT_API_VERSION,
  FEATURE_SCALAR_LIMITS,
  FEATURE_STREAMS,
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

class MemDest extends Destination {
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

class MemSource extends Source {
  open(key) {
    return new MemDest().get(key);
  }
}

class CopyHandler extends JobHandler {
  async handle(invocation, context) {
    const spec = JSON.parse(invocation.payloadJson || invocation.json || "{}");
    await context.progress.report(0, "opening");
    const opened = await context.input.open(spec.from);
    await context.progress.report(10, "copying");
    const put = await context.output.put(spec.to, opened.body, {
      contentType: opened.meta?.contentType,
      contentLength: opened.meta?.size || undefined,
    });
    await context.progress.report(100, "done");
    return {
      kind: "completed",
      message: `copied ${spec.from} -> ${spec.to}`,
      bytesCopied: put.bytesWritten,
    };
  }
}

class StreamPlugin extends BookclerkPlugin {
  async describe() {
    return {
      apiVersion: PRODUCT_API_VERSION,
      id: "v2_stream",
      kind: "output",
      displayName: `ABI v2 stream contract env=${Object.keys(this.env || {}).sort().join(",")}`,
      rpcFeatures: [FEATURE_SCALAR_LIMITS, FEATURE_STREAMS],
      scalarLimits: {
        maxScalarBytes: 262144,
        maxStreamWindowBytes: 1048576,
        maxListPage: 256,
      },
      supportedRoles: ["destination", "source", "worker"],
    };
  }

  destination() {
    return new MemDest();
  }

  source() {
    return new MemSource();
  }

  worker() {
    return new CopyHandler();
  }
}

export default StreamPlugin;
