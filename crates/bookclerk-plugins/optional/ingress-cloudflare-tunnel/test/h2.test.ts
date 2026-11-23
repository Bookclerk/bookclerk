/**
 * HTTP/2 framing (RFC 9113): the reader's preface and shape checks, header
 * blocks split across CONTINUATION frames, and the encoders.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  ErrorCode,
  Flag,
  FrameReader,
  FrameType,
  H2Error,
  MAX_HEADER_BLOCK,
  PREFACE,
  Setting,
  encodeData,
  encodeFrame,
  encodeGoAway,
  encodeHeaders,
  encodePing,
  encodeRst,
  encodeSettings,
  encodeSettingsAck,
  encodeWindowUpdate,
  join,
} from "../src/h2.ts";

/**
 * A reader past the preface and the edge's first SETTINGS.
 *
 * @returns The reader.
 */
function opened(): FrameReader {
  const reader = new FrameReader();
  assert.deepEqual(reader.push(join(PREFACE, encodeSettings([]))), [
    { kind: "settings", ack: false, params: [] },
  ]);
  return reader;
}

test("the preface must come first, byte by byte, then SETTINGS", () => {
  const reader = new FrameReader();
  assert.deepEqual(reader.push(PREFACE.subarray(0, 5)), []);
  assert.equal(reader.buffered, 5);
  assert.deepEqual(reader.push(PREFACE.subarray(5)), []);
  assert.throws(() => reader.push(encodePing(new Uint8Array(8), false)), /must end with SETTINGS/);
  assert.throws(
    () => new FrameReader().push(new TextEncoder().encode("GET / HTTP/1.1")),
    /preface/,
  );
  assert.throws(
    () => new FrameReader().push(join(PREFACE, encodeSettingsAck())),
    /must end with SETTINGS/,
  );
});

test("frames read whole across chunk boundaries", () => {
  const reader = opened();
  const bytes = join(
    encodeData(1, new Uint8Array([1, 2, 3]), true),
    encodeRst(3, ErrorCode.Cancel),
    encodeSettings([[Setting.MaxFrameSize, 20_000]]),
    encodeSettingsAck(),
    encodePing(new Uint8Array(8).fill(9), true),
    encodeGoAway(7, ErrorCode.NoError, "bye"),
    encodeWindowUpdate(0, 100),
    encodeFrame(0x42, 0, 0, new Uint8Array([1])),
    encodeFrame(FrameType.Priority, 0, 5, new Uint8Array(5)),
  );
  const frames = [];
  for (let i = 0; i < bytes.length; i += 7) frames.push(...reader.push(bytes.subarray(i, i + 7)));
  assert.deepEqual(frames, [
    { kind: "data", stream: 1, data: new Uint8Array([1, 2, 3]), endStream: true, flow: 3 },
    { kind: "rst", stream: 3, code: ErrorCode.Cancel },
    { kind: "settings", ack: false, params: [[Setting.MaxFrameSize, 20_000]] },
    { kind: "settings", ack: true, params: [] },
    { kind: "ping", ack: true, data: new Uint8Array(8).fill(9) },
    { kind: "goaway", lastStream: 7, code: 0, debug: "bye" },
    { kind: "window", stream: 0, increment: 100 },
  ]);
  assert.equal(reader.buffered, 0);
});

test("padding is stripped and counted for flow control; priority is skipped", () => {
  const reader = opened();
  const padded = encodeFrame(FrameType.Data, Flag.Padded, 1, new Uint8Array([2, 7, 0, 0]));
  assert.deepEqual(reader.push(padded), [
    { kind: "data", stream: 1, data: new Uint8Array([7]), endStream: false, flow: 4 },
  ]);
  const headers = encodeFrame(
    FrameType.Headers,
    Flag.EndHeaders | Flag.Priority | Flag.Padded | Flag.EndStream,
    3,
    new Uint8Array([1, 0, 0, 0, 1, 16, 0x82, 0]),
  );
  assert.deepEqual(reader.push(headers), [
    { kind: "headers", stream: 3, block: new Uint8Array([0x82]), endStream: true },
  ]);
});

test("a header block joins its CONTINUATION frames, and nothing may interrupt it", () => {
  const block = new Uint8Array(40_000).fill(0x82);
  const frames = encodeHeaders(5, block, true, 16_384);
  const reader = opened();
  const read = reader.push(frames);
  assert.equal(read.length, 1);
  assert.deepEqual(read[0], { kind: "headers", stream: 5, block, endStream: true });

  const open = opened();
  open.push(encodeFrame(FrameType.Headers, 0, 5, new Uint8Array([0x82])));
  assert.ok(open.buffered > 0);
  assert.throws(() => open.push(encodeData(5, new Uint8Array())), /interrupted/);
  const wrong = opened();
  wrong.push(encodeFrame(FrameType.Headers, 0, 5, new Uint8Array([0x82])));
  assert.throws(
    () => wrong.push(encodeFrame(FrameType.Continuation, Flag.EndHeaders, 7)),
    /CONTINUATION without/,
  );
  const big = opened();
  big.push(encodeFrame(FrameType.Headers, 0, 5, new Uint8Array(16_384)));
  assert.throws(() => {
    for (let i = 0; i * 16_384 <= MAX_HEADER_BLOCK; i++) {
      big.push(encodeFrame(FrameType.Continuation, 0, 5, new Uint8Array(16_384)));
    }
  }, /too large/);
});

