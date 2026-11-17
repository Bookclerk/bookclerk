/**
 * The Socket.io conversation with Audiobookshelf, frame by frame.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { onFrame, socketUrl } from "../src/push.ts";

test("the socket URL follows the base URL", () => {
  assert.equal(
    socketUrl("https://abs.example/"),
    "wss://abs.example/socket.io/?EIO=4&transport=websocket",
  );
  assert.equal(
    socketUrl("http://127.0.0.1:13378"),
    "ws://127.0.0.1:13378/socket.io/?EIO=4&transport=websocket",
  );
  assert.equal(
    socketUrl("https://home.example/abs"),
    "wss://home.example/abs/socket.io/?EIO=4&transport=websocket",
  );
  assert.equal(socketUrl("ftp://abs.example"), null);
  assert.equal(socketUrl("https://"), null);
  assert.equal(socketUrl(""), null);
});

test("the handshake joins, then authenticates", () => {
  assert.equal(onFrame('0{"sid":"x","pingInterval":25000}', "key").reply, "40");
  const joined = onFrame('40{"sid":"y"}', 'k"ey');
  assert.equal(joined.reply, '42["auth","k\\"ey"]');
  assert.equal(joined.close, false);
  assert.equal(joined.newUser, false);
  assert.deepEqual(onFrame('42["init",{"usersOnline":[]}]', "key"), {
    reply: null,
    close: false,
    newUser: false,
  });
});

test("a user_added event asks for a check", () => {
  const added = onFrame('42["user_added",{"id":"u1","username":"ada"}]', "key");
  assert.equal(added.newUser, true);
  assert.equal(added.reply, null);
  assert.equal(onFrame('421["user_added",{}]', "key").newUser, true, "an ack id");
  assert.equal(onFrame('42["user_online",{"id":"u1"}]', "key").newUser, false);
  assert.equal(onFrame('42["item_updated",{}]', "key").newUser, false);
});

test("a rejected key or a disconnect closes", () => {
  assert.equal(onFrame('42["invalid_token"]', "key").close, true);
  assert.equal(onFrame("41", "key").close, true);
  assert.equal(onFrame('44{"message":"no"}', "key").close, true);
  assert.equal(onFrame("1", "key").close, true);
});

test("noise is ignored", () => {
  for (const frame of ["", "3", "6", "42", "42[", "42{}", "42[1]", "4", "\u{1F600}", "40"]) {
    const reaction = onFrame(frame, "key");
    assert.equal(reaction.newUser || reaction.close, false, JSON.stringify(frame));
  }
});
