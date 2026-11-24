"""An in-memory ``connect()`` for plugin tests: a scripted server behind a fake relay.

The fake speaks the launcher egress relay's frames, so a plugin's
:meth:`~bookclerk_plugin_sdk.tcp.TcpSocket.start_tls` and
``secure_transport="on"`` code runs as it would on a host. The TypeScript
testkit's ``createTestConnect`` behaves the same way.

TLS itself is simulated: the fake answers the upgrade with TLS ready (or the
failure ``tls_failure`` names) and the server sees
:attr:`TestTcpConnection.tls` turn true. Certificate checks are the relay's
job and are covered by its own tests.

.. code-block:: python

    from bookclerk_plugin_sdk.testkit import TestConnect

    async def serve(conn):
        greeting = "220 ready"
        await conn.write(greeting + chr(13) + chr(10))

    fake = TestConnect(serve)
    sock = await fake.connect(("mail.example.com", 587), secure_transport="starttls")
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Literal

from .tcp import (
    MAX_TCP_FRAME_PAYLOAD,
    MAX_TLS_FAILURE_REASON_BYTES,
    STARTTLS_HOST_SUFFIX,
    RawSocket,
    SecureTransport,
    TcpSocket,
    connect,
    encode_data_frames,
)

__all__ = ["TestConnect", "TestDial", "TestTcpConnection"]


@dataclass(frozen=True)
class TestDial:
    """One connection the plugin opened.

    Attributes:
        hostname: Host without the framing suffix.
        port: Port the plugin dialed.
        tunnel: ``"off"`` for a plain socket, ``"framed"`` for one that may
            use TLS.
    """

    __test__ = False  # not a pytest test class

    hostname: str
    port: int
    tunnel: Literal["off", "framed"]


class TestTcpConnection:
    """The server's side of one test connection."""

    __test__ = False  # not a pytest test class

    def __init__(self, dial: TestDial, pair: _Pair) -> None:
        """Wraps the shared state of one dial.

        Args:
            dial: What the plugin dialed.
            pair: The state the fake relay and the server share.
        """
        self.hostname = dial.hostname
        """Host the plugin dialed, without the framing suffix."""
        self.port = dial.port
        """Port the plugin dialed."""
        self._pair = pair
        self._line_buf = bytearray()

    @property
    def tls(self) -> bool:
        """Whether the connection has upgraded to (simulated) TLS."""
        return self._pair.tls

    async def read(self) -> bytes | None:
        """The next plaintext bytes the plugin wrote.

        Returns:
            The bytes, or ``None`` once the plugin closed its side.
        """
        return await self._pair.inbox.get()

    async def read_line(self) -> str | None:
        """Reads up to and including the next newline, decoded as UTF-8.

        Returns:
            The line, or ``None`` when the plugin closed first.
        """
        while True:
            nl = self._line_buf.find(b"\n")
            if nl != -1:
                line = bytes(self._line_buf[: nl + 1])
                del self._line_buf[: nl + 1]
                return line.decode("utf-8")
            chunk = await self.read()
            if chunk is None:
                return None
            self._line_buf += chunk

    async def write(self, data: bytes | str) -> None:
        """Sends bytes (or UTF-8 text) to the plugin.

        Args:
            data: What to send.
        """
        self._pair.send(data.encode("utf-8") if isinstance(data, str) else bytes(data))

    async def wait_for_tls(self) -> None:
        """Waits until the plugin asks to upgrade and the upgrade is answered."""
        await self._pair.tls_settled.wait()

    async def close(self) -> None:
        """Ends the connection."""
        self._pair.end()


class _Inbox:
    """Chunks the plugin wrote, ended with ``None`` (which stays readable)."""

    def __init__(self) -> None:
        """An empty inbox."""
        self._queue: asyncio.Queue[bytes | None] = asyncio.Queue()
        self._ended = False

    def put(self, item: bytes | None) -> None:
        """Adds a chunk, or ``None`` to end the inbox (once).

        Args:
            item: The chunk, or ``None``.
        """
        if self._ended:
            return
        if item is None:
            self._ended = True
        self._queue.put_nowait(item)

    async def get(self) -> bytes | None:
        """The next chunk, or ``None`` once ended."""
        item = await self._queue.get()
        if item is None:
            self._queue.put_nowait(None)
        return item


