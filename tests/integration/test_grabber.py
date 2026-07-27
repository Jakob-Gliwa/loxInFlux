"""Integration tests for :class:`loxInFlux.grabber.LoxoneGrabber`.

Covers:
  * ``_grab_all_values`` state-gating (skips when the websocket is not CONNECTED)
    and its secured/unsecured dispatch based on each control's ``VisuPwd`` flag,
  * ``send_command`` for both the secured and unsecured websocket paths (plus
    shutdown short-circuit and error swallowing),
  * ``start`` when the grabber is disabled in config,
  * the ``start`` retry loop timing via ``local_sleep_patch`` (both the happy
    interval sleep and the 5s error backoff).

``loxInFlux.grabber`` is NOT re-exported as an instance, so it resolves to the
real submodule and ``local_sleep_patch("loxInFlux.grabber")`` works directly.
"""

from __future__ import annotations

import asyncio

import pytest

import loxInFlux.grabber as grabber_mod
from loxInFlux.config import get_config
from loxInFlux.grabber import LoxoneGrabber


class _FakeWS:
    """Minimal loxwebsocket stand-in for the grabber's send paths."""

    def __init__(self, state: str = "CONNECTED") -> None:
        self.state = state
        self.unsecured: list[tuple] = []
        self.secured: list[tuple] = []

    async def send_websocket_command(self, uuid, command) -> None:
        self.unsecured.append((uuid, command))

    async def send_websocket_command_to_visu_password_secured_control(
        self, uuid, command, password
    ) -> None:
        self.secured.append((uuid, command, password))


# --------------------------------------------------------------------------- #
# send_command                                                                 #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_send_command_unsecured():
    ws = _FakeWS()
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    await grabber.send_command(b"uuid-1", secured=False)

    # send_command decodes bytes UUIDs to str before dispatching.
    assert ws.unsecured == [("uuid-1", "all")]
    assert ws.secured == []


@pytest.mark.asyncio
async def test_send_command_secured_uses_visu_password():
    ws = _FakeWS()
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    await grabber.send_command(b"uuid-2", secured=True)

    # The secured path also receives a decoded str UUID (never b'uuid-2').
    assert ws.secured == [("uuid-2", "all", get_config().miniserver.visu_password)]
    assert ws.unsecured == []


@pytest.mark.asyncio
async def test_send_command_secured_receives_str_uuid_contract():
    # A2 contract: the visu-password-secured call must receive a plain str UUID
    # (loxwebsocket does NOT decode there), otherwise the command string would
    # interpolate a b'...' repr.
    ws = _FakeWS()
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    await grabber.send_command(b"0f1e2d3c-0011", secured=True)

    ((uuid, command, _password),) = ws.secured
    assert command == "all"
    assert uuid == "0f1e2d3c-0011"
    assert isinstance(uuid, str)
    assert not isinstance(uuid, bytes)


@pytest.mark.asyncio
async def test_send_command_passthrough_when_already_str():
    # A str UUID must be forwarded unchanged (no double-encoding/decoding).
    ws = _FakeWS()
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    await grabber.send_command("already-str", secured=True)

    assert ws.secured == [("already-str", "all", get_config().miniserver.visu_password)]


@pytest.mark.asyncio
async def test_send_command_secured_uses_public_visu_method_not_lowlevel():
    # Regression (Bug 3): the secured path must call the PUBLIC queueing method
    # ``send_websocket_command_to_visu_password_secured_control`` (which requests
    # salt/key), NOT the low-level ``send_command_to_visu_password_secured_control``
    # which assumes an already-established visual hash and fails in production.
    class _BothMethodsWS:
        def __init__(self) -> None:
            self.public: list[tuple] = []
            self.lowlevel: list[tuple] = []

        async def send_websocket_command_to_visu_password_secured_control(
            self, uuid, value, pw
        ) -> None:
            self.public.append((uuid, value, pw))

        async def send_command_to_visu_password_secured_control(
            self, uuid, value, pw
        ) -> None:
            self.lowlevel.append((uuid, value, pw))

    ws = _BothMethodsWS()
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    await grabber.send_command(b"secured-uuid", secured=True)

    assert ws.public == [("secured-uuid", "all", get_config().miniserver.visu_password)]
    assert ws.lowlevel == []


@pytest.mark.asyncio
async def test_send_command_noop_when_shutting_down():
    ws = _FakeWS()
    grabber = LoxoneGrabber()
    grabber.ws_client = ws
    grabber._shutdown_event.set()

    await grabber.send_command(b"uuid-3")

    assert ws.unsecured == []
    assert ws.secured == []


@pytest.mark.asyncio
async def test_send_command_swallows_errors():
    class _RaisingWS(_FakeWS):
        async def send_websocket_command(self, uuid, command) -> None:
            raise RuntimeError("ws exploded")

    grabber = LoxoneGrabber()
    grabber.ws_client = _RaisingWS()

    # Must not propagate - the grabber logs and continues.
    await grabber.send_command(b"uuid-4")


