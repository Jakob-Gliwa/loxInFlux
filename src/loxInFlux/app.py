import logging
import asyncio
import time
import orjson

from loxwebsocket.exceptions import LoxoneException
from .config import config
from loxwebsocket.lox_ws_api import LoxWs, loxwebsocket
from loxInFlux.miniserver import getControlsFromConfigXML, load_miniserver_config
from .telegraf import telegraf
import signal
import uvloop
from .utils import _build_base_url, get_numeric_value_if_possible, initialize_logging
from .grabber import LoxoneGrabber
from .logger import get_lazy_logger

logger = get_lazy_logger(__name__)

def on_exit():
    logger.info("LoxInFlux exited")

class LoxInfluxBridge:

    def __init__(self):
        self.base_url = _build_base_url()
        self.ws_client = None
        self._shutdown_event = asyncio.Event()
        self._tasks = set()
        self.websocket_controls = {}
        # Add message type handlers dictionary
        self._message_handlers = {
            2: self.handle_value_states  # Register handler for value states
        }
        self.grabber_controls = {}
        self.controls = {}
        self.ws_client_initialized = asyncio.Event()
        self.grabber_controls_updated = asyncio.Event()
        self.formatter =  f"{{:.{config.general.rounding_precision if config.general.round_floats else 15}f}}".format

    
    def get_controls(self):
        return self.controls, self.websocket_controls, self.grabber_controls

    async def handle_value_states(self, message, message_type):
        """Handler for value state messages (type 2)"""
        timestamp = b" " + orjson.dumps(time.time_ns())

        for uuid, value in message.items():
            try:
                # Round float values if configured
                point = b"".join([self.websocket_controls[uuid]["point_websocket"],  self.formatter(value).encode(), timestamp])
                asyncio.create_task(telegraf.write(point))
            except KeyError:
                if uuid in self.controls:
                    logger.warning("%s not in initial websocket controls list. It is in the overall controls though. adding it to websocket controls.", uuid) 
                    self.websocket_controls[uuid] = self.controls[uuid]
                    point = b"".join([self.websocket_controls[uuid]["point_websocket"],  self.formatter(value).encode(), timestamp])
                    asyncio.create_task(telegraf.write(point))
                else:
                    logger.debug("Omitting %s because it is not in the (websocket) controls list", uuid)
        #for uuid, value in message.items():
        #    await self.process_loxone_value_state_item(uuid, value, timestamp)
        #tasks = map(lambda item: self.process_loxone_value_state_item(item[0], item[1], timestamp), message.items())
        #await asyncio.gather(*tasks)

    async def handle_text_messages(self, message, message_type):
        if isinstance(message, dict) and len(message) == 1:
            uuid, message = next(iter(message.items()))
            if uuid in self.grabber_controls:
                control = self.grabber_controls[uuid]
                point = control["pointInflux"].tag("source","grabber").field("Default",get_numeric_value_if_possible(message["value"]))
                if "output0" in message: # At least one additional output is present
                    for key, value in message.items():
                        if "output" in key and "value" in value:
                            point = point.field(str(value.get("name") if value.get("name") else value.get("nr") if value.get("nr") else b'Subdefault'), get_numeric_value_if_possible(value.get("value"))).time(time.time_ns())
                await telegraf.write(point.to_line_protocol().encode())
            else:
                logger.warning("Omitting %s because it is not in the grabber controls list", uuid)

    async def main(self):
        # Initialize telegraf connection and gather controls
        await asyncio.gather(
            telegraf.initialize(), 
            self.init_gather_controls_from_miniserver(),
            self.init_websocket_connection(),
            self.init_grabber())
        await self._shutdown_event.wait()

    async def udpate_controls(self):
        await self.ws_client_initialized.wait()
        await self.grabber_controls_updated.wait()
        asyncio.create_task(self.init_gather_controls_from_miniserver())

    async def get_controls(self):
        return self.controls, self.websocket_controls, self.grabber_controls

    async def init_grabber(self):
        self.grabber = LoxoneGrabber()
        ws_init_task = asyncio.create_task(self.ws_client_initialized.wait())
        grabber_controls_task = asyncio.create_task(self.grabber_controls_updated.wait())
        self._message_handlers[0] = self.handle_text_messages
        self.ws_client.add_message_callback(self.handle_text_messages, message_types=[0])
        # Wait for controls to be gathered and websocket connection to be established
        await asyncio.wait([ws_init_task, grabber_controls_task],return_when=asyncio.ALL_COMPLETED)
        grabber_task = asyncio.create_task(self.grabber.start(self.get_controls))
        self._tasks.add(grabber_task)
        grabber_task.add_done_callback(self._tasks.discard)

    async def init_gather_controls_from_miniserver(self):
        config_xml, loxapp3_json = await load_miniserver_config(config.miniserver.host, config.miniserver.user, config.miniserver.password, persist=True)
        self.loxapp3_json_control = [control.replace("U:", "") for control in loxapp3_json["controls"]]
        new_controls, new_websocket_controls, new_grabber_controls = getControlsFromConfigXML(config_xml)
        new_websocket_controls = config.control.websocket.filter_controls(new_websocket_controls)
        new_grabber_controls = config.control.grabber.filter_controls(new_grabber_controls)
        self.controls.clear()
        self.controls.update(new_controls)
        self.websocket_controls.clear()
        self.websocket_controls.update(new_websocket_controls)
        self.grabber_controls.clear()
        self.grabber_controls.update(new_grabber_controls)
        logger.info("Extracted %d controls, %d websocket controls, %d grabber controls", 
                    len(self.controls), len(self.websocket_controls), len(self.grabber_controls))

        self.grabber_controls_updated.set()
        
    
    async def init_websocket_connection(self):
        try:
            self.ws_client = loxwebsocket
            await self.ws_client.connect(user=config.miniserver.user,password=config.miniserver.password,loxone_url=self.base_url, max_reconnect_attempts=config.miniserver.max_reconnect_attempts)
            # Register callback for specific message types
            self.ws_client.add_message_callback(self.handle_value_states, message_types=[2])
            self.ws_client.add_event_callback(self.udpate_controls, event_types=[LoxWs.EventType.RECONNECTED])
            self.ws_client_initialized.set()
            await self._shutdown_event.wait()
        except LoxoneException as e:
            logger.error("Failed to establish or maintain WebSocket connection: %s", e)
            # Attempts to Reconnect not successfull - shutting down
            raise RuntimeError(f"Failed to establish or maintain WebSocket connection: {e}")  # Exception message - f-string ok
        finally:
            if self.ws_client:
                await self.ws_client.stop()

    async def shutdown(self):
        """Cleanup method to be called on shutdown"""
        logger.info("Initiating shutdown...")
        self._shutdown_event.set()
        
        if self.ws_client:
            await self.ws_client.stop()
        await telegraf.close()
        
        # Cancel all remaining tasks
        tasks = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
        for task in tasks:
            task.cancel()
        
        logger.info("Waiting for %d tasks to complete...", len(tasks))
        await asyncio.gather(*tasks, return_exceptions=True)
        logger.info("Shutdown complete")

async def run_bridge(bridge):
    shutdown_event = asyncio.Event()

    def signal_handler():
        logger.info("Received shutdown signal")
        shutdown_event.set()

    for sig in (signal.SIGTERM, signal.SIGINT):
        asyncio.get_running_loop().add_signal_handler(sig, signal_handler)

    # Start the bridge main task
    main_task = asyncio.create_task(bridge.main())

    # Wait for shutdown signal
    await shutdown_event.wait()

    # Initiate shutdown
    main_task.cancel()
    try:
        await main_task
    except asyncio.CancelledError:
        pass


def main():
    # Get command line arguments including config_dir and data_dir
    initialize_logging()
    
    bridge = LoxInfluxBridge()

    # Set uvloop as the default event loop policy
    asyncio.set_event_loop_policy(uvloop.EventLoopPolicy())

    try:
        asyncio.run(run_bridge(bridge))
    except KeyboardInterrupt:
        pass
    except RuntimeError as e:
        logger.error("RuntimeError: %s", e)
    finally:
        # Run cleanup
        asyncio.run(bridge.shutdown())
        on_exit()


if __name__ == "__main__":
    main() 