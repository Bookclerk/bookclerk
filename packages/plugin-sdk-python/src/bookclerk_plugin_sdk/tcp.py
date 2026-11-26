"""TCP sockets for workerd plugins, with ``start_tls`` that works under Bookclerk.

The same behavior as the TypeScript SDK's ``connect()`` (``tcp.ts``) and the
Rust SDK's ``net`` + ``http::start_tls``: a plugin opens a socket to a host
its ``[capabilities.network] tcp`` grant names, and may upgrade it to TLS
(``secure_transport="starttls"`` then :meth:`TcpSocket.start_tls`) or start
in TLS (``secure_transport="on"``). The server's certificate is checked for
the dialed name against Mozilla's roots.

The pinned workerd cannot upgrade a socket relayed through the egress worker,
so a socket that may use TLS is a framed tunnel: the SDK dials the host with
:data:`STARTTLS_HOST_SUFFIX` appended and the launcher's egress relay
originates TLS when asked. Between the plugin and the relay every byte is a
frame: a kind byte, a big-endian 16-bit payload length, then the payload
(``bookclerk-workerd``'s ``egress_starttls`` module documents the layout).
This module hides the frames; reads and writes are plaintext.

Inside a workerd isolate::

    from bookclerk_plugin_sdk.tcp import connect

    sock = await connect(("smtp.example.com", 587), secure_transport="starttls")
    greeting = await sock.read()
    ...
    sock = await sock.start_tls()   # raises when the certificate does not verify

Tests pass ``dial=`` (see :class:`bookclerk_plugin_sdk.testkit.TestConnect`).
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any, Literal, Protocol

__all__ = [
    "MAX_TCP_FRAME_PAYLOAD",
    "MAX_TLS_FAILURE_REASON_BYTES",
    "STARTTLS_HOST_SUFFIX",
    "RawSocket",
    "RelayFrame",
    "RelayFrameDecoder",
    "SecureTransport",
    "TcpSocket",
    "connect",
    "encode_data_frames",
    "encode_start_tls_frame",
    "parse_tcp_address",
]

STARTTLS_HOST_SUFFIX = ".starttls.bookclerk.invalid"
"""Host-name suffix that asks the egress gate and relay for a framed tunnel."""

MAX_TCP_FRAME_PAYLOAD = 16 * 1024
"""Largest payload one frame carries, in bytes."""

MAX_TLS_FAILURE_REASON_BYTES = 512
"""Largest reason a TLS-failed frame carries, in bytes."""

_KIND_DATA = 0
_KIND_TLS = 1
_KIND_TLS_FAILED = 2
_HEADER_BYTES = 3

SecureTransport = Literal["off", "on", "starttls"]
"""How the socket uses TLS, as Workers ``SocketOptions.secureTransport``."""


class RawSocket(Protocol):
    """The byte stream a socket runs on: a Workers socket, or a test fake."""

    async def read(self) -> bytes | None:
        """The next bytes, or ``None`` once the peer closed."""
        ...

    async def write(self, data: bytes) -> None:
        """Sends ``data``."""
        ...

    async def close(self) -> None:
        """Closes both directions."""
        ...


Dial = Callable[[str, int], Awaitable[RawSocket]]
"""Opens a raw socket to ``(hostname, port)``; raises when refused."""


@dataclass(frozen=True)
class RelayFrame:
    """One frame the relay sent."""

    kind: Literal["data", "tls_ready", "tls_failed"]
    """What the frame is."""
    payload: bytes = b""
    """Server bytes for ``data``; the UTF-8 reason for ``tls_failed``."""


class RelayFrameDecoder:
    """Splits the relay's byte stream into :class:`RelayFrame`s (sans-IO)."""

    def __init__(self) -> None:
        """A decoder with nothing buffered."""
        self._buf = bytearray()

    def push(self, data: bytes) -> None:
        """Appends bytes read from the relay.

        Args:
            data: The next bytes, in order.
        """
        self._buf.extend(data)

    @property
    def buffered(self) -> int:
        """Bytes buffered that do not yet make a whole frame."""
        return len(self._buf)

    def next(self) -> RelayFrame | None:
        """Takes the next complete frame, if one is buffered.

        Returns:
            The frame, or ``None`` when more bytes are needed.

        Raises:
            ValueError: The header shows a malformed frame.
        """
        if len(self._buf) < _HEADER_BYTES:
            return None
        kind = self._buf[0]
        length = (self._buf[1] << 8) | self._buf[2]
        if kind == _KIND_DATA:
            if length == 0 or length > MAX_TCP_FRAME_PAYLOAD:
                raise ValueError(f"relay sent a data frame of {length} bytes")
        elif kind == _KIND_TLS:
            if length != 0:
                raise ValueError(f"relay sent a TLS-ready frame with {length} bytes")
        elif kind == _KIND_TLS_FAILED:
            if length > MAX_TLS_FAILURE_REASON_BYTES:
                raise ValueError(f"relay sent a TLS-failed reason of {length} bytes")
        else:
            raise ValueError(f"relay sent unknown frame kind {kind}")
        if len(self._buf) < _HEADER_BYTES + length:
            return None
        payload = bytes(self._buf[_HEADER_BYTES : _HEADER_BYTES + length])
        del self._buf[: _HEADER_BYTES + length]
        if kind == _KIND_DATA:
            return RelayFrame("data", payload)
        if kind == _KIND_TLS:
            return RelayFrame("tls_ready")
        return RelayFrame("tls_failed", payload)


