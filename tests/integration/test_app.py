"""Integration tests for the message handlers on :class:`LoxInfluxBridge`.

``handle_value_states`` (websocket value states, type 2) and
``handle_text_messages`` (grabber text messages, type 0) both funnel points to
the Telegraf writer returned by ``get_telegraf()``, which ``mock_telegraf``
replaces with a recording fake. Controls come from the ``sample_controls``
fixture (keys are UUID bytes).

``handle_value_states`` awaits each write inline (backpressure, no orphan
tasks); ``_drain_tasks`` remains as a defensive no-op for any stray tasks.

Line-protocol assertions here compare the **exact** emitted bytes (not
substrings / counts): the value-state path is ``point_websocket`` + the
formatted value + a space-prefixed nanosecond timestamp, and the text/grabber
path is the ``pointInflux`` template's ``to_line_protocol()`` after tagging
``source=grabber`` and filling the fields. ``time.time_ns`` is frozen so the
timestamp is deterministic.
"""

from __future__ import annotations

import asyncio
import sys
from copy import deepcopy

import orjson
import pytest

from loxInFlux.app import LoxInfluxBridge

# Frozen wall-clock (ns) so timestamped line-protocol output is deterministic.
_FIXED_NS = 1_700_000_000_123_456_789


def _freeze_time_ns(monkeypatch) -> None:
    """Pin ``time.time_ns()`` as seen by ``loxInFlux.app`` to ``_FIXED_NS``."""
    app_mod = sys.modules["loxInFlux.app"]
    monkeypatch.setattr(app_mod.time, "time_ns", lambda: _FIXED_NS)


async def _drain_tasks() -> None:
    """Await any tasks the handler spawned via ``asyncio.create_task``."""
    pending = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
    if pending:
        await asyncio.gather(*pending)


def _make_bridge(sample_controls) -> LoxInfluxBridge:
    bridge = LoxInfluxBridge()
    bridge.controls = dict(sample_controls.controls)
    bridge.websocket_controls = dict(sample_controls.websocket_controls)
    bridge.grabber_controls = dict(sample_controls.grabber_controls)
    return bridge


# --------------------------------------------------------------------------- #
# handle_value_states                                                          #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_value_state_writes_point_for_known_websocket_control(
    mock_telegraf, sample_controls, monkeypatch
):
    _freeze_time_ns(monkeypatch)
    bridge = _make_bridge(sample_controls)
    uuid = next(iter(sample_controls.websocket_controls))
    prefix = sample_controls.websocket_controls[uuid]["point_websocket"]

    await bridge.handle_value_states({uuid: 42.0}, 2)
    await _drain_tasks()

    # Exact bytes: point_websocket + formatted value + " " + ns-timestamp.
    # round_floats is off in the bootstrap config -> 15-decimal formatting.
    expected = prefix + b"42.000000000000000" + b" " + orjson.dumps(_FIXED_NS)
    assert mock_telegraf.writes == [expected]


@pytest.mark.asyncio
async def test_value_state_promotes_control_from_overall_set(
    mock_telegraf, sample_controls, monkeypatch
):
    _freeze_time_ns(monkeypatch)
    bridge = _make_bridge(sample_controls)
    # A control present in the overall set but not the initial websocket bucket.
    uuid = next(iter(sample_controls.grabber_controls))
    assert uuid not in bridge.websocket_controls
    prefix = sample_controls.grabber_controls[uuid]["point_websocket"]

    await bridge.handle_value_states({uuid: 1.0}, 2)
    await _drain_tasks()

    assert uuid in bridge.websocket_controls  # promoted on first sight
    expected = prefix + b"1.000000000000000" + b" " + orjson.dumps(_FIXED_NS)
    assert mock_telegraf.writes == [expected]


@pytest.mark.asyncio
async def test_value_state_omits_unknown_uuid(mock_telegraf, sample_controls):
    bridge = _make_bridge(sample_controls)

    await bridge.handle_value_states({b"totally-unknown-uuid": 5.0}, 2)
    await _drain_tasks()

    assert mock_telegraf.writes == []


@pytest.mark.asyncio
async def test_value_state_writes_all_known_entries(mock_telegraf, sample_controls):
    bridge = _make_bridge(sample_controls)
    known = list(sample_controls.websocket_controls)[:2]
    message = {uuid: float(i) for i, uuid in enumerate(known)}

    await bridge.handle_value_states(message, 2)
    await _drain_tasks()

    assert len(mock_telegraf.writes) == len(known)


@pytest.mark.asyncio
async def test_value_states_await_writes_without_orphan_tasks(
    mock_telegraf, sample_controls
):
    # Regression (Bug 4): writes are awaited inline, so after the handler returns
    # every point is already recorded (no draining needed) and NO untracked
    # background write task was spawned ("task exception was never retrieved").
    bridge = _make_bridge(sample_controls)
    known = list(sample_controls.websocket_controls)
    message = {uuid: float(i) for i, uuid in enumerate(known)}

    before = {t for t in asyncio.all_tasks() if not t.done()}
    await bridge.handle_value_states(message, 2)

    # No draining: all writes are present immediately after the await returns.
    assert len(mock_telegraf.writes) == len(known)

    # The handler must not have left any orphan (untracked) write tasks behind.
    leftover = {
        t
        for t in asyncio.all_tasks()
        if not t.done() and t is not asyncio.current_task()
    }
    assert leftover <= before


