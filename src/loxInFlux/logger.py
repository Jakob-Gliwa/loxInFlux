"""
Centralized logging configuration for loxInFlux.

Provides:
- LazyLogger: A performance-optimized logger wrapper with cached level checks
- TelegrafFormatter: Custom formatter for Telegraf execd output
- Automatic logging configuration on first use
"""

import logging
import os
import sys
from typing import Optional

from .config import config


# =============================================================================
# TRACE Level Support (lower than DEBUG)
# =============================================================================

TRACE_LEVEL = 5
logging.addLevelName(TRACE_LEVEL, 'TRACE')


def _trace(self, message, *args, **kwargs):
    """Log 'msg % args' with severity 'TRACE'."""
    if self.isEnabledFor(TRACE_LEVEL):
        self._log(TRACE_LEVEL, message, args, **kwargs)


# Add trace method to standard Logger class
logging.Logger.trace = _trace


# =============================================================================
# Telegraf Formatter
# =============================================================================

class TelegrafFormatter(logging.Formatter):
    """Custom formatter for Telegraf execd logging format."""
    
    LEVEL_PREFIXES = {
        logging.ERROR: 'E!',
        logging.WARNING: 'W!',
        logging.INFO: 'I!',
        logging.DEBUG: 'D!',
        TRACE_LEVEL: 'T!'
    }
    
    def format(self, record):
        """Format the log record according to Telegraf execd specifications."""
        prefix = self.LEVEL_PREFIXES.get(record.levelno, 'E!')
        formatted_msg = super().format(record)
        return f"{prefix} {formatted_msg}"


# =============================================================================
# LazyLogger - Performance Optimized Logger Wrapper
# =============================================================================

class LazyLogger:
    """
    A logger wrapper that provides lazy evaluation for debug messages.
    
    Caches the debug-enabled state for maximum performance, avoiding
    repeated isEnabledFor() calls in hot-paths.
    
    Performance gain: 2-7x faster when DEBUG is disabled.
    Overhead when DEBUG enabled: ~2.3% (negligible).
    """
    
    __slots__ = ('_logger', '_debug_enabled', '_trace_enabled')
    
    def __init__(self, logger: logging.Logger):
        """Initialize with an existing logger instance."""
        self._logger = logger
        self._update_level_cache()
    
    def _update_level_cache(self):
        """Update cached level states. Call this if the log level changes at runtime."""
        self._debug_enabled = self._logger.isEnabledFor(logging.DEBUG)
        self._trace_enabled = self._logger.isEnabledFor(TRACE_LEVEL)
    
    def trace(self, msg: str, *args, **kwargs):
        """Log with TRACE level (5) - only if enabled."""
        if self._trace_enabled:
            self._logger.log(TRACE_LEVEL, msg, *args, **kwargs)
    
    def debug(self, msg: str, *args, **kwargs):
        """Log with DEBUG level - only if enabled."""
        if self._debug_enabled:
            self._logger.debug(msg, *args, **kwargs)
    
    def info(self, msg: str, *args, **kwargs):
        """Log with INFO level."""
        self._logger.info(msg, *args, **kwargs)
    
    def warning(self, msg: str, *args, **kwargs):
        """Log with WARNING level."""
        self._logger.warning(msg, *args, **kwargs)
    
    def error(self, msg: str, *args, **kwargs):
        """Log with ERROR level."""
        self._logger.error(msg, *args, **kwargs)
    
    def critical(self, msg: str, *args, **kwargs):
        """Log with CRITICAL level."""
        self._logger.critical(msg, *args, **kwargs)
    
    def exception(self, msg: str, *args, **kwargs):
        """Log exception with ERROR level."""
        self._logger.exception(msg, *args, **kwargs)
    
    def log(self, level: int, msg: str, *args, **kwargs):
        """Log with custom level."""
        self._logger.log(level, msg, *args, **kwargs)
    
    def isEnabledFor(self, level: int) -> bool:
        """Check if the logger is enabled for a given level."""
        return self._logger.isEnabledFor(level)
    
    @property
    def level(self) -> int:
        """Get the current log level."""
        return self._logger.level
    
    def setLevel(self, level: int):
        """Set the log level and update cache."""
        self._logger.setLevel(level)
        self._update_level_cache()


# =============================================================================
# Global Configuration State
# =============================================================================

_configured = False
_lazy_loggers: dict[str, LazyLogger] = {}


def configure_logging(
    level: Optional[str] = None, 
    use_telegraf_format: bool = False,
    force: bool = False
) -> None:
    """
    Configure the root logger with appropriate settings.
    
    Args:
        level: Logging level (DEBUG, INFO, WARNING, ERROR, CRITICAL).
               If None, auto-detects from args/env/config.
        use_telegraf_format: Whether to use Telegraf execd formatting.
        force: Force reconfiguration even if already configured.
    """
    global _configured
    
    if _configured and not force:
        return
    
    # Determine log level from various sources
    if level is None:
        # Command line argument has highest priority
        if len(sys.argv) > 1 and sys.argv[1].upper() == "--DEBUG":
            level = "DEBUG"
        # Check argparser if available
        elif hasattr(config, '_args') and getattr(config._args, 'log_level', None):
            level = config._args.log_level.upper()
        # Environment variable has second priority
        elif env_level := os.getenv("APP_LOG_LEVEL"):
            level = env_level.upper()
        # Config file has lowest priority
        else:
            level = config.logging.level
    
    # Get root logger
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
    
    # Update all existing LazyLogger instances
    for lazy_logger in _lazy_loggers.values():
        lazy_logger._update_level_cache()
    
    _configured = True


def get_lazy_logger(name: Optional[str] = None) -> LazyLogger:
    """
    Get a LazyLogger instance with automatic configuration on first call.
    
    This is a drop-in replacement for logging.getLogger() that provides
    automatic lazy evaluation for debug messages.
    
    Args:
        name: Logger name (usually __name__). If None, returns root logger.
        
    Returns:
        A LazyLogger instance with cached level checks.
    """
    global _configured
    
    # Auto-configure on first use
    if not _configured:
        use_telegraf = config.telegraf.protocol.lower() == "execd"
        configure_logging(use_telegraf_format=use_telegraf)
    
    # Cache and return LazyLogger instances
    cache_key = name or "__root__"
    if cache_key not in _lazy_loggers:
        logger = logging.getLogger(name) if name else logging.getLogger()
        _lazy_loggers[cache_key] = LazyLogger(logger)
    
    return _lazy_loggers[cache_key]


def initialize_logging() -> object:
    """
    Initialize logging with command line arguments.
    
    Call this at application startup to configure logging based on
    command line arguments and configuration.
    
    Returns:
        The parsed command line arguments.
    """
    from loxInFlux.argparser import get_args
    
    args = get_args()
    
    # Set logging level - prioritize args over config
    log_level = args.log_level.upper() if args.log_level else config.logging.level.upper()
    
    # Determine if we should use Telegraf formatting
    use_telegraf_format = config.telegraf.protocol.lower() == "execd"
    
    # Configure logging with appropriate format
    configure_logging(log_level, use_telegraf_format, force=True)
    
    return args


def ensure_basic_logging() -> None:
    """Ensures that basic logging is configured if no handlers exist."""
    if not logging.getLogger().handlers:
        use_telegraf = config.telegraf.protocol.lower() == "execd"
        configure_logging(config.logging.level, use_telegraf)
