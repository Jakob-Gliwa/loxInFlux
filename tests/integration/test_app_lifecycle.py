"""Lifecycle / orchestration tests for :class:`LoxInfluxBridge` and ``run_bridge``.

These exercise the parts of ``app.py`` beyond the two message handlers: gathering
controls from the miniserver, the fault-tolerant shutdown, the reconnect-driven
re-gather, the signal-driven ``run_bridge`` supervisor, and the value-state
adoption path.

External I/O is faked locally:
  * a **local** ``_FakeLoxWs`` stands in for the loxwebsocket singleton (only the
    lifecycle surface the bridge touches: ``stop`` / ``connect`` / callback
    registration). It is defined here on purpose - the shared ``tests/support``
    doubles are owned elsewhere.
  * ``init_gather_controls_from_miniserver`` downloads over HTTP, so we patch
    ``loxInFlux.miniserver.aiohttp.ClientSession`` with the read-only
    ``FakeAiohttpSession`` and build the LoxCC/zip payload with the read-only
    ``support.loxcc`` / ``support.loxone_xml`` builders.
  * ``mock_telegraf`` (from conftest) records / no-ops the telegraf writer.

Lifecycle waits are always wrapped in ``asyncio.wait_for`` with small timeouts so
a wiring regression fails fast instead of hanging the suite.
"""

from __future__ import annotations

import asyncio
import contextlib
import enum
import logging
import os
import signal
import sys

import pytest
from loxwebsocket.exceptions import LoxoneException
from loxwebsocket.lox_ws_api import LoxWs
from support import fakes, loxcc, loxone_xml

from loxInFlux.app import LoxInfluxBridge, run_bridge
from loxInFlux.config import FilterConfig, get_config

_TIMEOUT = 1.0


# --------------------------------------------------------------------------- #
# local loxwebsocket fake                                                     #
# --------------------------------------------------------------------------- #
class _FakeLoxWs:
    """Local stand-in for the loxwebsocket singleton (lifecycle surface only)."""

    class EventType(enum.IntEnum):
        ANY = 0
        INITIALIZED = 1
        CONNECTED = 2
        CONNECTION_CLOSED = 3
        RECONNECTED = 4

    def __init__(
        self,
        *,
        stop_error: BaseException | None = None,
        connect_error: BaseException | None = None,
    ) -> None:
        self.state = "CLOSED"
        self.stop_calls = 0
        self._stop_error = stop_error
        self._connect_error = connect_error
        self.message_callbacks: list[tuple] = []
        self.event_callbacks: list[tuple] = []
        self.connect_kwargs: dict | None = None

    async def connect(self, **kwargs) -> None:
        self.connect_kwargs = kwargs
        # A failed connect (e.g. exhausted reconnect attempts) surfaces as a
        # LoxoneException from the real client; mirror that here.
        if self._connect_error is not None:
            raise self._connect_error
        self.state = "CONNECTED"

    def add_message_callback(self, callback, message_types=None) -> None:
        self.message_callbacks.append((callback, message_types))

    def add_event_callback(self, callback, event_types=None) -> None:
        self.event_callbacks.append((callback, event_types))

    async def stop(self) -> int:
        self.stop_calls += 1
        if self._stop_error is not None:
            raise self._stop_error
        self.state = "CLOSED"
        return 0


# --------------------------------------------------------------------------- #
# local grabber fake                                                          #
# --------------------------------------------------------------------------- #
class _FakeGrabber:
    """Local stand-in for ``LoxoneGrabber`` used by ``init_grabber``.

    ``init_grabber`` constructs the grabber itself (``LoxoneGrabber()``) and
    then schedules ``grabber.start(self.get_controls)`` as a bridge-owned task.
    This fake records the controls-getter it was handed, signals ``started`` and
    then blocks on ``_release`` so the spawned task stays alive in
    ``bridge._tasks`` for the assertion (and is torn down in the test's finally).
    """

    def __init__(self) -> None:
        self.start_calls: list = []
        self.started = asyncio.Event()
        self._release = asyncio.Event()

    async def start(self, controls_getter) -> None:
        self.start_calls.append(controls_getter)
        self.started.set()
        await self._release.wait()