class _Pair:
    """State shared by the fake relay (the plugin's raw socket) and the server."""

    def __init__(self, dial: TestDial, tls_failure: Callable[[str], str | None] | None) -> None:
        """Fresh state for one dial.

        Args:
            dial: What the plugin dialed.
            tls_failure: Names a handshake failure for a host, or ``None``.
        """
        self.dial = dial
        self.tls_failure = tls_failure
        self.inbox = _Inbox()
        self.to_plugin: asyncio.Queue[bytes | None] = asyncio.Queue()
        self.tls = False
        self.tls_settled = asyncio.Event()
        self.ended = False
        self._pending = bytearray()

    def end(self) -> None:
        """Closes both directions, once."""
        if self.ended:
            return
        self.ended = True
        self.to_plugin.put_nowait(None)
        self.inbox.put(None)

    def send(self, data: bytes) -> None:
        """Queues server bytes for the plugin, framed when the tunnel is.

        Args:
            data: The bytes; empty sends nothing.
        """
        if self.ended or not data:
            return
        self.to_plugin.put_nowait(
            encode_data_frames(data) if self.dial.tunnel == "framed" else data
        )

    def from_plugin(self, chunk: bytes) -> None:
        """Takes bytes the plugin wrote, decoding frames by an independent reading.

        Args:
            chunk: The bytes, in order.

        Raises:
            ValueError: The plugin sent a malformed frame or a second
                start-TLS frame; the connection ends.
        """
        if self.ended:
            return
        if self.dial.tunnel == "off":
            self.inbox.put(chunk)
            return
        self._pending += chunk
        at = 0
        while len(self._pending) - at >= 3:
            kind = self._pending[at]
            length = (self._pending[at + 1] << 8) | self._pending[at + 2]
            valid = (kind == 0 and 1 <= length <= MAX_TCP_FRAME_PAYLOAD) or (
                kind == 1 and length == 0
            )
            if not valid or (kind == 1 and self.tls):
                self.end()
                raise ValueError(f"test relay: malformed frame kind {kind} length {length}")
            if len(self._pending) - at - 3 < length:
                break
            if kind == 0:
                self.inbox.put(bytes(self._pending[at + 3 : at + 3 + length]))
            else:
                reason = self.tls_failure(self.dial.hostname) if self.tls_failure else None
                if reason:
                    text = reason.encode("utf-8")[:MAX_TLS_FAILURE_REASON_BYTES]
                    self.to_plugin.put_nowait(bytes((2, len(text) >> 8, len(text) & 0xFF)) + text)
                    self.tls_settled.set()
                    self.end()
                    return
                self.tls = True
                self.to_plugin.put_nowait(bytes((1, 0, 0)))
                self.tls_settled.set()
            at += 3 + length
        del self._pending[:at]


class _FakeRaw:
    """The plugin's raw socket to the fake relay."""

    def __init__(self, pair: _Pair) -> None:
        """Wraps the shared state.

        Args:
            pair: The state the fake relay and the server share.
        """
        self._pair = pair
        self._done = False

    async def read(self) -> bytes | None:
        """The next bytes from the relay, or ``None`` once closed."""
        if self._done:
            return None
        item = await self._pair.to_plugin.get()
        if item is None:
            self._done = True
        return item

    async def write(self, data: bytes) -> None:
        """Sends bytes to the relay.

        Args:
            data: The bytes.

        Raises:
            ConnectionError: The connection has ended.
        """
        if self._pair.ended:
            raise ConnectionError("the test connection is closed")
        self._pair.from_plugin(data)

    async def close(self) -> None:
        """Ends the connection."""
        self._pair.end()


class TestConnect:
    """The fake ``connect()``, plus what it saw.

    Pass :meth:`dial` as ``connect(..., dial=fake.dial)``, or call
    :meth:`connect` directly.
    """

    __test__ = False  # not a pytest test class

    def __init__(
        self,
        serve: Callable[[TestTcpConnection], Awaitable[None] | None],
        *,
        allows: Callable[[str, int], bool] | None = None,
        tls_failure: Callable[[str], str | None] | None = None,
    ) -> None:
        """Creates the fake.

        Args:
            serve: Runs the server for each connection the plugin opens.
            allows: Stands in for the plugin's ``tcp`` grant: return
                ``False`` to refuse, as the egress gate does
                (``network_not_granted``). Default: allow all.
            tls_failure: The handshake outcome for an upgrade: a reason
                string fails it, as a certificate that does not verify
                would. Default: succeed.
        """
        self._serve = serve
        self._allows = allows
        self._tls_failure = tls_failure
        self.dials: list[TestDial] = []
        """Every connection opened, in order."""
        self._tasks: set[asyncio.Task[None]] = set()

    async def dial(self, hostname: str, port: int) -> RawSocket:
        """Opens a raw socket as workerd's ``connect()`` would.

        Args:
            hostname: Host as the SDK dials it, suffix included for a
                framed tunnel.
            port: Port.

        Returns:
            The raw socket to the fake relay.

        Raises:
            ConnectionError: ``allows`` refused the host
                (``network_not_granted``).
        """
        framed = hostname.lower().endswith(STARTTLS_HOST_SUFFIX)
        host = hostname[: len(hostname) - len(STARTTLS_HOST_SUFFIX)] if framed else hostname
        record = TestDial(host, port, "framed" if framed else "off")
        self.dials.append(record)
        if self._allows is not None and not self._allows(host, port):
            raise ConnectionError(
                f"network_not_granted: tcp connect `{host}:{port}` is not in capabilities.network.tcp"
            )
        pair = _Pair(record, self._tls_failure)
        conn = TestTcpConnection(record, pair)

        async def run() -> None:
            """Runs the server script, then ends the connection."""
            try:
                result = self._serve(conn)
                if result is not None:
                    await result
            except Exception:
                pass
            finally:
                pair.end()

        task = asyncio.ensure_future(run())
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return _FakeRaw(pair)

    async def connect(
        self,
        address: str | tuple[str, int],
        *,
        secure_transport: SecureTransport = "off",
    ) -> TcpSocket:
        """The SDK's :func:`~bookclerk_plugin_sdk.tcp.connect` against this fake.

        Args:
            address: ``"host:port"`` or ``(host, port)``.
            secure_transport: ``"off"``, ``"starttls"`` or ``"on"``.

        Returns:
            The connected socket.
        """
        return await connect(address, secure_transport=secure_transport, dial=self.dial)
