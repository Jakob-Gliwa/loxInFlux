from datetime import datetime
import functools
import logging
import re
import time
import orjson as json
from typing import Callable, TypeVar, ParamSpec, Optional

import aiohttp
import asyncio

from .config import config
from .logger import (
    get_lazy_logger,
    configure_logging,
    ensure_basic_logging,
    initialize_logging,
    TRACE_LEVEL,
)

T = TypeVar('T')
P = ParamSpec('P')

CMD_GET_LOXAPP3_JSON_LAST_MODIFIED = "jdev/sps/LoxAPPversion3"

logger = get_lazy_logger(__name__)


# Matches a single leading number (optional sign/decimals) followed only by a
# non-numeric unit, e.g. "523ppm", "22.3°", "65%", "0.158kW", "-5.2 °C".
# Deliberately strict: strings that contain further digits after the number
# (dates like "2026-07-09", times like "12:34") do NOT match and stay untouched.
_NUMERIC_WITH_UNIT = re.compile(r'^\s*(-?\d+(?:\.\d+)?)\s*[^\d\s]*$')


def get_numeric_value_if_possible(value):
    # Always coerce numeric values to float (never int) so the grabber path agrees
    # with the websocket path, which already emits floats. A single InfluxDB field
    # that receives both int and float values raises a "field type conflict" and the
    # mismatching points are silently dropped, so a consistent numeric type matters.
    try:
        float_value = float(value)
    except (ValueError, TypeError):
        # Many Loxone sensor controls (InfoOnlyAnalog and friends) report their
        # value WITH a unit as a string, e.g. "523ppm", "22.3°", "65%", "0.158kW",
        # which float() cannot parse. Extract a leading number followed only by a
        # unit. Anything else (genuine text, dates, times) is returned unchanged.
        if isinstance(value, str):
            match = _NUMERIC_WITH_UNIT.match(value)
            if match:
                float_value = float(match.group(1))
            else:
                return value
        else:
            return value
    return round(float_value, config.general.rounding_precision) if config.general.round_floats else float_value

def _build_base_url():
        protocol = "https" if config.miniserver.port == 443 else "http"
        
        # Include port in URL if it's not the default port for the protocol
        if (protocol == "http" and config.miniserver.port != 80) or \
           (protocol == "https" and config.miniserver.port != 443):
            return f"{protocol}://{config.miniserver.host}:{config.miniserver.port}"
        else:
            return f"{protocol}://{config.miniserver.host}"

def log_performance(name: Optional[str] = None, severity: Optional[int] = logging.DEBUG):
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
                duration_ms = (end_time - start_time)
                logger.log(severity, "Performance: %s took %.2fns", operation_name, duration_ms)
                return result
            except Exception as e:
                end_time = time.perf_counter_ns()
                duration_ms = (end_time - start_time)
                logger.debug("Performance: %s failed after %.2fns with error: %s", operation_name, duration_ms, str(e))
                raise
                
        return wrapper
    return decorator


# Check if the element is in the LoxAPP3.json file for debug purposes
def checkIfElementInLox3APP(loxapp3_json_control_list, controls, uuid):
    if uuid not in loxapp3_json_control_list:
        if uuid in controls and "parent_uuid" in controls[uuid] and controls[uuid]["parent_uuid"] in loxapp3_json_control_list:
            logger.debug("%s parent in LoxAPP3.json", uuid)
        else:
            logger.warning("%s not in the LoxAPP3.json and not in the overall controls list", uuid)


async def get_loxapp3_json_last_modified():
    async with aiohttp.ClientSession(
            auth=aiohttp.BasicAuth(login=config.miniserver.username, password=config.miniserver.password),
            timeout=aiohttp.ClientTimeout(total=30)
        ) as client:
            async with client.get(
                f"{_build_base_url()}/{CMD_GET_LOXAPP3_JSON_LAST_MODIFIED}",
                allow_redirects=True,
                ssl=False  # Disable SSL verification
            ) as response:
                if response.status != 200:
                    logger.warning("Non-200 response received: %s", response.status)
                    return None
                
                return datetime.strptime(await response.json(loads=json.loads)["LL"]["value"], "%Y-%m-%d %H:%M:%S")