# --------------------------------------------------------------------------- #
# miniserver config fixtures (HTTP download fake)                             #
# --------------------------------------------------------------------------- #
def _build_config_session() -> tuple[fakes.FakeAiohttpSession, str, str]:
    """A FakeAiohttpSession serving one visu + one non-visu control.

    Returns the session plus the (str) UUIDs of the visu control (-> websocket
    bucket) and the non-visu control (-> grabber bucket).
    """
    room = loxone_xml.Room(title="Wohnzimmer")
    visu = loxone_xml.Control(
        type="InfoOnlyAnalog", title="Temp", analog=True, visu=True, room=room.uuid
    )
    nonvisu = loxone_xml.Control(
        type="InfoOnlyDigital", title="Fenster", room=room.uuid
    )
    xml = loxone_xml.build_config_xml(controls=[visu, nonvisu], rooms=[room])
    zip_bytes = loxcc.build_loxcc_zip(xml)

    session = fakes.FakeAiohttpSession()
    session.add_route(
        "/dev/fslist/prog/",
        fakes.FakeHTTPResponse(text="sps_0252_20240101120000.zip\n"),
    )
    session.add_route("/dev/fsget/prog/", fakes.FakeHTTPResponse(body=zip_bytes))
    return session, visu.uuid, nonvisu.uuid


@pytest.fixture
def _no_category_place():
    """Keep Category/Place elements out of the control set (as production does)."""
    get_config().control.type_blacklist = {"CATEGORY", "PLACE"}
    yield


# --------------------------------------------------------------------------- #
# init_gather_controls_from_miniserver                                        #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_init_gather_fills_controls_and_sets_event(
    mock_telegraf, monkeypatch, tmp_path, _no_category_place
):
    get_config().paths.data_dir = str(tmp_path)
    session, visu_uuid, nonvisu_uuid = _build_config_session()
    monkeypatch.setattr(
        "loxInFlux.miniserver.aiohttp.ClientSession", session.as_factory()
    )

    bridge = LoxInfluxBridge()
    assert not bridge.grabber_controls_updated.is_set()

    await asyncio.wait_for(
        bridge.init_gather_controls_from_miniserver(), timeout=_TIMEOUT
    )

    # Controls are populated and split into the right buckets.
    assert bridge.controls, "overall controls should be populated"
    assert visu_uuid.encode() in bridge.websocket_controls
    assert nonvisu_uuid.encode() in bridge.grabber_controls
    # The visu control is not a grabber control and vice versa.
    assert visu_uuid.encode() not in bridge.grabber_controls
    assert nonvisu_uuid.encode() not in bridge.websocket_controls
    # The gather is complete -> downstream tasks (grabber) may proceed.
    assert bridge.grabber_controls_updated.is_set()


@pytest.mark.asyncio
async def test_init_gather_applies_websocket_uuid_filter(
    mock_telegraf, monkeypatch, tmp_path, _no_category_place
):
    # The websocket bucket is passed through ``config.control.websocket`` before
    # being stored: a blacklisted UUID must be dropped from websocket_controls
    # while remaining in the overall controls set.
    get_config().paths.data_dir = str(tmp_path)
    session, visu_uuid, _nonvisu_uuid = _build_config_session()
    get_config().control.websocket = FilterConfig(uuid_blacklist=[visu_uuid])
    monkeypatch.setattr(
        "loxInFlux.miniserver.aiohttp.ClientSession", session.as_factory()
    )

    bridge = LoxInfluxBridge()
    await asyncio.wait_for(
        bridge.init_gather_controls_from_miniserver(), timeout=_TIMEOUT
    )

    assert visu_uuid.encode() in bridge.controls  # still in the overall set
    assert visu_uuid.encode() not in bridge.websocket_controls  # filtered out
    assert bridge.grabber_controls_updated.is_set()


