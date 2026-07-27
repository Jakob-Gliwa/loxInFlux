"""Unit tests for ``loxInFlux.utils``.

Covers ``get_numeric_value_if_possible`` (int/float/passthrough + rounding),
``_build_base_url`` (protocol + port inclusion), ``get_loxapp3_json_last_modified``
(aiohttp mocked via ``support.fakes``) and the ``log_performance`` decorator.
"""

from __future__ import annotations

import logging
import subprocess
from datetime import datetime
from unittest.mock import mock_open

import pytest
from support import fakes

import loxInFlux.utils as utils
from loxInFlux.config import get_config
from loxInFlux.utils import (
    _build_base_url,
    get_loxapp3_json_last_modified,
    get_numeric_value_if_possible,
    has_avx2,
    is_x86,
    log_performance,
    log_runtime_environment,
    machine,
    native_import_runs,
)

# --------------------------------------------------------------------------- #
# get_numeric_value_if_possible                                                #
# --------------------------------------------------------------------------- #


def test_numeric_int_string():
    result = get_numeric_value_if_possible("42")
    assert result == 42
    assert isinstance(result, int)


def test_numeric_float_string():
    result = get_numeric_value_if_possible("3.5")
    assert result == 3.5
    assert isinstance(result, float)


def test_numeric_non_numeric_passthrough():
    assert get_numeric_value_if_possible("hello") == "hello"


def test_numeric_scientific_notation_is_float():
    assert get_numeric_value_if_possible("1e3") == 1000.0


def test_numeric_int_input_passthrough():
    # int() of an int succeeds, so it is returned unchanged.
    assert get_numeric_value_if_possible(7) == 7


def test_numeric_rounding_applied_when_configured():
    get_config().general.round_floats = True
    get_config().general.rounding_precision = 2
    assert get_numeric_value_if_possible("3.14159") == 3.14


def test_numeric_no_rounding_by_default():
    get_config().general.round_floats = False
    assert get_numeric_value_if_possible("3.14159") == 3.14159


# --------------------------------------------------------------------------- #
# _build_base_url                                                              #
# --------------------------------------------------------------------------- #


def test_base_url_default_http_port_omits_port():
    get_config().miniserver.host = "127.0.0.1"
    get_config().miniserver.port = 80
    assert _build_base_url() == "http://127.0.0.1"


def test_base_url_https_default_port_omits_port():
    get_config().miniserver.host = "127.0.0.1"
    get_config().miniserver.port = 443
    assert _build_base_url() == "https://127.0.0.1"


def test_base_url_non_default_http_port_included():
    get_config().miniserver.host = "10.0.0.5"
    get_config().miniserver.port = 8080
    assert _build_base_url() == "http://10.0.0.5:8080"


def test_base_url_non_443_port_is_always_http():
    # Loxone serves https only on 443; any other port (even a TLS-looking 8443)
    # is plain http, so the URL keeps the explicit port.
    get_config().miniserver.host = "10.0.0.5"
    get_config().miniserver.port = 8443
    assert _build_base_url() == "http://10.0.0.5:8443"


# --------------------------------------------------------------------------- #
# has_avx2 (CPU feature detection)                                            #
#                                                                              #
# has_avx2 is lru_cached and reads is_x86()/platform.system(); every test must #
# start from a cleared cache and a known-x86 stub. The subprocess error tests  #
# also guard against a regression of the Python-2 ``except A, B:`` syntax that #
# once made utils.py unimportable.                                             #
# --------------------------------------------------------------------------- #


@pytest.fixture
def x86_platform(monkeypatch):
    """Force has_avx2 onto the x86 path with a cleared cache before and after."""
    has_avx2.cache_clear()
    monkeypatch.setattr(utils, "is_x86", lambda: True)
    yield monkeypatch
    has_avx2.cache_clear()


def test_has_avx2_false_when_not_x86(monkeypatch):
    # Non-x86 short-circuits before any /proc or sysctl access.
    has_avx2.cache_clear()
    monkeypatch.setattr(utils, "is_x86", lambda: False)

    def _fail(*args, **kwargs):  # pragma: no cover - must never be reached
        raise AssertionError("no CPU probe should run on non-x86")

    monkeypatch.setattr("builtins.open", _fail)
    monkeypatch.setattr(utils.subprocess, "check_output", _fail)
    try:
        assert has_avx2() is False
    finally:
        has_avx2.cache_clear()


def test_has_avx2_true_on_linux_with_avx2_flag(x86_platform):
    x86_platform.setattr(utils.platform, "system", lambda: "Linux")
    cpuinfo = "flags : fpu vme de pse tsc msr pae avx avx2 fma bmi1 bmi2\n"
    x86_platform.setattr("builtins.open", mock_open(read_data=cpuinfo))
    assert has_avx2() is True


def test_has_avx2_false_on_linux_without_avx2_flag(x86_platform):
    x86_platform.setattr(utils.platform, "system", lambda: "Linux")
    # avx present but not avx2, and avx512* must not trip the token match.
    cpuinfo = "flags : fpu vme de pse tsc msr pae avx avx512f avx512dq\n"
    x86_platform.setattr("builtins.open", mock_open(read_data=cpuinfo))
    assert has_avx2() is False


def test_has_avx2_true_on_darwin_leaf7_features(x86_platform):
    x86_platform.setattr(utils.platform, "system", lambda: "Darwin")
    x86_platform.setattr(
        utils.subprocess,
        "check_output",
        lambda *a, **k: "RDWRFSGS TSC_THREAD_OFFSET BMI1 AVX2 SMEP BMI2\n",
    )
    assert has_avx2() is True