test("frames that break RFC 9113 are connection errors with the right code", () => {
  const cases: Array<[Uint8Array, number, RegExp]> = [
    [encodeFrame(FrameType.Data, 0, 0), ErrorCode.ProtocolError, /DATA on stream 0/],
    [encodeFrame(FrameType.Data, Flag.Padded, 1), ErrorCode.FrameSizeError, /too short/],
    [
      encodeFrame(FrameType.Data, Flag.Padded, 1, new Uint8Array([4])),
      ErrorCode.ProtocolError,
      /padding/,
    ],
    [encodeFrame(FrameType.Headers, 4, 0), ErrorCode.ProtocolError, /HEADERS on stream 0/],
    [
      encodeFrame(FrameType.Headers, 0x24, 1, new Uint8Array(2)),
      ErrorCode.FrameSizeError,
      /priority/,
    ],
    [
      encodeFrame(FrameType.Headers, 0x24, 1, new Uint8Array([0, 0, 0, 1, 0])),
      ErrorCode.ProtocolError,
      /itself/,
    ],
    [encodeFrame(FrameType.Continuation, 4, 1), ErrorCode.ProtocolError, /CONTINUATION/],
    [encodeFrame(FrameType.Priority, 0, 0, new Uint8Array(5)), ErrorCode.ProtocolError, /PRIORITY/],
    [
      encodeFrame(FrameType.Priority, 0, 1, new Uint8Array(4)),
      ErrorCode.FrameSizeError,
      /PRIORITY/,
    ],
    [
      encodeFrame(FrameType.RstStream, 0, 0, new Uint8Array(4)),
      ErrorCode.ProtocolError,
      /RST_STREAM/,
    ],
    [
      encodeFrame(FrameType.RstStream, 0, 1, new Uint8Array(3)),
      ErrorCode.FrameSizeError,
      /RST_STREAM/,
    ],
    [encodeFrame(FrameType.Settings, 0, 1), ErrorCode.ProtocolError, /SETTINGS on a stream/],
    [
      encodeFrame(FrameType.Settings, 1, 0, new Uint8Array(6)),
      ErrorCode.FrameSizeError,
      /ack size/,
    ],
    [
      encodeFrame(FrameType.Settings, 0, 0, new Uint8Array(5)),
      ErrorCode.FrameSizeError,
      /SETTINGS size/,
    ],
    [encodeSettings([[Setting.EnablePush, 2]]), ErrorCode.ProtocolError, /ENABLE_PUSH/],
    [encodeSettings([[Setting.InitialWindowSize, 2 ** 31]]), ErrorCode.FlowControlError, /WINDOW/],
    [encodeSettings([[Setting.MaxFrameSize, 100]]), ErrorCode.ProtocolError, /MAX_FRAME_SIZE/],
    [encodeSettings([[Setting.MaxFrameSize, 2 ** 24]]), ErrorCode.ProtocolError, /MAX_FRAME_SIZE/],
    [
      encodeFrame(FrameType.PushPromise, 0, 1, new Uint8Array(4)),
      ErrorCode.ProtocolError,
      /PUSH_PROMISE/,
    ],
    [
      encodeFrame(FrameType.Ping, 0, 1, new Uint8Array(8)),
      ErrorCode.ProtocolError,
      /PING on a stream/,
    ],
    [encodeFrame(FrameType.Ping, 0, 0, new Uint8Array(7)), ErrorCode.FrameSizeError, /PING size/],
    [encodeFrame(FrameType.GoAway, 0, 1, new Uint8Array(8)), ErrorCode.ProtocolError, /GOAWAY on/],
    [
      encodeFrame(FrameType.GoAway, 0, 0, new Uint8Array(7)),
      ErrorCode.FrameSizeError,
      /GOAWAY size/,
    ],
    [
      encodeFrame(FrameType.WindowUpdate, 0, 0, new Uint8Array(3)),
      ErrorCode.FrameSizeError,
      /WINDOW_UPDATE/,
    ],
    [
      encodeFrame(FrameType.WindowUpdate, 0, 0, new Uint8Array(4)),
      ErrorCode.ProtocolError,
      /increment of 0/,
    ],
  ];
  for (const [frame, code, message] of cases) {
    assert.throws(
      () => opened().push(frame),
      (err: unknown) => {
        assert.ok(err instanceof H2Error, String(message));
        assert.equal(err.code, code, String(message));
        assert.match(err.message, message);
        return true;
      },
    );
  }
  // A frame past this side's maximum size is refused from its header alone.
  const header = new Uint8Array([0x00, 0x40, 0x01, 0, 0, 0, 0, 0, 1]);
  assert.throws(() => opened().push(header), /frame of 16385 bytes/);
});

test("the encoders write the frame header and split header blocks", () => {
  assert.deepEqual(
    encodeWindowUpdate(3, 0x7fffffff),
    new Uint8Array([0, 0, 4, 8, 0, 0, 0, 0, 3, 0x7f, 0xff, 0xff, 0xff]),
  );
  assert.deepEqual(encodeRst(1, 8), new Uint8Array([0, 0, 4, 3, 0, 0, 0, 0, 1, 0, 0, 0, 8]));
  const goaway = encodeGoAway(5, 1, "x".repeat(300));
  // The debug data is cut to 200 bytes.
  assert.equal(goaway.length, 9 + 8 + 200);
  const split = encodeHeaders(1, new Uint8Array(20), false, 16);
  // HEADERS without END_HEADERS, then CONTINUATION with it.
  assert.equal(split[3], FrameType.Headers);
  assert.equal(split[4], 0);
  assert.equal(split[9 + 16 + 3], FrameType.Continuation);
  assert.equal(split[9 + 16 + 4], Flag.EndHeaders);
  assert.deepEqual(join(), new Uint8Array());
});