# --------------------------------------------------------------------------- #
# shutdown                                                                    #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_shutdown_sets_event_and_closes_resources(mock_telegraf):
    bridge = LoxInfluxBridge()
    bridge.ws_client = _FakeLoxWs()

    await asyncio.wait_for(bridge.shutdown(), timeout=_TIMEOUT)

    assert bridge._shutdown_event.is_set()
    assert bridge.ws_client.stop_calls == 1
    assert mock_telegraf.closed


@pytest.mark.asyncio
async def test_shutdown_is_idempotent(mock_telegraf):
    bridge = LoxInfluxBridge()
    bridge.ws_client = _FakeLoxWs()

    await asyncio.wait_for(bridge.shutdown(), timeout=_TIMEOUT)
    # A second call must not raise and must leave the bridge shut down.
    await asyncio.wait_for(bridge.shutdown(), timeout=_TIMEOUT)

    assert bridge._shutdown_event.is_set()
    assert mock_telegraf.closed
    assert bridge.ws_client.stop_calls >= 1


@pytest.mark.asyncio
async def test_shutdown_survives_stop_and_close_errors(mock_telegraf, monkeypatch):
    # Fehlerfest: an exception in ws_client.stop() OR telegraf.close() must not
    # abort the rest of the cleanup (the bridge-owned task still gets cancelled).
    bridge = LoxInfluxBridge()
    bridge.ws_client = _FakeLoxWs(stop_error=RuntimeError("stop boom"))

    async def _boom_close() -> None:
        raise RuntimeError("close boom")

    monkeypatch.setattr(mock_telegraf, "close", _boom_close)

    async def _long() -> None:
        await asyncio.sleep(3600)

    owned = asyncio.create_task(_long())
    bridge._tasks.add(owned)

    # Must complete without propagating either failure.
    await asyncio.wait_for(bridge.shutdown(), timeout=_TIMEOUT)

    assert bridge._shutdown_event.is_set()
    assert bridge.ws_client.stop_calls == 1
    assert owned.cancelled()  # cleanup proceeded despite both errors


@pytest.mark.asyncio
async def test_shutdown_cancels_only_bridge_owned_tasks(mock_telegraf):
    bridge = LoxInfluxBridge()
    bridge.ws_client = _FakeLoxWs()

    async def _long() -> None:
        await asyncio.sleep(3600)

    owned = asyncio.create_task(_long())
    bridge._tasks.add(owned)
    foreign = asyncio.create_task(_long())  # NOT registered with the bridge

    try:
        await asyncio.wait_for(bridge.shutdown(), timeout=_TIMEOUT)

        assert owned.cancelled()  # bridge-owned task cancelled + awaited
        assert not foreign.cancelled()  # unrelated task left running
        assert not foreign.done()
    finally:
        foreign.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await foreign


# --------------------------------------------------------------------------- #
# udpate_controls (reconnect -> re-gather)                                    #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_update_controls_triggers_regather_after_events(mock_telegraf):
    bridge = LoxInfluxBridge()
    regathered = asyncio.Event()

    async def _spy() -> None:
        regathered.set()

    bridge.init_gather_controls_from_miniserver = _spy
    # Both preconditions satisfied -> re-gather is scheduled.
    bridge.ws_client_initialized.set()
    bridge.grabber_controls_updated.set()

    await asyncio.wait_for(bridge.udpate_controls(), timeout=_TIMEOUT)
    await asyncio.wait_for(regathered.wait(), timeout=_TIMEOUT)

    assert regathered.is_set()


@pytest.mark.asyncio
async def test_update_controls_waits_for_both_events(mock_telegraf):
    bridge = LoxInfluxBridge()
    called: list[bool] = []

    async def _spy() -> None:
        called.append(True)

    bridge.init_gather_controls_from_miniserver = _spy
    # Only one precondition set -> udpate_controls must block, never re-gather.
    bridge.ws_client_initialized.set()

    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(bridge.udpate_controls(), timeout=0.1)

    assert called == []


