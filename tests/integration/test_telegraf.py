"""Integration tests for the Telegraf writer factory and the four writers.

All real transports/clients are mocked:
  * UDP  -> ``loop.create_datagram_endpoint`` returns a ``FakeDatagramTransport``,
  * TCP  -> ``asyncio.open_connection`` returns ``FakeStreamReader/Writer``,
  * ExecD-> ``sys.stdout`` is replaced with an in-memory buffer,
  * MQTT -> ``gmqtt.Client`` is replaced with a recording fake.

The TCP reconnect / connect-retry paths use ``local_sleep_patch`` so the
``asyncio.sleep(1)`` backoff is instant but still yields to the loop.
"""

from __future__ import annotations

import asyncio
import inspect
import io
import sys

import pytest
from support import fakes

import loxInFlux.telegraf  # noqa: F401  (ensures the submodule is in sys.modules)
from loxInFlux.config import get_config
from loxInFlux.telegraf import (
    ExecDTelegrafWriter,
    MQTTTelegrafWriter,
    TCPTelegrafWriter,
    UDPTelegrafWriter,
)

# Resolve the real module via sys.modules to reach its module-level globals
# (e.g. the ``sys`` reference the ExecD writer uses).
telegraf_mod = sys.modules["loxInFlux.telegraf"]

# A realistic InfluxDB line-protocol datagram: measurement + tags + fields +
# nanosecond timestamp. The writers are byte-transparent transports, so the
# transport must receive this *exact* payload (byte-for-byte) - hence the tests
# below assert full-payload equality rather than substrings or counts.
_LINE = b"loxone,source=grabber,room=Wohnzimmer temperature=21.5,valid=1i 1700000000000000000"


# --------------------------------------------------------------------------- #
# UDP                                                                          #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_udp_write_sends_exact_datagram(monkeypatch):
    transport = fakes.FakeDatagramTransport()

    async def fake_cde(protocol_factory, *, remote_addr=None):
        return transport, protocol_factory()

    monkeypatch.setattr(
        asyncio.get_running_loop(), "create_datagram_endpoint", fake_cde
    )

    writer = UDPTelegrafWriter()
    await writer.initialize()
    await writer.write(_LINE)
    # One datagram carrying the exact line-protocol bytes, unmodified.
    assert transport.sent == [_LINE]

    # A second write is delivered as its own datagram - no concatenation, no
    # dropped/duplicated bytes.
    other = b"loxone,source=ws value=0"
    await writer.write(other)
    assert transport.sent == [_LINE, other]

    await writer.close()
    assert transport.closed
    assert writer._initialized is False


@pytest.mark.asyncio
async def test_udp_write_failure_triggers_reconnect(monkeypatch):
    dead = fakes.FakeDatagramTransport(fail_sendto=OSError("network down"))
    healthy = fakes.FakeDatagramTransport()
    transports = iter([dead, healthy])

    async def fake_cde(protocol_factory, *, remote_addr=None):
        return next(transports), protocol_factory()

    monkeypatch.setattr(
        asyncio.get_running_loop(), "create_datagram_endpoint", fake_cde
    )

    writer = UDPTelegrafWriter()
    await writer.initialize()
    # The failing sendto is swallowed and a reconnect is performed.
    await writer.write(_LINE)

    assert dead.closed is True
    assert writer.transport is healthy
    assert writer._initialized is True
    # UDP is fire-and-forget: the datagram that hit the dead socket is dropped,
    # not replayed onto the freshly reconnected transport.
    assert healthy.sent == []


# --------------------------------------------------------------------------- #
# TCP                                                                          #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_tcp_write_and_close(monkeypatch):
    reader = fakes.FakeStreamReader()
    writer_stream = fakes.FakeStreamWriter()

    async def fake_open(host, port):
        return reader, writer_stream

    monkeypatch.setattr(telegraf_mod.asyncio, "open_connection", fake_open)

    writer = TCPTelegrafWriter()
    await writer.initialize()
    await writer.write(_LINE)

    # TCP is a byte stream: each point must be newline-terminated so Telegraf's
    # line-by-line socket_listener can delimit points. The stream received the
    # line-protocol bytes plus a trailing newline and was drained once.
    assert bytes(writer_stream.data) == _LINE + b"\n"
    assert writer_stream.drained == 1

    # A second write is appended as its own newline-terminated record; the two
    # points stay separated on the stream (no concatenation into one line).
    other = b"loxone,source=ws value=0"
    await writer.write(other)
    assert bytes(writer_stream.data) == _LINE + b"\n" + other + b"\n"
    assert writer_stream.drained == 2

    await writer.close()
    assert writer_stream.closed is True
    assert writer_stream.wait_closed_called is True


