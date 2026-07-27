"""Unit tests for ``loxInFlux.logger``.

Covers ``LazyLogger`` level gating + ``setLevel`` cache refresh, ``TelegrafFormatter``
level prefixes, ``configure_logging`` level-source priority, and ``get_lazy_logger``
caching / auto-configuration.
"""

from __future__ import annotations

import logging

import loxInFlux.logger as logger_mod
from loxInFlux.config import get_config
from loxInFlux.logger import (
    TRACE_LEVEL,
    LazyLogger,
    TelegrafFormatter,
    configure_logging,
    get_lazy_logger,
    initialize_logging,
)


class _RecordingHandler(logging.Handler):
    def __init__(self):
        super().__init__()
        self.records: list[logging.LogRecord] = []

    def emit(self, record):
        self.records.append(record)


def _isolated_logger(name: str, level: int) -> tuple[logging.Logger, _RecordingHandler]:
    lg = logging.getLogger(name)
    lg.handlers.clear()
    lg.propagate = False
    handler = _RecordingHandler()
    lg.addHandler(handler)
    lg.setLevel(level)
    return lg, handler


# --------------------------------------------------------------------------- #
# LazyLogger gating / setLevel                                                 #
# --------------------------------------------------------------------------- #


def test_lazy_logger_debug_gated_when_disabled():
    lg, handler = _isolated_logger("test.lazy.debug", logging.WARNING)
    lazy = LazyLogger(lg)
    assert lazy._debug_enabled is False
    lazy.debug("should not appear")
    assert handler.records == []


def test_lazy_logger_setlevel_refreshes_cache_and_emits():
    lg, handler = _isolated_logger("test.lazy.setlevel", logging.WARNING)
    lazy = LazyLogger(lg)
    lazy.setLevel(logging.DEBUG)
    assert lazy._debug_enabled is True
    lazy.debug("now visible")
    assert any(r.getMessage() == "now visible" for r in handler.records)


def test_lazy_logger_trace_gating():
    lg, handler = _isolated_logger("test.lazy.trace", TRACE_LEVEL)
    lazy = LazyLogger(lg)
    assert lazy._trace_enabled is True
    lazy.trace("traced")
    assert any(r.getMessage() == "traced" for r in handler.records)
    # Raising the level above TRACE gates trace out.
    lazy.setLevel(logging.DEBUG)
    assert lazy._trace_enabled is False
    handler.records.clear()
    lazy.trace("gone")
    assert handler.records == []


def test_lazy_logger_info_always_emits_regardless_of_debug_cache():
    lg, handler = _isolated_logger("test.lazy.info", logging.INFO)
    lazy = LazyLogger(lg)
    lazy.info("hello")
    assert any(r.getMessage() == "hello" for r in handler.records)


def test_lazy_logger_level_property_and_isenabledfor():
    lg, _ = _isolated_logger("test.lazy.prop", logging.ERROR)
    lazy = LazyLogger(lg)
    assert lazy.level == logging.ERROR
    assert lazy.isEnabledFor(logging.ERROR) is True
    assert lazy.isEnabledFor(logging.DEBUG) is False


# --------------------------------------------------------------------------- #
# TelegrafFormatter                                                            #
# --------------------------------------------------------------------------- #


def _format(level: int) -> str:
    fmt = TelegrafFormatter("%(message)s")
    record = logging.LogRecord("n", level, "path", 1, "msg", None, None)
    return fmt.format(record)


def test_telegraf_formatter_level_prefixes():
    assert _format(logging.ERROR) == "E! msg"
    assert _format(logging.WARNING) == "W! msg"
    assert _format(logging.INFO) == "I! msg"
    assert _format(logging.DEBUG) == "D! msg"
    assert _format(TRACE_LEVEL) == "T! msg"


def test_telegraf_formatter_unknown_level_defaults_to_error_prefix():
    assert _format(logging.CRITICAL) == "E! msg"
    assert _format(99) == "E! msg"


# --------------------------------------------------------------------------- #
# configure_logging priority                                                   #
# --------------------------------------------------------------------------- #


def test_configure_logging_explicit_level_wins():
    configure_logging(level="ERROR", force=True)
    assert logging.getLogger().level == logging.ERROR


def test_initialize_logging_applies_cli_log_level_debug():
    """A real ``loxInFlux --log-level DEBUG`` startup ends up at DEBUG level.

    ``main()`` owns argument parsing and forwards the parsed level, so the CLI
    path is reproduced by passing the level explicitly - no ``sys.argv`` or
    ``get_args()`` involvement.
    """
    initialize_logging(log_level="DEBUG")
    assert logging.getLogger().level == logging.DEBUG


def test_initialize_logging_cli_level_overrides_config_level():
    """An explicit CLI level wins over the configured level."""
    get_config().logging.level = "WARNING"

    initialize_logging(log_level="debug")

    assert logging.getLogger().level == logging.DEBUG


def test_configure_logging_env_priority(monkeypatch):
    monkeypatch.setenv("APP_LOG_LEVEL", "ERROR")
    configure_logging(force=True)
    assert logging.getLogger().level == logging.ERROR


def test_configure_logging_falls_back_to_config_level(monkeypatch):
    monkeypatch.delenv("APP_LOG_LEVEL", raising=False)
    # Bootstrap config sets logging.level = "INFO".
    configure_logging(force=True)
    assert logging.getLogger().level == logging.INFO


def test_configure_logging_uses_telegraf_formatter_when_requested():
    configure_logging(level="INFO", use_telegraf_format=True, force=True)
    root = logging.getLogger()
    assert any(isinstance(h.formatter, TelegrafFormatter) for h in root.handlers)


def test_configure_logging_noop_when_already_configured(monkeypatch):
    logger_mod._configured = True
    root = logging.getLogger()
    root.setLevel(logging.CRITICAL)
    # No force -> early return, level untouched.
    configure_logging(level="DEBUG")
    assert root.level == logging.CRITICAL


# --------------------------------------------------------------------------- #
# get_lazy_logger caching / auto-config                                        #
# --------------------------------------------------------------------------- #


def test_get_lazy_logger_caches_by_name():
    a1 = get_lazy_logger("unique.cache.name")
    a2 = get_lazy_logger("unique.cache.name")
    assert a1 is a2
    assert isinstance(a1, LazyLogger)


def test_get_lazy_logger_distinct_names_distinct_instances():
    a = get_lazy_logger("distinct.name.a")
    b = get_lazy_logger("distinct.name.b")
    assert a is not b


def test_get_lazy_logger_autoconfigures_on_first_use():
    logger_mod._configured = False
    get_lazy_logger("autoconfig.probe")
    assert logger_mod._configured is True


# --------------------------------------------------------------------------- #
# initialize_logging                                                           #
# --------------------------------------------------------------------------- #


def test_initialize_logging_sets_level_and_handlers():
    """Startup entrypoint installs a handler and applies the config level.

    Without an explicit CLI level the bootstrap ``config.logging.level == "INFO"``
    applies.
    """
    initialize_logging()

    root = logging.getLogger()
    assert root.level == logging.INFO
    assert root.handlers, "initialize_logging must install a console handler"


def test_initialize_logging_invalid_level_falls_back_to_info():
    """An unrecognised configured level must not crash: it degrades to INFO."""
    get_config().logging.level = "NOT_A_LEVEL"

    initialize_logging()

    assert logging.getLogger().level == logging.INFO