# --------------------------------------------------------------------------- #
# run_bridge (signal-driven shutdown + early-failure)                         #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_run_bridge_signal_shutdown_cancels_main(mock_telegraf):
    bridge = LoxInfluxBridge()
    main_started = asyncio.Event()
    main_cancelled = asyncio.Event()

    async def _main() -> None:
        main_started.set()
        try:
            await asyncio.sleep(3600)
        except asyncio.CancelledError:
            main_cancelled.set()
            raise

    bridge.main = _main

    runner = asyncio.create_task(run_bridge(bridge))
    try:
        await asyncio.wait_for(main_started.wait(), timeout=_TIMEOUT)
        # The loop's own signal handler (installed by run_bridge) intercepts
        # this; it sets the shutdown event rather than terminating the process.
        os.kill(os.getpid(), signal.SIGTERM)

        await asyncio.wait_for(runner, timeout=_TIMEOUT)
    finally:
        if not runner.done():
            runner.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await runner

    assert main_cancelled.is_set()  # main_task was cancelled on shutdown
    assert runner.exception() is None  # CancelledError suppressed cleanly


@pytest.mark.asyncio
async def test_run_bridge_early_main_failure_does_not_hang(mock_telegraf):
    bridge = LoxInfluxBridge()

    async def _main() -> None:
        raise RuntimeError("early boom")

    bridge.main = _main

    # Without the FIRST_COMPLETED wait, run_bridge would block on the (never
    # set) shutdown event forever. It must instead return promptly and surface
    # the failure (main() catches RuntimeError upstream and still runs cleanup).
    with pytest.raises(RuntimeError, match="early boom"):
        await asyncio.wait_for(run_bridge(bridge), timeout=_TIMEOUT)


# --------------------------------------------------------------------------- #
# handle_value_states adoption path                                           #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_value_state_adopts_unknown_uuid_present_in_overall_controls(
    mock_telegraf, sample_controls, caplog
):
    # A UUID absent from the initial websocket bucket but present in the overall
    # controls is adopted into websocket_controls (with a warning) and written.
    bridge = LoxInfluxBridge()
    bridge.controls = dict(sample_controls.controls)
    bridge.websocket_controls = {}

    uuid = next(iter(sample_controls.controls))
    assert uuid not in bridge.websocket_controls

    with caplog.at_level(logging.WARNING, logger="loxInFlux.app"):
        await bridge.handle_value_states({uuid: 3.0}, 2)
        pending = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
        if pending:
            await asyncio.wait_for(asyncio.gather(*pending), timeout=_TIMEOUT)

    assert uuid in bridge.websocket_controls  # adopted
    assert len(mock_telegraf.writes) == 1
    assert any(
        "adding it to websocket controls" in record.getMessage()
        for record in caplog.records
    )


@pytest.mark.asyncio
async def test_value_state_ignores_uuid_absent_everywhere(
    mock_telegraf, sample_controls
):
    bridge = LoxInfluxBridge()
    bridge.controls = dict(sample_controls.controls)
    bridge.websocket_controls = {}

    await bridge.handle_value_states({b"nope-not-anywhere": 1.0}, 2)
    pending = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
    if pending:
        await asyncio.wait_for(asyncio.gather(*pending), timeout=_TIMEOUT)

    assert mock_telegraf.writes == []
    assert b"nope-not-anywhere" not in bridge.websocket_controls


# --------------------------------------------------------------------------- #
# helpers                                                                     #
# --------------------------------------------------------------------------- #
def _patch_loxwebsocket(monkeypatch, fake: _FakeLoxWs) -> None:
    """Swap the ``loxwebsocket`` singleton that ``app.init_websocket_connection``
    binds via ``self.ws_client = loxwebsocket``.

    Resolve the module object via ``sys.modules`` to patch its module-level
    ``loxwebsocket`` reference (as the shared fixtures do).
    """
    monkeypatch.setattr(sys.modules["loxInFlux.app"], "loxwebsocket", fake)