# --------------------------------------------------------------------------- #
# _grab_all_values (state-gating + dispatch)                                   #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_grab_all_values_skips_when_not_connected():
    ws = _FakeWS(state="CONNECTING")
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    called = False

    async def getter():
        nonlocal called
        called = True
        return {}, {}, {}

    grabber.controlGetter = getter

    await grabber._grab_all_values()

    assert called is False
    assert ws.unsecured == []
    assert ws.secured == []


@pytest.mark.asyncio
async def test_grab_all_values_dispatches_by_visu_pwd():
    ws = _FakeWS(state="CONNECTED")
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    grabber_controls = {
        b"plain": {"VisuPwd": False},
        b"secured": {"VisuPwd": True},
    }

    async def getter():
        return {}, {}, grabber_controls

    grabber.controlGetter = getter

    await grabber._grab_all_values()

    assert ws.unsecured == [("plain", "all")]
    assert ws.secured == [("secured", "all", get_config().miniserver.visu_password)]


@pytest.mark.asyncio
async def test_grab_all_values_decodes_builder_byte_uuids(sample_controls):
    # A2 roundtrip: grabber_controls from getControlsFromConfigXML are keyed by
    # UUID *bytes*; every dispatched command must carry a decoded str UUID.
    ws = _FakeWS(state="CONNECTED")
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    async def getter():
        return {}, {}, sample_controls.grabber_controls

    grabber.controlGetter = getter

    await grabber._grab_all_values()

    dispatched = [call[0] for call in ws.unsecured] + [call[0] for call in ws.secured]
    assert dispatched, "builder fixture should yield at least one grabber control"
    assert all(isinstance(uuid, str) for uuid in dispatched)


@pytest.mark.asyncio
async def test_grab_all_values_empty_control_set_is_noop():
    ws = _FakeWS(state="CONNECTED")
    grabber = LoxoneGrabber()
    grabber.ws_client = ws

    async def getter():
        return {}, {}, {}

    grabber.controlGetter = getter

    await grabber._grab_all_values()

    assert ws.unsecured == []
    assert ws.secured == []


# --------------------------------------------------------------------------- #
# start (disabled)                                                             #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_start_returns_early_when_disabled():
    get_config().general.grabber = False
    grabber = LoxoneGrabber()

    called = False

    async def getter():
        nonlocal called
        called = True
        return {}, {}, {}

    await grabber.start(getter)

    # start binds the websocket + getter before the disabled check, then returns
    # without ever entering the poll loop.
    assert grabber.ws_client is grabber_mod.loxwebsocket
    assert called is False


# --------------------------------------------------------------------------- #
# start retry loop timing                                                      #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_start_loop_sleeps_grabber_interval(local_sleep_patch):
    get_config().general.grabber = True
    get_config().general.grabber_interval = 300
    sleeper = local_sleep_patch("loxInFlux.grabber")

    grabber = LoxoneGrabber()
    calls: list[int] = []

    async def fake_grab():
        calls.append(1)
        grabber._shutdown_event.set()

    grabber._grab_all_values = fake_grab

    async def getter():
        return {}, {}, {}

    await grabber.start(getter)

    assert calls == [1]
    assert sleeper.calls == [300]


@pytest.mark.asyncio
async def test_start_loop_backs_off_five_seconds_on_error(local_sleep_patch):
    get_config().general.grabber = True
    sleeper = local_sleep_patch("loxInFlux.grabber")

    grabber = LoxoneGrabber()
    calls: list[int] = []

    async def failing_grab():
        calls.append(1)
        if len(calls) >= 3:
            grabber._shutdown_event.set()
        raise RuntimeError("boom")

    grabber._grab_all_values = failing_grab

    async def getter():
        return {}, {}, {}

    await grabber.start(getter)

    assert len(calls) == 3
    assert sleeper.calls == [5, 5, 5]


# --------------------------------------------------------------------------- #
# stop                                                                         #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_stop_sets_shutdown_event():
    grabber = LoxoneGrabber()
    assert grabber._shutdown_event.is_set() is False

    await grabber.stop()

    assert grabber._shutdown_event.is_set() is True


@pytest.mark.asyncio
async def test_stop_lets_running_start_loop_exit(local_sleep_patch):
    get_config().general.grabber = True
    get_config().general.grabber_interval = 300
    local_sleep_patch("loxInFlux.grabber")

    grabber = LoxoneGrabber()

    grabs = 0

    async def fake_grab():
        nonlocal grabs
        grabs += 1

    grabber._grab_all_values = fake_grab

    async def getter():
        return {}, {}, {}

    task = asyncio.create_task(grabber.start(getter))
    # Let the loop spin up before requesting shutdown.
    await asyncio.sleep(0)
    await grabber.stop()

    # stop() flips the event; the loop must observe it and finish promptly.
    await asyncio.wait_for(task, timeout=1)
    assert task.done()
    assert grabber._shutdown_event.is_set() is True
    assert grabs >= 1
