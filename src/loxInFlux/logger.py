import logging
import os
import sys
from typing import Optional

from .config import config

class Logger:
    _instance = None
    _logger: Optional[logging.Logger] = None

    def __new__(cls):
        if cls._instance is None:
            cls._instance = super(Logger, cls).__new__(cls)
            cls._instance._setup_logger()
        return cls._instance

    def _get_log_level(self) -> str:
        # Command line argument has highest priority
        if len(sys.argv) > 1 and sys.argv[1].upper() == "--DEBUG":
            return "DEBUG"
        
        # Environment variable has second priority
        env_level = os.getenv("APP_LOG_LEVEL")
        if env_level:
            return env_level.upper()
        
        # Config file has lowest priority
        return config.logging.level

    def _setup_logger(self):
        self._logger = logging.getLogger("loxone-statistics")
        
        # Set log level
        log_level = self._get_log_level()
        self._logger.setLevel(getattr(logging, log_level))

        # Create console handler
        handler = logging.StreamHandler()
        handler.setLevel(logging.DEBUG)


        # Add handler to logger
        self._logger.addHandler(handler)

    @property
    def logger(self) -> logging.Logger:
        return self._logger 