@pytest.mark.asyncio
async def test_tcp_write_drain_failure_reconnects(monkeypatch):
    dead = fakes.FakeStreamWriter(fail_drain=OSError("broken pipe"))
    healthy = fakes.FakeStreamWriter()
    writers = iter([dead, healthy])
    readers = iter([fakes.FakeStreamReader(), fakes.FakeStreamReader()])

    async def fake_open(host, port):
        return next(readers), next(writers)

    monkeypatch.setattr(telegraf_mod.asyncio, "open_connection", fake_open)

    writer = TCPTelegrafWriter()
    await writer.initialize()
    await writer.write(_LINE)  # write reaches the dead stream, drain raises

    assert dead.closed is True
    assert writer.writer is healthy
    assert writer._initialized is True
    # The payload (newline-terminated) was handed to the dead stream before drain
    # failed; the reconnect does not replay it, so the healthy stream starts empty.
    assert bytes(dead.data) == _LINE + b"\n"
    assert bytes(healthy.data) == b""


@pytest.mark.asyncio
async def test_tcp_connect_retry_loop_raises_after_exhausting_retries(
    monkeypatch, local_sleep_patch
):
    # The no-arg form patches the global ``asyncio.sleep`` (which telegraf uses)
    # - simplest way to make the connect-retry backoff instant here.
    sleeper = local_sleep_patch()

    async def always_fail(host, port):
        raise OSError("connection refused")

    monkeypatch.setattr(telegraf_mod.asyncio, "open_connection", always_fail)

    writer = TCPTelegrafWriter()  # bootstrap config: max_retries == 3
    # connect() makes exactly max_retries attempts (1,2,3); the final failure is
    # re-raised. Only the first two attempts back off (no sleep before raising).
    with pytest.raises(OSError, match="connection refused"):
        await writer.connect()

    assert sleeper.calls == [1, 1]
    assert writer.writer is None


@pytest.mark.asyncio
async def test_tcp_connect_unlimited_retries_until_success(
    monkeypatch, local_sleep_patch
):
    # max_retries <= 0 means "unlimited": keep retrying (backing off) until a
    # connection succeeds, never raising in between.
    sleeper = local_sleep_patch()
    reader = fakes.FakeStreamReader()
    writer_stream = fakes.FakeStreamWriter()
    attempts = {"n": 0}

    async def fail_twice_then_succeed(host, port):
        attempts["n"] += 1
        if attempts["n"] < 3:
            raise OSError("connection refused")
        return reader, writer_stream

    monkeypatch.setattr(
        telegraf_mod.asyncio, "open_connection", fail_twice_then_succeed
    )

    writer = TCPTelegrafWriter()
    writer.max_retries = 0  # unlimited

    await writer.connect()

    assert attempts["n"] == 3
    assert sleeper.calls == [1, 1]  # backed off after the two failures
    assert writer.writer is writer_stream


