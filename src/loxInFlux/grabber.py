import asyncio
from collections.abc import Callable

from loxwebsocket.lox_ws_api import loxwebsocket

from .config import get_config
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
        cfg = get_config()
        if not cfg.general.grabber:
            logger.info("Grabber ist in der Konfiguration deaktiviert")
            return

        logger.info(
            "Starte Loxone Grabber mit Intervall: %d Sekunden",
            cfg.general.grabber_interval,
        )

        while not self._shutdown_event.is_set():
            try:
                await self._grab_all_values()
                await asyncio.sleep(get_config().general.grabber_interval)
            except Exception as e:
                logger.error("Fehler in der Grabber-Schleife: %s", str(e))
                await asyncio.sleep(5)  # Warte etwas bevor erneut versucht wird

    async def send_command(self, uuid, secured=False):
        # loxwebsocket decodes bytes UUIDs on the unsecured path but interpolates
        # them verbatim on the secured path (sending b'uuid'). Decode once here so
        # both branches receive a plain str UUID.
        uuid = uuid.decode() if isinstance(uuid, bytes) else uuid
        # Begrenze die gleichzeitigen Aufgaben
        async with self.semaphore:
            if self._shutdown_event.is_set():
                return
            try:
                # TODO: Check why SequenceController are not working
                if not secured:
                    await self.ws_client.send_websocket_command(uuid, "all")
                else:
                    await self.ws_client.send_websocket_command_to_visu_password_secured_control(
                        uuid, "all", get_config().miniserver.visu_password
                    )
                logger.debug("Grabber-Befehl für UUID gesendet: %s", uuid)
            except Exception as e:
                logger.error(
                    "Fehler beim Senden des Grabber-Befehls für UUID %s: %s",
                    uuid,
                    str(e),
                )

    async def _grab_all_values(self):
        """Fordert aktuelle Werte für alle Kontrollen in der Grabber-Liste an."""
        if self.ws_client.state != "CONNECTED":
            logger.error("Loxone WebSocket ist nicht verbunden")
            return

        tasks = []
        _, _, grabber_controls = await self.controlGetter()
        # Fork (19.11.2026): Visu-passwortgeschuetzte Controls nur abfragen, wenn ein Visu-Passwort konfiguriert ist.
        # Ohne Passwort verwarf der Miniserver die verschluesselte Abfrage frueher still ("Unrecognized command");
        # seit Firmware 17.3 schliesst er dafuer die ganze Websocket-Verbindung (Code 4020) - die Reconnect-Schleife
        # fuehrte dann zur IP-Sperre des Miniservers und zu einem Datenausfall.
        hat_visu_pw = bool(get_config().miniserver.visu_password)
        uebersprungen = 0
        for uuid, control in grabber_controls.items():
            if control["VisuPwd"] and not hat_visu_pw:
                uebersprungen += 1
                continue
            task = asyncio.create_task(self.send_command(uuid, control["VisuPwd"]))
            tasks.append(task)
        if uebersprungen and not getattr(self, "_visu_hinweis", False):
            logger.info(
                "Grabber: %d visu-passwortgeschuetzte Controls uebersprungen (kein visu_password konfiguriert)",
                uebersprungen,
            )
            self._visu_hinweis = True

        if tasks:
            await asyncio.gather(*tasks)

    async def stop(self):
        """Stoppt die Grabber-Aufgabe."""
        logger.info("Stoppe Loxone Grabber")
        self._shutdown_event.set()