def _patch_grabber(monkeypatch, cls) -> None:
    """Replace ``LoxoneGrabber`` as seen by ``app.init_grabber``."""
    monkeypatch.setattr(sys.modules["loxInFlux.app"], "LoxoneGrabber", cls)


async def _cancel(task: asyncio.Task) -> None:
    """Best-effort teardown of a spawned task.

    If the task already finished (possibly with an exception the test already
    asserted on), just retrieve its result so asyncio does not warn about a
    never-retrieved exception - re-awaiting would re-raise it here.
    """
    if task.done():
        with contextlib.suppress(BaseException):
            task.exception()
        return
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
        await task


# --------------------------------------------------------------------------- #
# init_websocket_connection                                                   #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_ws_connection_happy_path_registers_callbacks_and_signals(
    mock_telegraf, monkeypatch
):
    # The CONNECTED path: connect() with the configured credentials, register the
    # value-state (type 2) message callback and the RECONNECTED event callback,
    # flip ws_client_initialized, then block on the shutdown event. Setting it
    # lets the coroutine drain and stop() runs exactly once in the finally.
    fake = _FakeLoxWs()
    _patch_loxwebsocket(monkeypatch, fake)
    bridge = LoxInfluxBridge()

    task = asyncio.create_task(bridge.init_websocket_connection())
    try:
        await asyncio.wait_for(bridge.ws_client_initialized.wait(), timeout=_TIMEOUT)

        assert bridge.ws_client is fake
        assert fake.connect_kwargs == {
            "user": get_config().miniserver.user,
            "password": get_config().miniserver.password,
            "loxone_url": bridge.base_url,
            "max_reconnect_attempts": get_config().miniserver.max_reconnect_attempts,
        }
        assert (bridge.handle_value_states, [2]) in fake.message_callbacks
        assert (
            bridge.udpate_controls,
            [LoxWs.EventType.RECONNECTED],
        ) in fake.event_callbacks

        # Still blocked on the shutdown event until we set it.
        assert not task.done()
        bridge._shutdown_event.set()
        await asyncio.wait_for(task, timeout=_TIMEOUT)
        assert fake.stop_calls == 1
    finally:
        await _cancel(task)


@pytest.mark.asyncio
async def test_ws_connection_loxone_exception_raises_runtime_error_and_stops(
    mock_telegraf, monkeypatch
):
    # Reconnect exhaustion: the real client raises LoxoneException once all
    # reconnect attempts fail. The bridge must translate it into a RuntimeError,
    # never flip ws_client_initialized, and still stop() in the finally.
    fake = _FakeLoxWs(
        connect_error=LoxoneException("All reconnection attempts failed.")
    )
    _patch_loxwebsocket(monkeypatch, fake)
    bridge = LoxInfluxBridge()

    with pytest.raises(RuntimeError, match="Failed to establish or maintain"):
        await asyncio.wait_for(bridge.init_websocket_connection(), timeout=_TIMEOUT)

    assert not bridge.ws_client_initialized.is_set()
    assert fake.stop_calls == 1


@pytest.mark.asyncio
async def test_ws_connection_stop_error_in_finally_propagates(
    mock_telegraf, monkeypatch
):
    # If stop() raises during the finally cleanup, the error is not swallowed:
    # init_websocket_connection has no guard around the finally stop().
    fake = _FakeLoxWs(stop_error=RuntimeError("stop boom"))
    _patch_loxwebsocket(monkeypatch, fake)
    bridge = LoxInfluxBridge()

    task = asyncio.create_task(bridge.init_websocket_connection())
    try:
        await asyncio.wait_for(bridge.ws_client_initialized.wait(), timeout=_TIMEOUT)
        bridge._shutdown_event.set()
        with pytest.raises(RuntimeError, match="stop boom"):
            await asyncio.wait_for(task, timeout=_TIMEOUT)
        assert fake.stop_calls == 1
    finally:
        await _cancel(task)


