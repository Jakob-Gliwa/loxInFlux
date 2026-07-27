import functools
import logging
import platform
import subprocess
import sys
import time
from collections.abc import Callable
from datetime import datetime
from importlib import metadata
from typing import ParamSpec, TypeVar

import aiohttp
import orjson as json

from .config import get_config
from .logger import (
    ensure_basic_logging,
    get_lazy_logger,
)

T = TypeVar("T")
P = ParamSpec("P")

CMD_GET_LOXAPP3_JSON_LAST_MODIFIED = "jdev/sps/LoxAPPversion3"

logger = get_lazy_logger(__name__)


################################################################################
# CPU / architecture detection
#
# Dependency-free (no lscpu, absent from slim images): reads /proc/cpuinfo on
# Linux and sysctl on macOS, defaulting to False when in doubt so we never pick
# a native code path the CPU cannot run.
################################################################################

_X86_TOKENS = frozenset({"x86_64", "amd64", "i386", "i686", "x86"})


@functools.lru_cache(maxsize=1)
def machine() -> str:
    return platform.machine().lower()


@functools.lru_cache(maxsize=1)
def is_x86() -> bool:
    return machine() in _X86_TOKENS


@functools.lru_cache(maxsize=1)
def has_avx2() -> bool:
    """Whether this (x86) CPU supports AVX2. Always False on non-x86.

    Diagnostic only (reported once by log_runtime_environment). The pygixml
    fast-path gate is the subprocess probe in native_import_runs, not this flag,
    so a conservative False here never risks a SIGILL - it only omits a banner.
    """
    if not is_x86():
        return False

    system = platform.system()
    try:
        if system == "Linux":
            with open("/proc/cpuinfo", encoding="utf-8", errors="ignore") as fh:
                text = fh.read()
        elif system == "Darwin":
            # AVX2 is advertised in the CPUID leaf-7 feature set; query just that
            # key instead of dumping all of `sysctl -a`.
            text = subprocess.check_output(
                ["sysctl", "-n", "machdep.cpu.leaf7_features"], text=True
            )
        else:
            # No reliable dependency-free AVX2 probe elsewhere. Notably on
            # Windows `wmic cpu get Caption` returns the model name, never the
            # feature flags (and wmic is deprecated/removed on recent builds),
            # so we report unknown (False) rather than pretend to detect it.
            return False
    except OSError, subprocess.SubprocessError:
        return False

    # Token match: /proc/cpuinfo and sysctl both list flags space-separated, so
    # split guards against substring false positives (and future avx-* tokens).
    return "avx2" in text.lower().split()


@functools.cache
def native_import_runs(probe_code: str) -> bool:
    """True iff *probe_code* runs to completion in a fresh subprocess on THIS CPU.

    Gate for native deps that ship CPU-specific code with no runtime dispatch
    (e.g. a pygixml build compiled with AVX2 / -march=native): importing such a
    build on a CPU missing an instruction raises SIGILL (exit 132) and takes the
    whole process down, uncatchable in-process. Running the risky import in a
    throwaway child turns that hard crash into a boolean, so the caller can fall
    back (e.g. to lxml) instead of dying. Result is cached.
    """
    try:
        proc = subprocess.run(
            [sys.executable, "-c", probe_code],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
            check=False,
        )
    except OSError, subprocess.SubprocessError:
        return False
    return proc.returncode == 0


################################################################################
# Startup diagnostics
################################################################################

# Dependencies worth pinning down when triaging crashes / deployment issues.
_KEY_PACKAGES = (
    "aiohttp",
    "orjson",
    "pycryptodome",
    "uvloop",
    "gmqtt",
    "lxml",
    "pygixml",
    "lz4",
    "loxwebsocket",
    "influxdb-client",
    "construct",
)


def _dependency_versions() -> str:
    parts = []
    for name in _KEY_PACKAGES:
        try:
            parts.append(f"{name}={metadata.version(name)}")
        except metadata.PackageNotFoundError:
            parts.append(f"{name}=<not installed>")
    return ", ".join(parts)