@pytest.mark.asyncio
async def test_tcp_real_socket_delivers_two_points_as_two_lines(monkeypatch):
    """End-to-end TCP contract against a real ``asyncio.start_server``.

    Telegraf's ``socket_listener`` reads InfluxDB line protocol over TCP
    line-by-line. This spins up an actual loopback server that consumes the
    stream via ``reader.readline()`` (i.e. newline-delimited framing, exactly
    like Telegraf) and asserts two writes arrive as two SEPARATE, newline-
    terminated lines.

    This is the regression guard for the newline bug: if ``write`` stopped
    appending ``\\n`` the two points would coalesce into a single un-terminated
    line, ``readline()`` would never see a second line, and the wait below would
    time out (the FakeStreamWriter unit tests could not catch this - they only
    inspect the raw bytes handed to the transport, not line framing).
    """
    received: list[bytes] = []
    got_two = asyncio.Event()

    async def handle(reader, server_writer):
        try:
            while len(received) < 2:
                line = await reader.readline()
                if not line:  # client closed before sending two lines
                    break
                received.append(line)
            got_two.set()
        finally:
            # Close the server-side stream so its StreamWriter isn't GC'd unclosed
            # (which would raise a PytestUnraisableExceptionWarning).
            server_writer.close()

    server = await asyncio.start_server(handle, "127.0.0.1", 0)
    host, port = server.sockets[0].getsockname()[:2]

    # Point the writer at the real loopback server (config is re-init per test).
    monkeypatch.setattr(get_config().telegraf, "host", host)
    monkeypatch.setattr(get_config().telegraf, "port", port)

    other = b"loxone,source=ws value=0"
    writer = TCPTelegrafWriter()
    try:
        await writer.initialize()
        await writer.write(_LINE)
        await writer.write(other)

        # Would time out if the two points were not newline-separated.
        await asyncio.wait_for(got_two.wait(), timeout=5)
    finally:
        # Close the client first so the server-side ``readline`` sees EOF and the
        # handler terminates - otherwise ``wait_closed`` would hang on a bug that
        # left the two points unframed (making the failure a clean timeout).
        await writer.close()
        server.close()
        await server.wait_closed()

    # Exactly two lines, each the original payload with a single trailing newline.
    assert received == [_LINE + b"\n", other + b"\n"]


# --------------------------------------------------------------------------- #
# ExecD                                                                        #
# --------------------------------------------------------------------------- #
class _FakeStdout:
    """Minimal stdout replacement: buffered writes + no-op reconfigure/flush."""

    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def reconfigure(self, **_kwargs) -> None:
        pass

    def flush(self) -> None:
        pass


@pytest.mark.asyncio
async def test_execd_write_appends_newline(monkeypatch):
    fake_stdout = _FakeStdout()
    monkeypatch.setattr(telegraf_mod.sys, "stdout", fake_stdout)

    writer = ExecDTelegrafWriter()
    await writer.write(_LINE)
    # ExecD emits the exact payload plus a single trailing newline (line-delimited
    # protocol for the Telegraf execd plugin) - nothing else.
    assert fake_stdout.buffer.getvalue() == _LINE + b"\n"

    # A second line is appended as its own newline-terminated record.
    other = b"loxone,source=ws value=0"
    await writer.write(other)
    assert fake_stdout.buffer.getvalue() == _LINE + b"\n" + other + b"\n"


@pytest.mark.asyncio
async def test_execd_initialize_and_close(monkeypatch):
    monkeypatch.setattr(telegraf_mod.sys, "stdout", _FakeStdout())

    writer = ExecDTelegrafWriter()
    await writer.initialize()
    assert writer._initialized is True

    await writer.close()
    assert writer._initialized is False


# --------------------------------------------------------------------------- #
# MQTT                                                                         #
# --------------------------------------------------------------------------- #
class _FakeMQTTClient:
    def __init__(self, client_id) -> None:
        self.client_id = client_id
        self.config = None
        self.auth = None
        self.connected = False
        self.disconnected = False
        self.connect_kwargs = None
        self.published: list[tuple] = []
        self.on_connect = None
        self.on_disconnect = None

    def set_config(self, cfg) -> None:
        self.config = cfg

    def set_auth_credentials(self, username, password) -> None:
        self.auth = (username, password)

    async def connect(self, **kwargs) -> None:
        self.connected = True
        self.connect_kwargs = kwargs

    async def disconnect(self) -> None:
        self.disconnected = True

    def publish(self, topic, point, qos=0) -> None:
        self.published.append((topic, point, qos))


