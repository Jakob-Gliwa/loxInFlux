"""Public-API contract tests for the ``loxInFlux`` package.

Guards three things that break silently otherwise:

* every name advertised in ``loxInFlux.__all__`` is actually importable,
* ``loxInFlux.__version__`` stays in sync with the installed package metadata
  (drift guard - currently both ``"0.3.0"``),
* ``create_telegraf_writer`` returns the writer type selected by
  ``config.telegraf.protocol``.
"""

from __future__ import annotations

import importlib
import io
from importlib import metadata

import pytest

import loxInFlux
from loxInFlux.config import get_config
from loxInFlux.telegraf import (
    ExecDTelegrafWriter,
    MQTTTelegrafWriter,
    TCPTelegrafWriter,
    UDPTelegrafWriter,
    create_telegraf_writer,
)


def test_all_names_are_importable():
    """Every symbol in ``__all__`` must resolve on the package object."""
    assert loxInFlux.__all__, "__all__ should not be empty"
    missing = [name for name in loxInFlux.__all__ if not hasattr(loxInFlux, name)]
    assert not missing, f"names in __all__ not importable: {missing}"


def test_all_names_are_unique():
    assert len(loxInFlux.__all__) == len(set(loxInFlux.__all__))


def test_all_exports_exact_set():
    """Pin the exact public export surface (regression guard).

    Any accidental add/remove in ``loxInFlux.__all__`` fails here. Update this
    set only for an *intentional* public-API change.
    """
    assert set(loxInFlux.__all__) == {
        "LoxInfluxBridge",
        "get_config",
        "init_config",
        "get_lazy_logger",
        "configure_logging",
        "initialize_logging",
        "LazyLogger",
        "main",
        "log_performance",
        "get_telegraf",
        "init_telegraf",
    }


def test_version_matches_package_metadata():
    """Drift guard: hard-coded ``__version__`` must equal the dist metadata."""
    assert loxInFlux.__version__ == metadata.version("loxInFlux")


class _DummyStdout(io.TextIOBase):
    """Stand-in for ``sys.stdout`` that supports ``reconfigure`` + ``buffer``.

    ``ExecDTelegrafWriter.__init__`` calls ``sys.stdout.reconfigure(...)``, which
    pytest's captured stream may not expose; this keeps construction side-effect
    free during the factory contract test.
    """

    buffer = io.BytesIO()

    def reconfigure(self, **_kwargs):
        return None


@pytest.mark.parametrize(
    ("protocol", "expected_type"),
    [
        ("udp", UDPTelegrafWriter),
        ("UDP", UDPTelegrafWriter),
        ("tcp", TCPTelegrafWriter),
        ("execd", ExecDTelegrafWriter),
        ("mqtt", MQTTTelegrafWriter),
        ("something-unknown", UDPTelegrafWriter),  # falls back to UDP
    ],
)
def test_create_telegraf_writer_returns_expected_type(
    protocol, expected_type, monkeypatch
):
    get_config().telegraf.protocol = protocol
    # ExecDTelegrafWriter.__init__ calls sys.stdout.reconfigure(...); pytest's
    # captured stream may lack it, so swap in a compatible dummy for the ctor.
    telegraf_mod = importlib.import_module("loxInFlux.telegraf")
    monkeypatch.setattr(telegraf_mod.sys, "stdout", _DummyStdout())

    writer = create_telegraf_writer()

    assert type(writer) is expected_type
    assert isinstance(writer, telegraf_mod.TelegrafWriter)
