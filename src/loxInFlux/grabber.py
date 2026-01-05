import asyncio
import logging
from loxwebsocket.lox_ws_api import loxwebsocket
from typing import Callable
from .config import config
from .logger import get_lazy_logger

logger = get_lazy_logger(__name__)

class LoxoneGrabber:
    def __init__(self):
        self._shutdown_event = asyncio.Event()
        self.semaphore = asyncio.Semaphore(10)
    
    async def start(self, controls_getter: Callable[[], tuple[dict, dict, dict]]):
        self.ws_client = loxwebsocket
        self.controlGetter = controls_getter    
        """Start the grabber task."""
        if not config.general.grabber:
            logger.info("Grabber ist in der Konfiguration deaktiviert")
            return

        logger.info("Starte Loxone Grabber mit Intervall: %d Sekunden", 
                   config.general.grabber_interval)
        
        while not self._shutdown_event.is_set():
            try:
                await self._grab_all_values()
                await asyncio.sleep(config.general.grabber_interval)
            except Exception as e:
                logger.error("Fehler in der Grabber-Schleife: %s", str(e))
                await asyncio.sleep(5)  # Warte etwas bevor erneut versucht wird

    async def send_command(self, uuid, secured=False):
          # Begrenze die gleichzeitigen Aufgaben
        async with self.semaphore:
            if self._shutdown_event.is_set():
                return
            try:
                # TODO: Check why SequenceController are not working
                if not secured:
                    await self.ws_client.send_websocket_command(uuid, "all")
                else:
                    await self.ws_client.send_command_to_visu_password_secured_control(uuid, "all", config.miniserver.visu_password)
                logger.debug("Grabber-Befehl für UUID gesendet: %s", uuid)
            except Exception as e:
                logger.error("Fehler beim Senden des Grabber-Befehls für UUID %s: %s", 
                                uuid, str(e))

    async def _grab_all_values(self):
        """Fordert aktuelle Werte für alle Kontrollen in der Grabber-Liste an."""
        if not self.ws_client.state == "CONNECTED":
            logger.error("Loxone WebSocket ist nicht verbunden")
            return
        
        tasks = []
        _,_,grabber_controls = await self.controlGetter()
        for uuid, control in grabber_controls.items():
            task = asyncio.create_task(self.send_command(uuid, control["VisuPwd"]))
            tasks.append(task)
        
        if tasks:
            await asyncio.gather(*tasks)

    async def stop(self):
        """Stoppt die Grabber-Aufgabe."""
        logger.info("Stoppe Loxone Grabber")
        self._shutdown_event.set()