@pytest.mark.asyncio
async def test_mqtt_connect_write_close(monkeypatch):
    monkeypatch.setattr(telegraf_mod, "MQTTClient", _FakeMQTTClient)

    writer = MQTTTelegrafWriter()
    await writer.initialize()

    assert isinstance(writer.client, _FakeMQTTClient)
    assert writer.client.connected is True
    assert writer.client.client_id == writer.mqtt_config.client_id
    assert writer.client.auth == (
        writer.mqtt_config.username,
        writer.mqtt_config.password,
    )

    await writer.write(_LINE)
    # Exactly one publish, carrying the unmodified line-protocol bytes on the
    # configured topic at qos 0.
    assert writer.client.published == [(writer.mqtt_config.topic, _LINE, 0)]

    await writer.close()
    assert writer.client.disconnected is True
    assert writer._initialized is False


@pytest.mark.asyncio
async def test_mqtt_publish_failure_triggers_reconnect(monkeypatch):
    class _FailingPublishClient(_FakeMQTTClient):
        def __init__(self, client_id) -> None:
            super().__init__(client_id)
            self._failed_once = False

        def publish(self, topic, point, qos=0) -> None:
            if not self._failed_once:
                self._failed_once = True
                raise RuntimeError("publish failed")
            super().publish(topic, point, qos)

    monkeypatch.setattr(telegraf_mod, "MQTTClient", _FailingPublishClient)

    writer = MQTTTelegrafWriter()
    await writer.initialize()
    first_client = writer.client

    # The failing publish is swallowed and the writer reconnects a fresh client.
    await writer.write(b"x")

    assert first_client.disconnected is True
    assert writer.client is not first_client
    assert writer._initialized is True


@pytest.mark.asyncio
async def test_mqtt_callbacks_match_gmqtt_bound_signature(monkeypatch, caplog):
    """gmqtt invokes the callbacks as ``cb(client, flags, rc, properties)`` and
    ``cb(client, packet)``; because we register *bound* methods the writer
    instance is already bound to ``self``. This reproduces that exact call shape.

    Regression: the callbacks previously lacked ``self``, shifting every arg by
    one - which made ``_on_disconnect`` treat the MQTT packet as an exception and
    log a bogus "Disconnect error".
    """
    monkeypatch.setattr(telegraf_mod, "MQTTClient", _FakeMQTTClient)

    writer = MQTTTelegrafWriter()
    await writer.initialize()

    on_connect = writer.client.on_connect
    on_disconnect = writer.client.on_disconnect

    with caplog.at_level("INFO"):
        # gmqtt 0.7.0: on_connect(self=client, session_present, result, properties)
        on_connect(writer.client, 0, 0, {})
        # gmqtt 0.7.0: on_disconnect(self=client, packet)
        on_disconnect(writer.client, b"\x00")

    assert "MQTT connected" in caplog.text
    assert "MQTT disconnected" in caplog.text
    # A normal disconnect packet must NOT be misreported as an error.
    assert "Disconnect error" not in caplog.text


@pytest.mark.asyncio
async def test_mqtt_disconnect_reports_real_error(monkeypatch, caplog):
    monkeypatch.setattr(telegraf_mod, "MQTTClient", _FakeMQTTClient)

    writer = MQTTTelegrafWriter()
    await writer.initialize()

    with caplog.at_level("ERROR"):
        writer.client.on_disconnect(writer.client, b"\x00", RuntimeError("boom"))

    assert "Disconnect error" in caplog.text
    assert "boom" in caplog.text


def test_mqtt_callback_signatures_match_gmqtt_contract():
    """Pin the exact callback arity gmqtt expects (regression guard).

    gmqtt invokes ``on_connect(client, flags, rc, properties)`` and
    ``on_disconnect(client, packet, exc=None)`` on the registered *bound*
    methods (``self`` already applied). ``inspect.signature`` on the bound method
    therefore drops ``self`` and must expose exactly these parameters - if a
    refactor drops ``self`` again every positional arg shifts and
    ``_on_disconnect`` would treat the MQTT packet as an exception.
    """
    writer = MQTTTelegrafWriter()

    assert list(inspect.signature(writer._on_connect).parameters) == [
        "client",
        "flags",
        "rc",
        "properties",
    ]
    assert list(inspect.signature(writer._on_disconnect).parameters) == [
        "client",
        "packet",
        "exc",
    ]
