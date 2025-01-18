from datetime import datetime
import functools
import logging
import time
import orjson as json
from typing import Callable, TypeVar, ParamSpec, Optional

import aiohttp
import asyncio

from loxInFlux.argparser import get_args
from .config import config

T = TypeVar('T')
P = ParamSpec('P')

CMD_GET_LOXAPP3_JSON_LAST_MODIFIED = "jdev/sps/LoxAPPversion3"

logger = logging.getLogger(__name__)


# Define TRACE level (lower than DEBUG)
TRACE_LEVEL = 5  # DEBUG is 10, so we make TRACE lower
logging.addLevelName(TRACE_LEVEL, 'TRACE')

class TelegrafFormatter(logging.Formatter):
    """Custom formatter for Telegraf execd logging format."""
    
    LEVEL_PREFIXES = {
        logging.ERROR: 'E!',
        logging.WARNING: 'W!',
        logging.INFO: 'I!',
        logging.DEBUG: 'D!',
        TRACE_LEVEL: 'T!'  # Use our custom TRACE level
    }
    
    def format(self, record):
        """Format the log record according to Telegraf execd specifications."""
        # Get the appropriate prefix for the log level
        prefix = self.LEVEL_PREFIXES.get(record.levelno, 'E!')
        
        # Format the message with timestamp and other details
        formatted_msg = super().format(record)
        
        # Return the message with the appropriate prefix
        return f"{prefix} {formatted_msg}"

# Add trace method to Logger class
def trace(self, message, *args, **kwargs):
    """Log 'msg % args' with severity 'TRACE'."""
    if self.isEnabledFor(TRACE_LEVEL):
        self._log(TRACE_LEVEL, message, args, **kwargs)

# Add trace method to Logger class
logging.Logger.trace = trace

def configure_logging(level: str = "INFO", use_telegraf_format: bool = False):
    """Configure logging with appropriate formatter based on configuration.
    
    Args:
        level: Logging level to use
        use_telegraf_format: Whether to use Telegraf execd formatting
    """
    root_logger = logging.getLogger()
    
    # Clear any existing handlers
    root_logger.handlers.clear()
    
    # Create console handler
    console_handler = logging.StreamHandler()
    
    # Set formatter based on configuration
    if use_telegraf_format:
        formatter = TelegrafFormatter('%(message)s')
    else:
        formatter = logging.Formatter('%(asctime)s - %(name)s - %(levelname)s - %(message)s')
    
    console_handler.setFormatter(formatter)
    root_logger.addHandler(console_handler)
    
    # Set log level
    log_level = getattr(logging, level.upper(), logging.INFO)
    root_logger.setLevel(log_level)

# Update the ensure_basic_logging function
def ensure_basic_logging():
    """Ensures that basic logging is configured if no handlers exist."""
    if not logging.getLogger().handlers:
        use_telegraf = config.telegraf.protocol.lower() == "execd"
        configure_logging(config.logging.level, use_telegraf)

def get_numeric_value_if_possible(value):
    try:
        return int(value)
    except ValueError:
        try:
            float_value = float(value)
            return round(float_value, config.general.rounding_precision) if config.general.round_floats else float_value
        except ValueError:
            return value

def initialize_logging():
    args = get_args()
    # Initialize config with command line arguments
    
    # Set logging level - prioritize args over config
    log_level = args.log_level.upper() if args.log_level else config.logging.level.upper()
    
    # Determine if we should use Telegraf formatting
    use_telegraf_format = config.telegraf.protocol.lower() == "execd"
    
    # Configure logging with appropriate format
    configure_logging(log_level, use_telegraf_format)
    
    return args

def _build_base_url():
        protocol = "https" if config.miniserver.port == 443 else "http"
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
                logger.log(severity, f"Performance: {operation_name} took {duration_ms:.2f}ns")
                return result
            except Exception as e:
                end_time = time.perf_counter_ns()
                duration_ms = (end_time - start_time)
                logger.debug(f"Performance: {operation_name} failed after {duration_ms:.2f}ns with error: {str(e)}")
                raise
                
        return wrapper
    return decorator


# Check if the element is in the LoxAPP3.json file for debug purposes
def checkIfElementInLox3APP(loxapp3_json_control_list, controls, uuid):
    if uuid not in loxapp3_json_control_list:
        if uuid in controls and "parent_uuid" in controls[uuid] and controls[uuid]["parent_uuid"] in loxapp3_json_control_list:
            logger.debug(f"{uuid} parent in LoxAPP3.json")
        else:
            logger.warn(f"{uuid} not in the LoxAPP3.json and not in the overall controls list")


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
