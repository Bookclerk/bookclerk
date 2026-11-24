"""The SDK's ``connect()`` through the testkit's fake relay, and the frame codec.

The TypeScript SDK runs the same cases (``scripts/test-tcp.mjs``); the frame
layout is the one ``bookclerk-workerd::egress_starttls`` speaks.
"""

from __future__ import annotations

import random
import unittest

from bookclerk_plugin_sdk.tcp import (
    MAX_TCP_FRAME_PAYLOAD,
    STARTTLS_HOST_SUFFIX,
    RelayFrame,
    RelayFrameDecoder,
    connect,
    encode_data_frames,
    encode_start_tls_frame,
    parse_tcp_address,
)
from bookclerk_plugin_sdk.testkit import TestConnect, TestTcpConnection


async def read_until(sock, suffix: str) -> str:
    out = ""
    while not out.endswith(suffix):
        chunk = await sock.read()
        if chunk is None:
            raise AssertionError(f"closed after {out!r}")
        out += chunk.decode()
    return out


def smtp(case: unittest.TestCase):
    async def serve(conn: TestTcpConnection) -> None:
        await conn.write("220 mail ready\r\n")
        case.assertEqual(await conn.read_line(), "EHLO test\r\n")
        await conn.write("250-mail\r\n250 STARTTLS\r\n")
        case.assertEqual(await conn.read_line(), "STARTTLS\r\n")
        case.assertFalse(conn.tls)
        await conn.write("220 go ahead\r\n")
        await conn.wait_for_tls()
        if not conn.tls:
            return
        case.assertEqual(await conn.read_line(), "EHLO test\r\n")
        await conn.write("250 secure\r\n")

    return serve


async def to_go_ahead(sock) -> None:
    await read_until(sock, "\r\n")
    await sock.write(b"EHLO test\r\n")
    await read_until(sock, "STARTTLS\r\n")
    await sock.write(b"STARTTLS\r\n")
    assert await read_until(sock, "\r\n") == "220 go ahead\r\n"


class ConnectTest(unittest.IsolatedAsyncioTestCase):
    async def test_starttls_upgrades_and_carries_plaintext(self) -> None:
        fake = TestConnect(smtp(self))
        plain = await fake.connect(("mail.example.com", 587), secure_transport="starttls")
        self.assertEqual(await read_until(plain, "\r\n"), "220 mail ready\r\n")
        await plain.write(b"EHLO test\r\n")
        self.assertIn("250 STARTTLS", await read_until(plain, "STARTTLS\r\n"))
        await plain.write(b"STARTTLS\r\n")
        self.assertEqual(await read_until(plain, "\r\n"), "220 go ahead\r\n")
        secure = await plain.start_tls()
        self.assertTrue(secure.tls)
        await secure.write(b"EHLO test\r\n")
        self.assertEqual(await read_until(secure, "\r\n"), "250 secure\r\n")
        self.assertEqual(
            [(d.hostname, d.port, d.tunnel) for d in fake.dials],
            [("mail.example.com", 587, "framed")],
        )
        with self.assertRaises(RuntimeError):
            await plain.start_tls()
        with self.assertRaises(RuntimeError):
            await secure.start_tls()
        with self.assertRaises(RuntimeError):
            await plain.write(b"x")
        await secure.close()

    async def test_on_upgrades_before_the_first_byte(self) -> None:
        async def serve(conn: TestTcpConnection) -> None:
            await conn.wait_for_tls()
            self.assertTrue(conn.tls)
            self.assertEqual(await conn.read_line(), "PING\n")
            await conn.write("PONG\n")

        fake = TestConnect(serve)
        sock = await fake.connect("db.example.com:5432", secure_transport="on")
        self.assertTrue(sock.tls)
        await sock.write(b"PING\n")
        self.assertEqual(await read_until(sock, "\n"), "PONG\n")
        with self.assertRaises(RuntimeError):
            await sock.start_tls()

    async def test_a_failed_handshake_raises_with_the_reason(self) -> None:
        fake = TestConnect(
            smtp(self), tls_failure=lambda _host: "invalid peer certificate: NotValidForName"
        )
        plain = await fake.connect(("mail.example.com", 587), secure_transport="starttls")
        await to_go_ahead(plain)
        with self.assertRaisesRegex(
            ConnectionError, "TLS handshake failed: invalid peer certificate"
        ):
            await plain.start_tls()

    async def test_a_refused_grant_raises_network_not_granted(self) -> None:
        fake = TestConnect(smtp(self), allows=lambda _h, _p: False)
        with self.assertRaisesRegex(ConnectionError, "network_not_granted"):
            await fake.connect(("mail.example.com", 25), secure_transport="starttls")

    async def test_a_plain_socket_is_not_framed(self) -> None:
        async def serve(conn: TestTcpConnection) -> None:
            got = await conn.read()
            assert got is not None
            await conn.write(got)

        fake = TestConnect(serve)
        sock = await connect(("echo.example.com", 7), dial=fake.dial)
        await sock.write(b"raw")
        self.assertEqual(await read_until(sock, "raw"), "raw")
        self.assertEqual(fake.dials[0].tunnel, "off")
        with self.assertRaises(RuntimeError):
            await sock.start_tls()
        self.assertIsNone(await sock.read())

    async def test_large_writes_split_into_bounded_frames(self) -> None:
        big = bytes(i % 251 for i in range(MAX_TCP_FRAME_PAYLOAD * 3 + 17))
        received = [0]

        async def serve(conn: TestTcpConnection) -> None:
            while received[0] < len(big):
                chunk = await conn.read()
                assert chunk is not None and len(chunk) <= MAX_TCP_FRAME_PAYLOAD
                received[0] += len(chunk)
            await conn.write(big)

        fake = TestConnect(serve)
        sock = await fake.connect(("bulk.example.com", 9000), secure_transport="starttls")
        await sock.write(big)
        back = bytearray()
        while len(back) < len(big):
            chunk = await sock.read()
            assert chunk is not None
            back += chunk
        self.assertEqual(received[0], len(big))
        self.assertEqual(bytes(back), big)

    async def test_bad_addresses_and_ipv6_tls_targets_are_refused_before_dialing(self) -> None:
        dials: list[tuple[str, int]] = []

        async def dial(host: str, port: int):
            dials.append((host, port))
            raise AssertionError("unreachable")

        for address, mode in [
            ("nohost", "off"),
            (("a", 0), "off"),
            (("a", 70000), "off"),
            (("::1", 443), "on"),
            ((f"x{STARTTLS_HOST_SUFFIX}", 1), "on"),
            (("a", 1), "maybe"),
        ]:
            with self.assertRaises(ValueError, msg=repr(address)):
                await connect(address, secure_transport=mode, dial=dial)  # type: ignore[arg-type]
        self.assertEqual(dials, [])
        self.assertEqual(parse_tcp_address("[::1]:25"), ("::1", 25))