def encode_data_frames(data: bytes) -> bytes:
    """Encodes plaintext for the server as data frames.

    Args:
        data: Bytes to send; empty encodes nothing.

    Returns:
        One frame per :data:`MAX_TCP_FRAME_PAYLOAD` bytes.
    """
    out = bytearray()
    for start in range(0, len(data), MAX_TCP_FRAME_PAYLOAD):
        chunk = data[start : start + MAX_TCP_FRAME_PAYLOAD]
        out += bytes((_KIND_DATA, len(chunk) >> 8, len(chunk) & 0xFF))
        out += chunk
    return bytes(out)


def encode_start_tls_frame() -> bytes:
    """The start-TLS frame."""
    return bytes((_KIND_TLS, 0, 0))


def parse_tcp_address(address: str | tuple[str, int]) -> tuple[str, int]:
    """Parses ``"host:port"`` (IPv6 in brackets) or ``(host, port)``.

    Args:
        address: Where to connect.

    Returns:
        ``(hostname, port)`` with IPv6 brackets removed.

    Raises:
        ValueError: No host, or a port outside 1 to 65535.
    """
    if isinstance(address, str):
        host, sep, port_text = address.rpartition(":")
        if not sep or not host:
            raise ValueError(f"connect address {address!r} has no port")
        if host.startswith("[") and host.endswith("]"):
            host = host[1:-1]
        try:
            port = int(port_text)
        except ValueError:
            raise ValueError(f"connect port {port_text!r} is not a number") from None
    else:
        host, port = address
    if not isinstance(host, str) or not host:
        raise ValueError("connect address has no host")
    if not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
        raise ValueError(f"connect port {port!r} is not 1 to 65535")
    return host, port


class TcpSocket:
    """A TCP socket; plaintext reads and writes, TLS done by the relay."""

    def __init__(
        self,
        raw: RawSocket,
        hostname: str,
        port: int,
        *,
        framed: bool,
        can_upgrade: bool,
        tls: bool = False,
        decoder: RelayFrameDecoder | None = None,
    ) -> None:
        """Wraps ``raw``; use :func:`connect` rather than this constructor.

        Args:
            raw: The byte stream to the relay.
            hostname: Host the plugin dialed, without the suffix.
            port: Port the plugin dialed.
            framed: Whether ``raw`` carries frames.
            can_upgrade: Whether :meth:`start_tls` is allowed.
            tls: Whether the socket is already inside TLS.
            decoder: Frames already read, carried over an upgrade.
        """
        self._raw = raw
        self.hostname = hostname
        """Host the plugin dialed."""
        self.port = port
        """Port the plugin dialed."""
        self.tls = tls
        """Whether reads and writes are inside TLS."""
        self._framed = framed
        self._can_upgrade = can_upgrade
        self._usable = True
        self._decoder = decoder or RelayFrameDecoder()
        self._eof = False

    def _check_usable(self) -> None:
        """Raises when this socket has upgraded or closed."""
        if not self._usable:
            raise RuntimeError("the socket upgraded; use the socket start_tls() returned")

    async def _next_frame(self) -> RelayFrame | None:
        """The next relay frame, or ``None`` at end of stream."""
        while True:
            try:
                frame = self._decoder.next()
            except ValueError:
                await self._raw.close()
                raise
            if frame is not None:
                return frame
            if self._eof:
                if self._decoder.buffered:
                    raise ValueError("relay ended inside a frame")
                return None
            chunk = await self._raw.read()
            if chunk is None:
                self._eof = True
            else:
                self._decoder.push(chunk)

    async def read(self) -> bytes | None:
        """The next bytes from the server.

        Returns:
            Plaintext bytes, or ``None`` once the server closed.

        Raises:
            RuntimeError: The socket upgraded; read the new one.
            ValueError: The relay sent a malformed or unexpected frame.
        """
        self._check_usable()
        if not self._framed:
            return await self._raw.read()
        frame = await self._next_frame()
        if frame is None:
            return None
        if frame.kind != "data":
            await self._raw.close()
            raise ValueError(f"relay sent {frame.kind} unasked")
        return frame.payload

    async def write(self, data: bytes) -> None:
        """Sends bytes to the server.

        Args:
            data: Plaintext bytes; empty sends nothing.

        Raises:
            RuntimeError: The socket upgraded; write the new one.
        """
        self._check_usable()
        if not data:
            return
        await self._raw.write(encode_data_frames(bytes(data)) if self._framed else bytes(data))

    async def start_tls(self) -> TcpSocket:
        """Upgrades to TLS; call after reading the server's go-ahead.

        Returns:
            The secure socket. This socket stops being usable.

        Raises:
            RuntimeError: The socket was not opened with
                ``secure_transport="starttls"``, or has already upgraded.
            ConnectionError: The handshake failed (for example the
                certificate does not verify for the host), or the connection
                closed first.
        """
        self._check_usable()
        if not self._can_upgrade:
            raise RuntimeError('start_tls() needs a socket opened with secure_transport="starttls"')
        self._usable = False
        await self._raw.write(encode_start_tls_frame())
        while True:
            frame = await self._next_frame()
            if frame is None:
                raise ConnectionError("the connection closed before TLS was ready")
            if frame.kind == "tls_ready":
                return TcpSocket(
                    self._raw,
                    self.hostname,
                    self.port,
                    framed=True,
                    can_upgrade=False,
                    tls=True,
                    decoder=self._decoder,
                )
            if frame.kind == "tls_failed":
                await self._raw.close()
                reason = frame.payload.decode("utf-8", "replace")
                raise ConnectionError(f"TLS handshake failed: {reason}")
            # Server bytes between the go-ahead and the upgrade belong to no
            # one; a well-behaved server sends none.
            await self._raw.close()
            raise ValueError("server sent data while TLS was starting")

    async def close(self) -> None:
        """Closes the socket."""
        self._usable = False
        await self._raw.close()


