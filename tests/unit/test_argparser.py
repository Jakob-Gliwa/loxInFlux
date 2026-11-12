"""Unit tests for :mod:`loxInFlux.argparser`.

``get_args`` lazily builds its parser and caches the parsed ``Namespace`` in the
module-level ``_args`` global (adding the arguments to the module-level
``_parser`` on first call). The conftest bootstrap has already primed both, so
each test swaps in a *fresh* parser + cleared cache (restored on teardown) and
drives ``sys.argv`` explicitly.
"""

from __future__ import annotations

import argparse
import sys

import pytest

import loxInFlux.argparser as argparser_mod


@pytest.fixture
def fresh_argparser(monkeypatch):
    """Reset argparser module state so ``get_args`` re-parses from scratch.

    A fresh ``ArgumentParser`` avoids "conflicting option string" errors from
    re-adding arguments to the already-populated bootstrap parser, and clearing
    ``_args`` forces a re-parse. ``monkeypatch`` restores both after the test.
    """
    monkeypatch.setattr(
        argparser_mod, "_parser", argparse.ArgumentParser(description="LoxInFlux")
    )
    monkeypatch.setattr(argparser_mod, "_args", None)
    return argparser_mod


def test_get_args_defaults_to_none(fresh_argparser, monkeypatch):
    monkeypatch.setattr(sys, "argv", ["loxinflux"])

    args = fresh_argparser.get_args()

    assert args.config_dir is None
    assert args.data_dir is None
    assert args.log_level is None


def test_get_args_parses_explicit_config_and_data_dir(fresh_argparser, monkeypatch):
    monkeypatch.setattr(
        sys,
        "argv",
        [
            "loxinflux",
            "--config-dir",
            "/tmp/my-config",
            "--data-dir",
            "/tmp/my-data",
            "--log-level",
            "DEBUG",
        ],
    )

    args = fresh_argparser.get_args()

    assert args.config_dir == "/tmp/my-config"
    assert args.data_dir == "/tmp/my-data"
    assert args.log_level == "DEBUG"


def test_get_args_rejects_invalid_log_level(fresh_argparser, monkeypatch):
    monkeypatch.setattr(sys, "argv", ["loxinflux", "--log-level", "TRACE"])

    with pytest.raises(SystemExit):
        fresh_argparser.get_args()


def test_get_args_caches_first_parse(fresh_argparser, monkeypatch):
    monkeypatch.setattr(sys, "argv", ["loxinflux", "--config-dir", "/tmp/first"])
    first = fresh_argparser.get_args()

    # A later argv change must NOT be re-read: the result is cached in ``_args``.
    monkeypatch.setattr(sys, "argv", ["loxinflux", "--config-dir", "/tmp/second"])
    second = fresh_argparser.get_args()

    assert first is second
    assert second.config_dir == "/tmp/first"
