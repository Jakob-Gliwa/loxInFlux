import abc
import asyncio
import sys

from gmqtt import Client as MQTTClient
from gmqtt import constants as MQTTconstants

from .config import get_config
from .logger import get_lazy_logger

logger = get_lazy_logger(__name__)


class TelegrafWriter(abc.ABC):
    """Abstract base class for Telegraf writers."""

    @abc.abstractmethod
    async def initialize(self):
        """Initialize the connection."""
        pass

    @abc.abstractmethod
    async def connect(self) -> None:
        """Create connection to Telegraf."""
        pass

    @abc.abstractmethod
    async def close(self) -> None:
        """Close the connection."""
        pass

    @abc.abstractmethod
    async def write(self, point: bytes) -> None:
        """Write a point to Telegraf."""
        pass

    async def __aenter__(self):
        """Async context manager entry."""
        await self.initialize()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        """Async context manager exit."""
        await self.close()


class UDPTelegrafWriter(TelegrafWriter):
    def __init__(self):
        """Initialize the UDP Telegraf writer with connection details."""
        cfg = get_config()
        self.host = cfg.telegraf.host
        self.port = cfg.telegraf.port
        self.transport = None
        self._initialized = False

    async def initialize(self):
        """Initialize the UDP connection if not already initialized."""
        if not self._initialized:
            await self.connect()
            self._initialized = True
        return self

    async def connect(self) -> None:
        """Create UDP connection to Telegraf."""
        try:
            loop = asyncio.get_running_loop()
            transport, _ = await loop.create_datagram_endpoint(
                lambda: asyncio.DatagramProtocol(), remote_addr=(self.host, self.port)
            )
            self.transport = transport
            logger.info(
                "Created UDP endpoint for Telegraf at %s:%d", self.host, self.port
            )
        except Exception as e:
            logger.error("Failed to create UDP endpoint for Telegraf: %s", e)
            raise

    async def close(self) -> None:
        """Close the UDP connection."""
        if self.transport:
            try:
                self.transport.close()
                logger.info("Closed UDP connection to Telegraf")
            except Exception as e:
                logger.error("Error closing Telegraf connection: %s", e)
        self._initialized = False

    async def write(self, point: bytes) -> None:
        try:
            self.transport.sendto(point)
        except Exception as e:
            logger.error("Failed to write to Telegraf: %s", e)
            # Try to reconnect
            await self.close()
            await self.initialize()


class TCPTelegrafWriter(TelegrafWriter):
    def __init__(self):
        """Initialize the TCP Telegraf writer with connection details."""
        cfg = get_config()
        self.host = cfg.telegraf.host
        self.port = cfg.telegraf.port
        self.max_retries = cfg.telegraf.max_retries
        self.writer = None
        self.reader = None
        self._initialized = False

    async def initialize(self):
        """Initialize the TCP connection if not already initialized."""
        if not self._initialized:
            await self.connect()
            self._initialized = True
        return self

    async def connect(self) -> None:
        """Create TCP connection to Telegraf.

        ``max_retries <= 0`` retries indefinitely (mirrors the MQTT writer's
        "unlimited" semantics); otherwise exactly ``max_retries`` attempts are
        made and the last failure is re-raised to the caller.
        """
        unlimited = self.max_retries <= 0
        attempt = 0
        while True:
            attempt += 1
            try:
                self.reader, self.writer = await asyncio.open_connection(
                    self.host, self.port
                )
                logger.info(
                    "Created TCP connection to Telegraf at %s:%d", self.host, self.port
                )
                return
            except Exception as e:
                logger.error(
                    "Failed to create TCP connection to Telegraf (attempt %d): %s",
                    attempt,
                    e,
                )
                if not unlimited and attempt >= self.max_retries:
                    raise
                await asyncio.sleep(1)

    async def close(self) -> None:
        """Close the TCP connection."""
        if self.writer:
            try:
                self.writer.close()
                await self.writer.wait_closed()
                logger.info("Closed TCP connection to Telegraf")
            except Exception as e:
                logger.error("Error closing Telegraf connection: %s", e)
        self._initialized = False

    async def write(self, point: bytes) -> None:
        """Write a point to Telegraf via TCP.

        TCP is a byte stream without message boundaries, so each point must be
        newline-terminated. Telegraf's ``socket_listener`` reads InfluxDB line
        protocol over TCP line-by-line; without the trailing ``\\n`` consecutive
        writes would be concatenated (``point1point2``) into a single, malformed
        line and dropped by the parser.

        Args:
            point: InfluxDB Point to write
        """
        if not self._initialized:
            await self.initialize()

        if point:
            try:
                self.writer.write(point + b"\n")
                await self.writer.drain()
            except Exception as e:
                logger.error("Failed to write to Telegraf: %s", e)
                # Try to reconnect
                await self.close()
                await self.initialize()