class _JsSocket:
    """A Workers ``cloudflare:sockets`` socket as a :class:`RawSocket`."""

    def __init__(self, socket: Any) -> None:
        """Takes the socket's reader and writer.

        Args:
            socket: The JavaScript socket ``connect()`` returned.
        """
        self._socket = socket
        self._reader = socket.readable.getReader()
        self._writer = socket.writable.getWriter()

    async def opened(self) -> None:
        """Waits for the connection; raises when the egress gate refused it."""
        await self._socket.opened

    async def read(self) -> bytes | None:
        """The next bytes, or ``None`` once the peer closed."""
        result = await self._reader.read()
        if result.done:
            return None
        return bytes(result.value.to_py())

    async def write(self, data: bytes) -> None:
        """Sends ``data``."""
        from pyodide.ffi import to_js  # type: ignore[import-not-found]

        await self._writer.write(to_js(data))

    async def close(self) -> None:
        """Closes both directions."""
        await self._socket.close()


async def _workers_dial(hostname: str, port: int) -> RawSocket:
    """Dials through workerd's ``connect()``; only inside an isolate."""
    from js import Object  # type: ignore[import-not-found]
    from pyodide.ffi import to_js  # type: ignore[import-not-found]
    from workers import import_from_javascript  # type: ignore[import-not-found]

    sockets = import_from_javascript("cloudflare:sockets")
    socket = sockets.connect(
        to_js({"hostname": hostname, "port": port}, dict_converter=Object.fromEntries),
        to_js({"secureTransport": "off"}, dict_converter=Object.fromEntries),
    )
    raw = _JsSocket(socket)
    await raw.opened()
    return raw


async def connect(
    address: str | tuple[str, int],
    *,
    secure_transport: SecureTransport = "off",
    dial: Dial | None = None,
) -> TcpSocket:
    """Opens a TCP socket to a host the plugin's ``tcp`` grant names.

    Args:
        address: ``"host:port"`` or ``(host, port)``.
        secure_transport: ``"off"`` (plain), ``"starttls"`` (plain until
            :meth:`TcpSocket.start_tls`) or ``"on"`` (TLS from the start).
        dial: Opens the raw socket; workerd's ``connect()`` by default, a
            test fake in tests.

    Returns:
        The connected socket (after the handshake for ``"on"``).

    Raises:
        ValueError: A bad address, an IPv6 host with TLS, or an unknown mode.
        ConnectionError: The TLS handshake failed.
        Exception: Whatever ``dial`` raises when the grant refuses the host.
    """
    hostname, port = parse_tcp_address(address)
    if secure_transport not in ("off", "on", "starttls"):
        raise ValueError(f"secure_transport {secure_transport!r} is not off, on or starttls")
    opener = dial or _workers_dial
    if secure_transport == "off":
        return TcpSocket(
            await opener(hostname, port), hostname, port, framed=False, can_upgrade=False
        )
    if ":" in hostname:
        raise ValueError("TLS sockets need a host name or IPv4 address, not IPv6")
    if hostname.lower().endswith(STARTTLS_HOST_SUFFIX):
        raise ValueError(f"connect host may not end in {STARTTLS_HOST_SUFFIX}")
    raw = await opener(f"{hostname}{STARTTLS_HOST_SUFFIX}", port)
    sock = TcpSocket(raw, hostname, port, framed=True, can_upgrade=True)
    return await sock.start_tls() if secure_transport == "on" else sock