@pytest.mark.asyncio
async def test_ws_connection_cancelled_during_startup_still_stops(
    mock_telegraf, monkeypatch
):
    # Cancellation while parked on the shutdown event (e.g. run_bridge cancels
    # main() on a signal) must still run the finally cleanup: stop() once.
    fake = _FakeLoxWs()
    _patch_loxwebsocket(monkeypatch, fake)
    bridge = LoxInfluxBridge()

    task = asyncio.create_task(bridge.init_websocket_connection())
    await asyncio.wait_for(bridge.ws_client_initialized.wait(), timeout=_TIMEOUT)

    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
        await asyncio.wait_for(task, timeout=_TIMEOUT)

    assert fake.stop_calls == 1


# --------------------------------------------------------------------------- #
# init_grabber                                                                #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_init_grabber_registers_text_callback_and_starts_when_events_set(
    mock_telegraf, monkeypatch
):
    # With both preconditions satisfied, init_grabber registers the text-message
    # (type 0) callback, records the handler, and schedules grabber.start with
    # the bridge's get_controls as a bridge-owned task.
    _patch_grabber(monkeypatch, _FakeGrabber)
    bridge = LoxInfluxBridge()
    bridge.ws_client = _FakeLoxWs()
    bridge.ws_client_initialized.set()
    bridge.grabber_controls_updated.set()

    await asyncio.wait_for(bridge.init_grabber(), timeout=_TIMEOUT)
    try:
        assert isinstance(bridge.grabber, _FakeGrabber)
        assert (bridge.handle_text_messages, [0]) in bridge.ws_client.message_callbacks
        assert bridge._message_handlers[0] == bridge.handle_text_messages

        await asyncio.wait_for(bridge.grabber.started.wait(), timeout=_TIMEOUT)
        assert bridge.grabber.start_calls == [bridge.get_controls]
        # The still-running grabber task is tracked as bridge-owned.
        assert any(not t.done() for t in bridge._tasks)
    finally:
        bridge.grabber._release.set()
        for t in list(bridge._tasks):
            await _cancel(t)


@pytest.mark.asyncio
async def test_init_grabber_blocks_until_both_events_set(mock_telegraf, monkeypatch):
    # The callback registration happens up front, but the grabber must not start
    # until BOTH ws_client_initialized and grabber_controls_updated are set.
    _patch_grabber(monkeypatch, _FakeGrabber)
    bridge = LoxInfluxBridge()
    bridge.ws_client = _FakeLoxWs()
    bridge.ws_client_initialized.set()  # only one precondition

    task = asyncio.create_task(bridge.init_grabber())
    try:
        # Let it register the callback and reach the ALL_COMPLETED wait.
        for _ in range(5):
            await asyncio.sleep(0)

        assert not task.done()  # blocked: second event still unset
        assert (bridge.handle_text_messages, [0]) in bridge.ws_client.message_callbacks
        assert bridge.grabber.start_calls == []  # grabber not started yet

        # Releasing the second event lets init_grabber finish + start the grabber.
        bridge.grabber_controls_updated.set()
        await asyncio.wait_for(task, timeout=_TIMEOUT)
        await asyncio.wait_for(bridge.grabber.started.wait(), timeout=_TIMEOUT)
        assert bridge.grabber.start_calls == [bridge.get_controls]
    finally:
        if isinstance(bridge.grabber, _FakeGrabber):
            bridge.grabber._release.set()
        await _cancel(task)
        for t in list(bridge._tasks):
            await _cancel(t)