@pytest.mark.asyncio
async def test_value_states_slow_writer_preserves_order(sample_controls, monkeypatch):
    # A slow writer must throttle (backpressure) and preserve per-message order;
    # because writes are awaited sequentially they cannot interleave.
    order: list[bytes] = []

    class _SlowWriter:
        async def write(self, point: bytes) -> None:
            await asyncio.sleep(0)  # yield: a fire-and-forget design could reorder
            order.append(point)

    telegraf_mod = sys.modules["loxInFlux.telegraf"]
    monkeypatch.setattr(telegraf_mod, "_writer", _SlowWriter())

    bridge = _make_bridge(sample_controls)
    known = list(sample_controls.websocket_controls)
    message = {uuid: float(i) for i, uuid in enumerate(known)}

    await bridge.handle_value_states(message, 2)

    assert len(order) == len(known)
    # Each recorded point carries the websocket prefix of the matching control,
    # in the same order the message iterated.
    expected_prefixes = [
        sample_controls.websocket_controls[uuid]["point_websocket"] for uuid in known
    ]
    for point, prefix in zip(order, expected_prefixes, strict=True):
        assert point.startswith(prefix)


# --------------------------------------------------------------------------- #
# handle_text_messages                                                         #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_text_message_writes_for_known_grabber_control(
    mock_telegraf, sample_controls
):
    bridge = _make_bridge(sample_controls)
    uuid = next(iter(sample_controls.grabber_controls))
    control = sample_controls.grabber_controls[uuid]

    await bridge.handle_text_messages({uuid: {"value": "23.5"}}, 0)

    # Exact line-protocol: fresh copy of the template, source=grabber, the
    # numeric-coerced Default value and (with no outputs) no trailing timestamp.
    expected = (
        deepcopy(control["pointInflux"])
        .tag("source", "grabber")
        .field("Default", 23.5)
        .to_line_protocol()
        .encode()
    )
    assert mock_telegraf.writes == [expected]


@pytest.mark.asyncio
async def test_text_message_with_additional_outputs(
    mock_telegraf, sample_controls, monkeypatch
):
    _freeze_time_ns(monkeypatch)
    bridge = _make_bridge(sample_controls)
    uuid = next(iter(sample_controls.grabber_controls))
    control = sample_controls.grabber_controls[uuid]

    message = {
        uuid: {
            "value": "10",
            "output0": {"name": "Power", "value": "5"},
        }
    }
    await bridge.handle_text_messages(message, 0)

    # Exact line-protocol: Default + the named output field (both int-coerced ->
    # "i" suffix) and the frozen ns timestamp added by the multi-output branch.
    expected = (
        deepcopy(control["pointInflux"])
        .tag("source", "grabber")
        .field("Default", 10)
        .field("Power", 5)
        .time(_FIXED_NS)
        .to_line_protocol()
        .encode()
    )
    assert mock_telegraf.writes == [expected]


@pytest.mark.asyncio
async def test_text_message_omits_unknown_grabber_control(
    mock_telegraf, sample_controls
):
    bridge = _make_bridge(sample_controls)

    await bridge.handle_text_messages({b"unknown": {"value": "1"}}, 0)

    assert mock_telegraf.writes == []


@pytest.mark.asyncio
async def test_text_message_ignores_multi_entry_message(mock_telegraf, sample_controls):
    bridge = _make_bridge(sample_controls)

    await bridge.handle_text_messages({b"a": {"value": "1"}, b"b": {"value": "2"}}, 0)

    assert mock_telegraf.writes == []


# --------------------------------------------------------------------------- #
# A3: per-message copy of the shared/cached Point template                     #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_text_message_does_not_accumulate_output_fields_across_calls(
    mock_telegraf, sample_controls
):
    # Regression for the shared-Point bug: two grabber messages for the SAME
    # control carrying *different* named outputs must not leak the first
    # message's field into the second line-protocol line. Before the deepcopy
    # fix the handler mutated the cached ``pointInflux`` in place, so "Alpha"
    # accumulated onto the second ("Beta") write.
    bridge = _make_bridge(sample_controls)
    uuid = next(iter(sample_controls.grabber_controls))

    await bridge.handle_text_messages(
        {uuid: {"value": "1", "output0": {"name": "Alpha", "value": "10"}}}, 0
    )
    await bridge.handle_text_messages(
        {uuid: {"value": "2", "output0": {"name": "Beta", "value": "20"}}}, 0
    )

    assert len(mock_telegraf.writes) == 2
    first, second = mock_telegraf.writes
    assert b"Alpha" in first and b"Beta" not in first
    # The crux: the second message must be free of the first message's field.
    assert b"Beta" in second and b"Alpha" not in second


@pytest.mark.asyncio
async def test_grabber_handler_does_not_poison_control_cache(
    mock_telegraf, sample_controls
):
    # ``getControlsFromConfigXML`` is lru_cached and returns the *same* object
    # graph on repeated calls, so caller mutation of a control's ``pointInflux``
    # would poison the cache for every subsequent consumer. Assert both facts:
    # (1) the cache hands back the identical object, and (2) running the handler
    # leaves the shared template Point untouched (the deepcopy fix).
    from loxInFlux.miniserver import getControlsFromConfigXML

    first = getControlsFromConfigXML(sample_controls.xml)
    second = getControlsFromConfigXML(sample_controls.xml)
    assert first is second  # lru_cache returns the identical object graph

    bridge = _make_bridge(sample_controls)
    uuid = next(iter(sample_controls.grabber_controls))
    shared_point = sample_controls.grabber_controls[uuid]["pointInflux"]
    before = shared_point.to_line_protocol()

    await bridge.handle_text_messages(
        {uuid: {"value": "7", "output0": {"name": "Leak", "value": "9"}}}, 0
    )

    # The per-control template the lru_cache hands back must be pristine: no
    # source=grabber tag, no leaked "Leak" field, identical serialization.
    assert shared_point.to_line_protocol() == before
    assert b"Leak" not in shared_point.to_line_protocol().encode()
    assert b"source=grabber" not in shared_point.to_line_protocol().encode()