class FrameCodecTest(unittest.TestCase):
    def test_the_codec_matches_the_relay_layout(self) -> None:
        self.assertEqual(list(encode_start_tls_frame()), [1, 0, 0])
        self.assertEqual(list(encode_data_frames(b"hi")), [0, 0, 2, 104, 105])
        self.assertEqual(encode_data_frames(b""), b"")
        d = RelayFrameDecoder()
        d.push(bytes([0, 0, 2, 104]))
        self.assertIsNone(d.next())
        d.push(bytes([105, 1, 0, 0, 2, 0, 2, 110, 111]))
        self.assertEqual(d.next(), RelayFrame("data", b"hi"))
        self.assertEqual(d.next(), RelayFrame("tls_ready"))
        self.assertEqual(d.next(), RelayFrame("tls_failed", b"no"))
        self.assertIsNone(d.next())
        for bad in ([0, 0, 0], [0, 0x40, 0x01], [1, 0, 1], [2, 0x02, 0x01], [3, 0, 0]):
            decoder = RelayFrameDecoder()
            decoder.push(bytes(bad))
            with self.assertRaises(ValueError, msg=repr(bad)):
                decoder.next()

    def test_random_streams_decode_like_an_independent_reading(self) -> None:
        rng = random.Random(7)
        for _ in range(3000):
            data = bytes(
                rng.randrange(256) if rng.randrange(4) == 0 else rng.choice([0, 1, 2, 0, 0, 3])
                for _ in range(rng.randrange(40))
            )
            expected: list[tuple[int, bytes]] = []
            error = False
            at = 0
            while len(data) - at >= 3:
                kind = data[at]
                length = (data[at + 1] << 8) | data[at + 2]
                ok = (
                    (kind == 0 and 1 <= length <= 16384)
                    or (kind == 1 and length == 0)
                    or (kind == 2 and length <= 512)
                )
                if not ok:
                    error = True
                    break
                if len(data) - at - 3 < length:
                    break
                expected.append((kind, data[at + 3 : at + 3 + length]))
                at += 3 + length
            decoder = RelayFrameDecoder()
            got: list[tuple[int, bytes]] = []
            threw = False
            step = 1 + rng.randrange(5)
            kinds = {"data": 0, "tls_ready": 1, "tls_failed": 2}
            for i in range(0, len(data), step):
                decoder.push(data[i : i + step])
                try:
                    while (frame := decoder.next()) is not None:
                        got.append((kinds[frame.kind], frame.payload))
                except ValueError:
                    threw = True
                    break
            self.assertEqual(threw, error, repr(data))
            self.assertEqual(got, expected, repr(data))


if __name__ == "__main__":
    unittest.main()
