import asyncio
import contextlib
import signal
import time
from copy import deepcopy

import orjson
import uvloop
from loxwebsocket.exceptions import LoxoneException
from loxwebsocket.lox_ws_api import LoxWs, loxwebsocket

from loxInFlux.argparser import get_args
from loxInFlux.miniserver import getControlsFromConfigXML, load_miniserver_config

from .config import get_config, init_config
from .grabber import LoxoneGrabber
from .logger import get_lazy_logger, initialize_logging
from .telegraf import get_telegraf, init_telegraf
from .utils import (
    _build_base_url,
    get_numeric_value_if_possible,
    log_runtime_environment,
)

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
        cfg = get_config()
        self.formatter = f"{{:.{cfg.general.rounding_precision if cfg.general.round_floats else 15}f}}".format

    async def handle_value_states(self, message, message_type):
        """Handler for value state messages (type 2)"""
        timestamp = b" " + orjson.dumps(time.time_ns())

        for uuid, value in message.items():
            try:
                # Round float values if configured
                point = b"".join(
                    [
                        self.websocket_controls[uuid]["point_websocket"],
                        self.formatter(value).encode(),
                        timestamp,
                    ]
                )
                # Await the write instead of firing an untracked task: this
                # gives natural backpressure (a slow writer throttles intake
                # instead of piling up orphan tasks), avoids interleaved drains
                # on the shared TCP writer, and ensures a failing write surfaces
                # via the writer's own error handling rather than as a lost
                # "task exception was never retrieved".
                await get_telegraf().write(point)
            except KeyError:
                if uuid in self.controls:
                    logger.warning(
                        "%s not in initial websocket controls list. It is in the overall controls though. adding it to websocket controls.",
                        uuid,
                    )
                    self.websocket_controls[uuid] = self.controls[uuid]
                    point = b"".join(
                        [
                            self.websocket_controls[uuid]["point_websocket"],
                            self.formatter(value).encode(),
                            timestamp,
                        ]
                    )
                    await get_telegraf().write(point)
                else:
                    logger.debug(
                        "Omitting %s because it is not in the (websocket) controls list",
                        uuid,
                    )
        # for uuid, value in message.items():
        #    await self.process_loxone_value_state_item(uuid, value, timestamp)
        # tasks = map(lambda item: self.process_loxone_value_state_item(item[0], item[1], timestamp), message.items())
        # await asyncio.gather(*tasks)

    async def handle_text_messages(self, message, message_type):
        if isinstance(message, dict) and len(message) == 1:
            uuid, message = next(iter(message.items()))
            if uuid in self.grabber_controls:
                control = self.grabber_controls[uuid]
                # Work on a fresh copy of the per-control template Point:
                # ``getControlsFromConfigXML`` is lru_cached and hands back the
                # same object graph on every call, so mutating the stored
                # ``pointInflux`` in place would (a) accumulate multi-output
                # fields across messages and (b) poison the cache for other
                # tasks. The grabber path is periodic, so the deepcopy is cheap.
                point = (
                    deepcopy(control["pointInflux"])
                    .tag("source", "grabber")
                    .field("Default", get_numeric_value_if_possible(message["value"]))
                )
                if "output0" in message:  # At least one additional output is present
                    for key, value in message.items():
                        if "output" in key and "value" in value:
                            point = point.field(
                                str(
                                    value.get("name")
                                    if value.get("name")
                                    else value.get("nr")
                                    if value.get("nr")
                                    else b"Subdefault"
                                ),
                                get_numeric_value_if_possible(value.get("value")),
                            ).time(time.time_ns())
                await get_telegraf().write(point.to_line_protocol().encode())
            else:
                logger.warning(
                    "Omitting %s because it is not in the grabber controls list", uuid
                )

    async def main(self):
        # Initialize telegraf connection and gather controls
        await asyncio.gather(
            get_telegraf().initialize(),
            self.init_gather_controls_from_miniserver(),
            self.init_websocket_connection(),
            self.init_grabber(),
        )
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
        grabber_controls_task = asyncio.create_task(
            self.grabber_controls_updated.wait()
        )
        self._message_handlers[0] = self.handle_text_messages
        self.ws_client.add_message_callback(
            self.handle_text_messages, message_types=[0]
        )
        # Wait for controls to be gathered and websocket connection to be established
        await asyncio.wait(
            [ws_init_task, grabber_controls_task], return_when=asyncio.ALL_COMPLETED
        )
        grabber_task = asyncio.create_task(self.grabber.start(self.get_controls))
        self._tasks.add(grabber_task)
        grabber_task.add_done_callback(self._tasks.discard)

    async def init_gather_controls_from_miniserver(self):
        cfg = get_config()
        config_xml, loxapp3_json = await load_miniserver_config(
            cfg.miniserver.user,
            cfg.miniserver.password,
            persist=True,
        )
        self.loxapp3_json_control = [
            control.replace("U:", "") for control in loxapp3_json["controls"]
        ]
        new_controls, new_websocket_controls, new_grabber_controls = (
            getControlsFromConfigXML(config_xml)
        )
        new_websocket_controls = cfg.control.websocket.filter_controls(
            new_websocket_controls
        )
        new_grabber_controls = cfg.control.grabber.filter_controls(new_grabber_controls)
        self.controls.clear()
        self.controls.update(new_controls)
        self.websocket_controls.clear()
        self.websocket_controls.update(new_websocket_controls)
        self.grabber_controls.clear()
        self.grabber_controls.update(new_grabber_controls)
        logger.info(
            "Extracted %d controls, %d websocket controls, %d grabber controls",
            len(self.controls),
            len(self.websocket_controls),
            len(self.grabber_controls),
        )

        self.grabber_controls_updated.set()

    async def init_websocket_connection(self):
        try:
            self.ws_client = loxwebsocket
            cfg = get_config()
            await self.ws_client.connect(
                user=cfg.miniserver.user,
                password=cfg.miniserver.password,
                loxone_url=self.base_url,
                max_reconnect_attempts=cfg.miniserver.max_reconnect_attempts,
            )
            # Register callback for specific message types
            self.ws_client.add_message_callback(
                self.handle_value_states, message_types=[2]
            )
            self.ws_client.add_event_callback(
                self.udpate_controls, event_types=[LoxWs.EventType.RECONNECTED]
            )
            self.ws_client_initialized.set()
            await self._shutdown_event.wait()
        except LoxoneException as e:
            logger.error("Failed to establish or maintain WebSocket connection: %s", e)
            # Attempts to Reconnect not successfull - shutting down
            raise RuntimeError(
                f"Failed to establish or maintain WebSocket connection: {e}"
            ) from e  # Exception message - f-string ok
        finally:
            if self.ws_client:
                await self.ws_client.stop()

    async def shutdown(self):
        """Cleanup method to be called on shutdown.

        Idempotent and fault-tolerant: an exception while stopping the
        websocket or closing telegraf must not abort the rest of the cleanup,
        and calling this more than once is safe. Only the tasks the bridge
        started itself (``self._tasks``) are cancelled - never every task on
        the loop, which would also kill unrelated tasks and the supervisor.
        """
        logger.info("Initiating shutdown...")
        self._shutdown_event.set()

        if self.ws_client:
            try:
                await self.ws_client.stop()
            except Exception as e:
                logger.error("Error stopping websocket client: %s", e)

        try:
            await get_telegraf().close()
        except Exception as e:
            logger.error("Error closing telegraf: %s", e)

        # Cancel only the bridge-owned tasks (grabber, re-gather, ...); a bare
        # ``asyncio.all_tasks()`` sweep would cancel unrelated tasks too.
        current = asyncio.current_task()
        tasks = [t for t in self._tasks if t is not current and not t.done()]
        for task in tasks:
            task.cancel()

        if tasks:
            logger.info("Waiting for %d bridge task(s) to complete...", len(tasks))
            await asyncio.gather(*tasks, return_exceptions=True)
        logger.info("Shutdown complete")


