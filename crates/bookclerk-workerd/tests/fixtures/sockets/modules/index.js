/**
 * Contract fixture: `fetch()` and `cloudflare:sockets` `connect()` through
 * Bookclerk's egress worker (`globalOutbound`).
 *
 * Keys:
 * - `tcp:<host>:<port>` — CONNECT, write `ping\\n`, return the echo bytes
 * - `fetch:<url>` — isolate `fetch`, return `STATUS\\n<body>`
 */

import { connect } from "cloudflare:sockets";
import {
  BookclerkEntrypoint,
  StorageEntrypoint,
  PluginError,
} from "@bookclerk/plugin-sdk/workerd";

function bytesStream(buf) {
  return new ReadableStream({
    start(controller) {
      if (buf.byteLength > 0) controller.enqueue(buf);
      controller.close();
    },
  });
}

function toBytes(text) {
  return new TextEncoder().encode(text);
}

export class Storage extends StorageEntrypoint {
  async head(_key) {
    return null;
  }

  async list() {
    return { objects: [] };
  }

  async get(key) {
    if (key.startsWith("tcp:")) {
      const rest = key.slice(4);
      const idx = rest.lastIndexOf(":");
      const hostname = rest.slice(0, idx);
      const port = Number(rest.slice(idx + 1));
      try {
        const socket = connect(
          { hostname, port },
          { secureTransport: "off", allowHalfOpen: true },
        );
        await socket.opened;
        const writer = socket.writable.getWriter();
        await writer.write(toBytes("ping\n"));
        await writer.close();
        const reader = socket.readable.getReader();
        const { value } = await reader.read();
        try {
          await socket.close();
        } catch {
          /* already closed */
        }
        const body = value || new Uint8Array();
        return { meta: { key, size: body.byteLength }, body: bytesStream(body) };
      } catch (err) {
        throw PluginError.fromWire("failed_precondition", String(err && err.message ? err.message : err));
      }
    }
    if (key.startsWith("fetch:")) {
      const url = key.slice(6);
      try {
        const res = await fetch(url);
        const text = await res.text();
        if (!res.ok) {
          throw PluginError.fromWire(
            "failed_precondition",
            `${res.status} ${text}`,
          );
        }
        const body = toBytes(`${res.status}\n${text}`);
        return { meta: { key, size: body.byteLength }, body: bytesStream(body) };
      } catch (err) {
        if (err && err.wireCode) throw err;
        throw PluginError.fromWire("failed_precondition", String(err && err.message ? err.message : err));
      }
    }
    throw Object.assign(new Error(`not found: ${key}`), { code: "not_found" });
  }

  async put() {
    throw PluginError.fromWire("invalid_params", "read-only fixture");
  }

  async copy() {
    throw PluginError.fromWire("invalid_params", "read-only fixture");
  }

  async delete() {}

  async commit(_key, commitToken) {
    return { key: _key, bytesWritten: 0, etag: commitToken };
  }

  async abortStage() {}
}

export default class SocketsPlugin extends BookclerkEntrypoint {
  async describe() {
    return { displayName: "Sockets gateway fixture" };
  }
}