def log_runtime_environment() -> None:
    """Emit the startup environment banner at INFO level.

    Localizes future deployment issues across amd64/arm64/Pi: records the active
    XML parser, CPU features and key dependency versions in one place. Called
    once from app.main() after logging is configured.
    """
    import loxInFlux
    from loxInFlux import miniserver

    logger.info("----- loxInFlux runtime environment -----")
    logger.info(
        "version=%s  python=%s  executable=%s",
        getattr(loxInFlux, "__version__", "unknown"),
        platform.python_version(),
        sys.executable,
    )
    logger.info(
        "platform=%s %s  arch=%s  (x86=%s, avx2=%s)",
        platform.system(),
        platform.release(),
        platform.machine(),
        is_x86(),
        has_avx2(),
    )
    # The parser probe is deferred out of import time, so resolve it here - this
    # banner is the first thing that wants to know the active backend.
    miniserver._ensure_pygixml()
    logger.info(
        "xml parser: %s  (%s)",
        getattr(miniserver, "ACTIVE_XML_PARSER", "lxml"),
        getattr(miniserver, "XML_PARSER_REASON", "default"),
    )
    logger.info("dependencies: %s", _dependency_versions())
    logger.info("-----------------------------------------")


def get_numeric_value_if_possible(value):
    try:
        return int(value)
    except ValueError:
        try:
            float_value = float(value)
            cfg = get_config()
            return (
                round(float_value, cfg.general.rounding_precision)
                if cfg.general.round_floats
                else float_value
            )
        except ValueError:
            return value


def _build_base_url():
    # Loxone serves https only on the standard TLS port 443; every other port is
    # plain http (there is no https-on-custom-port case for a Miniserver). The
    # port is appended only when it deviates from http's default of 80.
    ms = get_config().miniserver
    if ms.port == 443:
        return f"https://{ms.host}"
    if ms.port == 80:
        return f"http://{ms.host}"
    return f"http://{ms.host}:{ms.port}"


def log_performance(name: str | None = None, severity: int | None = logging.DEBUG):
    """
    A decorator that logs the execution time of a function if logging level is DEBUG or lower.

    Args:
        name: Optional name to use in the log message. If not provided, uses the function name.
    """

    def decorator(func: Callable[P, T]) -> Callable[P, T]:
        @functools.wraps(func)
        def wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
            ensure_basic_logging()  # Ensure logging is configured
            logger = logging.getLogger(func.__module__)

            operation_name = name or func.__name__
            start_time = time.perf_counter_ns()

            try:
                result = func(*args, **kwargs)
                end_time = time.perf_counter_ns()
                duration_ms = end_time - start_time
                logger.log(
                    severity, "Performance: %s took %.2fns", operation_name, duration_ms
                )
                return result
            except Exception as e:
                end_time = time.perf_counter_ns()
                duration_ms = end_time - start_time
                logger.debug(
                    "Performance: %s failed after %.2fns with error: %s",
                    operation_name,
                    duration_ms,
                    str(e),
                )
                raise

        return wrapper

    return decorator


# Check if the element is in the LoxAPP3.json file for debug purposes
def checkIfElementInLox3APP(loxapp3_json_control_list, controls, uuid):
    if uuid not in loxapp3_json_control_list:
        if (
            uuid in controls
            and "parent_uuid" in controls[uuid]
            and controls[uuid]["parent_uuid"] in loxapp3_json_control_list
        ):
            logger.debug("%s parent in LoxAPP3.json", uuid)
        else:
            logger.warning(
                "%s not in the LoxAPP3.json and not in the overall controls list", uuid
            )


async def get_loxapp3_json_last_modified():
    async with (
        aiohttp.ClientSession(
            # aiohttp.BasicAuth is deprecated (removed in 4.0); build the header
            # ourselves via the non-deprecated encode_basic_auth helper instead.
            headers={
                "Authorization": aiohttp.encode_basic_auth(
                    get_config().miniserver.user, get_config().miniserver.password
                )
            },
            timeout=aiohttp.ClientTimeout(total=30),
        ) as client,
        client.get(
            f"{_build_base_url()}/{CMD_GET_LOXAPP3_JSON_LAST_MODIFIED}",
            allow_redirects=True,
            ssl=False,  # Disable SSL verification
        ) as response,
    ):
        if response.status != 200:
            logger.warning("Non-200 response received: %s", response.status)
            return None

        return datetime.strptime(
            (await response.json(loads=json.loads))["LL"]["value"],
            "%Y-%m-%d %H:%M:%S",
        )