async def run_bridge(bridge):
    shutdown_event = asyncio.Event()

    def signal_handler():
        logger.info("Received shutdown signal")
        shutdown_event.set()

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, signal_handler)

    # Start the bridge main task
    main_task = asyncio.create_task(bridge.main())
    shutdown_wait = asyncio.create_task(shutdown_event.wait())

    # Return as soon as EITHER a shutdown signal arrives OR main() finishes on
    # its own. Waiting on the signal alone would hang forever if main() failed
    # early (e.g. the websocket connection could never be established).
    await asyncio.wait({main_task, shutdown_wait}, return_when=asyncio.FIRST_COMPLETED)

    shutdown_wait.cancel()
    with contextlib.suppress(asyncio.CancelledError):
        await shutdown_wait

    # Signal-driven shutdown: main() is still running, cancel it cleanly. If
    # main() already finished (early failure), re-await it to surface its
    # exception - main() catches RuntimeError upstream and still runs cleanup.
    if not main_task.done():
        main_task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
        await main_task


def main():
    # Explicitly wire the process singletons in dependency order: the config
    # must be loaded before logging (level/format) and before the Telegraf
    # writer (protocol) are constructed. Nothing is initialized at import time.
    init_config()
    initialize_logging(get_args().log_level)

    # Emit the runtime environment banner once, right after logging is set up.
    log_runtime_environment()

    init_telegraf()
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