def test_has_avx2_false_on_windows_and_other(x86_platform):
    # No reliable dependency-free probe on Windows/other -> reported as unknown.
    x86_platform.setattr(utils.platform, "system", lambda: "Windows")

    def _fail(*args, **kwargs):  # pragma: no cover - must never be reached
        raise AssertionError("no probe command should run on Windows/other")

    x86_platform.setattr(utils.subprocess, "check_output", _fail)
    assert has_avx2() is False


def test_has_avx2_returns_false_on_oserror(x86_platform):
    x86_platform.setattr(utils.platform, "system", lambda: "Linux")

    def _boom(*args, **kwargs):
        raise OSError("no /proc/cpuinfo")

    x86_platform.setattr("builtins.open", _boom)
    assert has_avx2() is False


def test_has_avx2_returns_false_on_subprocess_error(x86_platform):
    x86_platform.setattr(utils.platform, "system", lambda: "Darwin")

    def _boom(*args, **kwargs):
        raise subprocess.SubprocessError("sysctl failed")

    x86_platform.setattr(utils.subprocess, "check_output", _boom)
    assert has_avx2() is False


# --------------------------------------------------------------------------- #
# native_import_runs (subprocess gate for native imports)                     #
# --------------------------------------------------------------------------- #


def test_native_import_runs_returns_false_on_oserror(monkeypatch):
    def _boom(*args, **kwargs):
        raise OSError("cannot spawn subprocess")

    monkeypatch.setattr(utils.subprocess, "run", _boom)
    assert native_import_runs("import sys  # oserror probe") is False


def test_native_import_runs_returns_false_on_subprocess_error(monkeypatch):
    def _boom(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd="python", timeout=30)

    monkeypatch.setattr(utils.subprocess, "run", _boom)
    assert native_import_runs("import sys  # timeout probe") is False


def test_native_import_runs_true_when_probe_succeeds(monkeypatch):
    class _CompletedProc:
        returncode = 0

    monkeypatch.setattr(utils.subprocess, "run", lambda *a, **k: _CompletedProc())
    assert native_import_runs("import sys  # success probe") is True


# --------------------------------------------------------------------------- #
# get_loxapp3_json_last_modified (aiohttp mocked)                              #
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_get_loxapp3_last_modified_parses_datetime(monkeypatch):
    get_config().miniserver.host = "127.0.0.1"
    get_config().miniserver.port = 80
    session = fakes.FakeAiohttpSession(
        default=fakes.FakeHTTPResponse(
            status=200, json_data={"LL": {"value": "2024-01-01 12:30:45"}}
        )
    )
    monkeypatch.setattr("loxInFlux.utils.aiohttp.ClientSession", session.as_factory())

    result = await get_loxapp3_json_last_modified()

    assert result == datetime(2024, 1, 1, 12, 30, 45)
    # Verify it hit the documented endpoint on the configured base URL.
    requested_url = session.requests[0][0]
    assert requested_url == "http://127.0.0.1/jdev/sps/LoxAPPversion3"


@pytest.mark.parametrize("status", [404, 500])
@pytest.mark.asyncio
async def test_get_loxapp3_last_modified_returns_none_on_non_200(monkeypatch, status):
    # Any non-200 (client 404 or server 500) short-circuits to None without ever
    # trying to parse a body as JSON.
    session = fakes.FakeAiohttpSession(default=fakes.FakeHTTPResponse(status=status))
    monkeypatch.setattr("loxInFlux.utils.aiohttp.ClientSession", session.as_factory())

    assert await get_loxapp3_json_last_modified() is None


# --------------------------------------------------------------------------- #
# log_runtime_environment / CPU-arch probe                                     #
# --------------------------------------------------------------------------- #


def test_arch_probe_returns_expected_types():
    # The dependency-free CPU probe must always answer without crashing.
    assert isinstance(machine(), str)
    assert isinstance(is_x86(), bool)
    assert isinstance(has_avx2(), bool)


def test_log_runtime_environment_emits_banner(caplog):
    with caplog.at_level(logging.INFO, logger="loxInFlux.utils"):
        log_runtime_environment()

    text = caplog.text
    # Banner framing + the diagnostic lines the startup log promises.
    assert "loxInFlux runtime environment" in text
    assert "version=" in text and "python=" in text
    assert "arch=" in text and "x86=" in text and "avx2=" in text
    assert "xml parser:" in text
    assert "dependencies:" in text
    # A couple of the pinned key packages must be reported.
    assert "aiohttp=" in text
    assert "orjson=" in text


# --------------------------------------------------------------------------- #
# log_performance                                                              #
# --------------------------------------------------------------------------- #


def test_log_performance_returns_result_and_logs(caplog):
    @log_performance("myop")
    def double(x):
        return x * 2

    with caplog.at_level(logging.DEBUG):
        assert double(21) == 42

    assert "Performance: myop took" in caplog.text


def test_log_performance_uses_function_name_by_default(caplog):
    @log_performance()
    def named_operation():
        return "ok"

    with caplog.at_level(logging.DEBUG):
        assert named_operation() == "ok"

    assert "Performance: named_operation" in caplog.text


def test_log_performance_respects_severity(caplog):
    @log_performance("infoop", severity=logging.INFO)
    def op():
        return 1

    with caplog.at_level(logging.INFO):
        op()

    perf_records = [
        r for r in caplog.records if "Performance: infoop" in r.getMessage()
    ]
    assert perf_records
    assert all(r.levelno == logging.INFO for r in perf_records)


def test_log_performance_reraises_on_exception(caplog):
    @log_performance("failop")
    def boom():
        raise ValueError("kaboom")

    with caplog.at_level(logging.DEBUG), pytest.raises(ValueError, match="kaboom"):
        boom()

    assert "Performance: failop failed" in caplog.text