# --------------------------------------------------------------------------- #
# main (orchestration)                                                        #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_main_gathers_all_subsystems_and_blocks_until_shutdown(mock_telegraf):
    # main() gathers telegraf.initialize + the three init_* coroutines, then
    # parks on the shutdown event. Replace the three inits with spies (telegraf
    # is the recording fake) and assert all four ran and main stays blocked.
    bridge = LoxInfluxBridge()
    calls: list[str] = []
    gathered = asyncio.Event()

    def _mark(name: str) -> None:
        calls.append(name)
        if len(calls) == 3:  # all three init_* spies have run
            gathered.set()

    async def _gather_spy() -> None:
        _mark("gather")

    async def _ws_spy() -> None:
        _mark("ws")

    async def _grabber_spy() -> None:
        _mark("grabber")

    bridge.init_gather_controls_from_miniserver = _gather_spy
    bridge.init_websocket_connection = _ws_spy
    bridge.init_grabber = _grabber_spy

    task = asyncio.create_task(bridge.main())
    try:
        await asyncio.wait_for(gathered.wait(), timeout=_TIMEOUT)
        assert set(calls) == {"gather", "ws", "grabber"}
        # telegraf.initialize is the first coroutine in the gather, so it has
        # already completed by the time the spies run.
        assert mock_telegraf.initialized

        # All subsystems done, but main() blocks on the shutdown event.
        await asyncio.sleep(0)
        assert not task.done()

        bridge._shutdown_event.set()
        await asyncio.wait_for(task, timeout=_TIMEOUT)
    finally:
        await _cancel(task)


@pytest.mark.asyncio
async def test_main_propagates_subsystem_failure(mock_telegraf):
    # A startup failure in any gathered subsystem must propagate out of main()
    # (asyncio.gather re-raises the first exception) and never reach the final
    # shutdown wait.
    bridge = LoxInfluxBridge()

    async def _ok() -> None:
        return None

    async def _boom() -> None:
        raise RuntimeError("startup boom")

    bridge.init_gather_controls_from_miniserver = _ok
    bridge.init_websocket_connection = _boom
    bridge.init_grabber = _ok

    with pytest.raises(RuntimeError, match="startup boom"):
        await asyncio.wait_for(bridge.main(), timeout=_TIMEOUT)

    assert not bridge._shutdown_event.is_set()


# --------------------------------------------------------------------------- #
# main (full wiring: CONNECTED end-to-end)                                    #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_main_full_wiring_connects_gathers_and_shuts_down(
    mock_telegraf, monkeypatch, tmp_path, _no_category_place
):
    # Capstone: run the real main() with faked external I/O only (loxwebsocket,
    # the HTTP config download, telegraf) and the grabber disabled so its poll
    # loop returns early. Asserts the whole orchestration converges: controls
    # gathered + bucketed, websocket connected + callbacks registered, telegraf
    # initialized, and a clean shutdown that stops the websocket once.
    get_config().paths.data_dir = str(tmp_path)
    get_config().general.grabber = False  # real grabber returns before its poll loop
    session, visu_uuid, nonvisu_uuid = _build_config_session()
    monkeypatch.setattr(
        "loxInFlux.miniserver.aiohttp.ClientSession", session.as_factory()
    )
    fake = _FakeLoxWs()
    _patch_loxwebsocket(monkeypatch, fake)

    bridge = LoxInfluxBridge()
    task = asyncio.create_task(bridge.main())
    try:
        await asyncio.wait_for(bridge.ws_client_initialized.wait(), timeout=_TIMEOUT)
        await asyncio.wait_for(bridge.grabber_controls_updated.wait(), timeout=_TIMEOUT)

        # Controls gathered and split into the right buckets.
        assert bridge.controls
        assert visu_uuid.encode() in bridge.websocket_controls
        assert nonvisu_uuid.encode() in bridge.grabber_controls
        # Websocket connected + callbacks registered.
        assert fake.connect_kwargs is not None
        assert (bridge.handle_value_states, [2]) in fake.message_callbacks
        assert (
            bridge.udpate_controls,
            [LoxWs.EventType.RECONNECTED],
        ) in fake.event_callbacks
        assert mock_telegraf.initialized

        # A single shutdown event releases both init_websocket_connection and
        # main()'s own wait; main() then returns and the websocket is stopped.
        bridge._shutdown_event.set()
        await asyncio.wait_for(task, timeout=_TIMEOUT)
        assert fake.stop_calls == 1
    finally:
        await _cancel(task)
        for t in list(bridge._tasks):
            await _cancel(t)
