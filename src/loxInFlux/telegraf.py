import abc
import asyncio
import logging
from typing import Optional
from .config import config
import sys
from gmqtt import Client as MQTTClient
from gmqtt import constants as MQTTconstants

logger = logging.getLogger(__name__)

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
        self.host = config.telegraf.host
        self.port = config.telegraf.port
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
                lambda: asyncio.DatagramProtocol(),
                remote_addr=(self.host, self.port)
            )
            self.transport = transport
            logger.info(f"Created UDP endpoint for Telegraf at {self.host}:{self.port}")
        except Exception as e:
            logger.error(f"Failed to create UDP endpoint for Telegraf: {e}")
            raise

    async def close(self) -> None:
        """Close the UDP connection."""
        if self.transport:
            try:
                self.transport.close()
                logger.info("Closed UDP connection to Telegraf")
            except Exception as e:
                logger.error(f"Error closing Telegraf connection: {e}")
        self._initialized = False
        
    async def write(self, point: bytes) -> None:
        try:
            self.transport.sendto(point)
        except Exception as e:
            logger.error(f"Failed to write to Telegraf: {e}")
            # Try to reconnect
            await self.close()
            await self.initialize()

class TCPTelegrafWriter(TelegrafWriter):
    def __init__(self):
        """Initialize the TCP Telegraf writer with connection details."""
        self.host = config.telegraf.host
        self.port = config.telegraf.port
        self.max_retries = config.telegraf.max_retries
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
        """Create TCP connection to Telegraf."""
        for attempt in range(1,self.max_retries):
            try:
                self.reader, self.writer = await asyncio.open_connection(self.host, self.port)
                logger.info(f"Created TCP connection to Telegraf at {self.host}:{self.port}")
                return
            except Exception as e:
                logger.error(f"Failed to create TCP connection to Telegraf: {e}")
                if self.max_retries > 0 and attempt > self.max_retries:
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
                logger.error(f"Error closing Telegraf connection: {e}")
        self._initialized = False
        
    async def write(self, point: bytes) -> None:
        """Write a point to Telegraf via TCP.
        
        Args:
            point: InfluxDB Point to write
        """
        if not self._initialized:
            await self.initialize()
            
        if point:
            try:
                self.writer.write(point)
                await self.writer.drain()
            except Exception as e:
                logger.error(f"Failed to write to Telegraf: {e}")
                # Try to reconnect
                await self.close()
                await self.initialize()

class ExecDTelegrafWriter(TelegrafWriter):
    """Writer that outputs metrics to stdout for Telegraf execd input plugin."""
    
    def __init__(self):
        """Initialize the ExecD writer."""
        self._initialized = False
        self._buffer = []
        self._buffer_size = 5000  # Max number of points to buffer
        self._flush_task = None
        # Ensure stdout is not buffered
        sys.stdout.reconfigure(line_buffering=True)

    async def initialize(self):
        """Initialize the writer and start flush task."""
        if not self._initialized:
            self._initialized = True
            self._flush_task = asyncio.create_task(self._periodic_flush())
            logger.info("Initialized ExecD writer")
        return self

    async def connect(self) -> None:
        """No connection needed for ExecD writer."""
        pass

    async def close(self) -> None:
        """Close the writer and flush remaining points."""
        if self._flush_task:
            self._flush_task.cancel()
            try:
                await self._flush_task
            except asyncio.CancelledError:
                pass
        await self._flush_buffer()
        self._initialized = False
    
    async def _periodic_flush(self):
        """Periodically flush the buffer."""
        while True:
            try:
                await asyncio.sleep(1.0)  # Flush every second
                await self._flush_buffer()
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Error in periodic flush: {e}")

    async def _flush_buffer(self):
        """Flush the current buffer to stdout."""
        if not self._buffer:
            return
            
        try:
            sys.stdout.buffer.write(b''.join(self._buffer))
            sys.stdout.flush()
            self._buffer.clear()
        except Exception as e:
            logger.error(f"Failed to flush buffer to stdout: {e}")
    
    async def write(self, point: bytes) -> None:
        """Write a point to the buffer.
        
        Args:
            point: InfluxDB Point to write
        """
        if point:
            try:
                sys.stdout.buffer.write(point + b'\n')
                sys.stdout.flush()
            except Exception as e:
                logger.error(f"Failed to write to buffer: {e}")

class MQTTTelegrafWriter(TelegrafWriter):
    """Writer that publishes metrics to MQTT broker using gmqtt."""
    
    def __init__(self):
        """Initialize the MQTT Telegraf writer with connection details."""
        self.mqtt_config = config.telegraf.mqtt
        self.max_retries = config.telegraf.max_retries
        self.client: Optional[MQTTClient] = None
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
            self.client.set_config({'reconnect_retries': self.max_retries if self.max_retries > 0 else MQTTconstants.UNLIMITED_RECONNECTS, 'reconnect_delay': 60})
            self.client.set_auth_credentials(self.mqtt_config.username, self.mqtt_config.password)
            # Connect to broker
            await self.client.connect(
                host=self.mqtt_config.host, 
                port=self.mqtt_config.port, 
                keepalive=60,
                version=MQTTconstants.MQTTv311 # MQTTv3.1.1 for performacne reasons - no need for v5
            )
            logger.info(f"Connected to MQTT broker at {self.mqtt_config.host}:{self.mqtt_config.port}")
        except Exception as e:
            logger.error(f"Failed to connect to MQTT broker: {e}")
            raise

    async def close(self) -> None:
        """Close the MQTT connection."""
        if self.client:
            try:
                await self.client.disconnect()
                logger.info("Disconnected from MQTT broker")
            except Exception as e:
                logger.error(f"Error disconnecting from MQTT broker: {e}")
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
                logger.error(f"Failed to publish to MQTT: {e}")
                # Try to reconnect
                await self.close()
                await self.initialize()

    def _on_connect(client, flags, rc, properties, userdata):
        logger.info("MQTT connected")

    def _on_disconnect(client, packet, exc=None):
        logger.info("MQTT disconnected")
        if exc:
            logger.error(f"Disconnect error: {exc}")

def create_telegraf_writer() -> TelegrafWriter:
    """Factory function to create the appropriate TelegrafWriter based on config."""
    writer_type = config.telegraf.protocol.lower()
    if writer_type == "tcp":
        return TCPTelegrafWriter()
    elif writer_type == "execd":
        return ExecDTelegrafWriter()
    elif writer_type == "mqtt":
        return MQTTTelegrafWriter()
    return UDPTelegrafWriter()  # Default to UDP

# Create a global instance
telegraf = create_telegraf_writer()