class ExecDTelegrafWriter(TelegrafWriter):
    """Writer that outputs metrics to stdout for Telegraf execd input plugin.

    Telegraf's execd input consumes line protocol line-by-line from the child's
    stdout, so each point is written and flushed immediately. No buffering is
    used: batching would only add latency without a throughput benefit here.
    """

    def __init__(self):
        """Initialize the ExecD writer."""
        self._initialized = False
        # Ensure stdout is not buffered
        sys.stdout.reconfigure(line_buffering=True)

    async def initialize(self):
        """Initialize the writer."""
        if not self._initialized:
            self._initialized = True
            logger.info("Initialized ExecD writer")
        return self

    async def connect(self) -> None:
        """No connection needed for ExecD writer."""
        pass

    async def close(self) -> None:
        """Close the writer."""
        self._initialized = False

    async def write(self, point: bytes) -> None:
        """Write a point to stdout.

        Args:
            point: InfluxDB Point to write
        """
        if point:
            try:
                sys.stdout.buffer.write(point + b"\n")
                sys.stdout.flush()
            except Exception as e:
                logger.error("Failed to write to stdout: %s", e)


class MQTTTelegrafWriter(TelegrafWriter):
    """Writer that publishes metrics to MQTT broker using gmqtt."""

    def __init__(self):
        """Initialize the MQTT Telegraf writer with connection details."""
        cfg = get_config()
        self.mqtt_config = cfg.telegraf.mqtt
        self.max_retries = cfg.telegraf.max_retries
        self.client: MQTTClient | None = None
        self._initialized = False
        self._STOP = asyncio.Event()

    async def initialize(self):
        """Initialize the MQTT connection if not already initialized."""
        if not self._initialized:
            await self.connect()
            self._initialized = True
        return self

    async def connect(self) -> None:
        """Create MQTT connection to broker."""
        try:
            self.client = MQTTClient(self.mqtt_config.client_id)

            # Set callbacks
            self.client.on_connect = self._on_connect
            self.client.on_disconnect = self._on_disconnect
            self.client.set_config(
                {
                    "reconnect_retries": self.max_retries
                    if self.max_retries > 0
                    else MQTTconstants.UNLIMITED_RECONNECTS,
                    "reconnect_delay": 60,
                }
            )
            self.client.set_auth_credentials(
                self.mqtt_config.username, self.mqtt_config.password
            )
            # Connect to broker
            await self.client.connect(
                host=self.mqtt_config.host,
                port=self.mqtt_config.port,
                keepalive=60,
                version=MQTTconstants.MQTTv311,  # MQTTv3.1.1 for performacne reasons - no need for v5
            )
            logger.info(
                "Connected to MQTT broker at %s:%d",
                self.mqtt_config.host,
                self.mqtt_config.port,
            )
        except Exception as e:
            logger.error("Failed to connect to MQTT broker: %s", e)
            raise

    async def close(self) -> None:
        """Close the MQTT connection."""
        if self.client:
            try:
                await self.client.disconnect()
                logger.info("Disconnected from MQTT broker")
            except Exception as e:
                logger.error("Error disconnecting from MQTT broker: %s", e)
        self._initialized = False

    async def write(self, point: bytes) -> None:
        """Write a point to MQTT broker.

        Args:
            point: InfluxDB Point to write
        """
        if not self._initialized:
            await self.initialize()

        if point and self.client:
            try:
                self.client.publish(self.mqtt_config.topic, point, qos=0)
            except Exception as e:
                logger.error("Failed to publish to MQTT: %s", e)
                # Try to reconnect
                await self.close()
                await self.initialize()

    def _on_connect(self, client, flags, rc, properties):
        logger.info("MQTT connected")

    def _on_disconnect(self, client, packet, exc=None):
        logger.info("MQTT disconnected")
        if exc:
            logger.error("Disconnect error: %s", exc)


def create_telegraf_writer() -> TelegrafWriter:
    """Factory function to create the appropriate TelegrafWriter based on config."""
    writer_type = get_config().telegraf.protocol.lower()
    if writer_type == "tcp":
        return TCPTelegrafWriter()
    elif writer_type == "execd":
        return ExecDTelegrafWriter()
    elif writer_type == "mqtt":
        return MQTTTelegrafWriter()
    return UDPTelegrafWriter()  # Default to UDP


# The writer is created explicitly via ``init_telegraf()`` (from ``main()`` or
# the test bootstrap) rather than at import time: constructing a writer reads the
# app config, so an import-time instance would defeat the side-effect-free import.
_writer: TelegrafWriter | None = None


def init_telegraf() -> TelegrafWriter:
    """Create the configured Telegraf writer and store it as the singleton."""
    global _writer
    _writer = create_telegraf_writer()
    return _writer


def get_telegraf() -> TelegrafWriter:
    """Return the initialized Telegraf writer.

    Raises:
        RuntimeError: if ``init_telegraf()`` has not been called yet.
    """
    if _writer is None:
        raise RuntimeError(
            "Telegraf writer not initialized - call init_telegraf() first "
            "(main() does this at startup)."
        )
    return _writer


def reset_telegraf() -> None:
    """Drop the created writer (test isolation helper)."""
    global _writer
    _writer = None
