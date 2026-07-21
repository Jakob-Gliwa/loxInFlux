import os
import os.path
import re
import struct
import sys
import zipfile
import zlib
from copy import deepcopy
from datetime import datetime
from functools import lru_cache
from io import BytesIO

import aiofiles
import aiohttp

# LZ4 Import for efficient decompression
import lz4.block as lz4b
import lz4.frame as lz4f
import orjson
from influxdb_client import Point
from lxml import etree as ET

from loxInFlux.config import config
from loxInFlux.logger import get_lazy_logger
from loxInFlux.utils import (
    _build_base_url,
    get_loxapp3_json_last_modified,
    log_performance,
    native_import_runs,
)

logger = get_lazy_logger(__name__)

_loxapp3_cache_json_last_modified = None

# Matches the timestamped Loxone config archives in /prog, e.g.
# "sps_0252_20260104121455.zip". There is no fixed-name pointer to the active
# config, so we list the directory and pick the newest match.
_CONFIG_FILE_PATTERN = re.compile(r"(sps_\d+_\d+\.(?:zip|LoxCC))")


# ---------------------------------------------------------------------------
# XML parser selection
#
# pygixml (pugixml-backed) parses + XPath-traverses the Loxone config markedly
# faster than lxml, but ships a native build with NO runtime CPU dispatch: on a
# CPU lacking an instruction it was compiled for, even importing it raises
# SIGILL (exit 132) and kills the process (uncatchable in-process). We therefore
# VERIFY the import in a throwaway subprocess first; if that fails - or pygixml
# is absent - we fall back to lxml. The strict pygixml parse additionally falls
# back to lxml's recover=True path for malformed configs. ACTIVE_XML_PARSER /
# XML_PARSER_REASON are reported once at startup by utils.log_runtime_environment().
# ---------------------------------------------------------------------------
_PYGIXML_PROBE = "import pygixml; pygixml.XPathQuery('//C')"

# pygixml is the preferred (default) parser; lxml is only the fallback. We start
# from the lxml baseline and upgrade to pygixml iff the CPU probe AND the import
# both succeed, so the fallback state is defined in exactly one place.
pygixml = None
PygiXMLError = Exception
_PYGIXML_AVAILABLE = False
ACTIVE_XML_PARSER = "lxml"
XML_PARSER_REASON = "pygixml load probe failed (would SIGILL/import error); using lxml"

if native_import_runs(_PYGIXML_PROBE):
    # Breadcrumb: the probe passed, so the in-process import below is safe on
    # this CPU. Logging it leaves a traceable last line should a hard crash
    # (e.g. SIGILL) ever slip through despite the probe.
    logger.info("pygixml load probe passed; importing pygixml ...")
    try:
        import pygixml
        from pygixml import PygiXMLError

        _PYGIXML_AVAILABLE = True
        ACTIVE_XML_PARSER = "pygixml"
        XML_PARSER_REASON = "fast path (load probe passed)"
    except Exception as e:  # pragma: no cover - platform dependent
        XML_PARSER_REASON = f"pygixml import failed: {e}"


def _is_lz4_frame(data: bytes) -> bool:
    """
    Extended LZ4 frame detection including skippable frames.
    """
    if len(data) < 4:
        return False
    m = int.from_bytes(data[:4], "little")
    return m in (0x184D2204, 0x184C2102) or 0x184D2A50 <= m <= 0x184D2A5F


def _decompress_loxcc_block_lz4(data: bytes, uncompressed_size: int) -> bytes:
    """
    LZ4 decompression function for LoxCC blocks.
    Extended automatic detection of LZ4-Frame vs. LZ4-Block.
    Extremely fast compared to manual implementation.
    """
    if _is_lz4_frame(data):
        return lz4f.decompress(data)
    try:
        return lz4b.decompress(data, uncompressed_size=uncompressed_size)
    except Exception as e:
        # last attempt: possibly misidentified
        try:
            return lz4f.decompress(data)
        except Exception:
            raise ValueError(f"LZ4 decompression failed: {e}") from e


valueplaceholder = b"[valueplaceholder]"
sourceplaceholder = b"[sourceplaceholder]"

# Blacklist for controls that are not capable of being used in the grabber as their API is not consistent with the other controls.
# Currently I'm just aware of the VIRTUALTEXTIN control, but there might be more.
# When VIRTUALTEXTIN is polled by the grabber, the /all command is set as its text value.
# I don't know wether there is a way to get the current value of the control.
SYS_BLACKLIST = ["VIRTUALTEXTIN"]


def remove_bom(xmlstr: str) -> str:
    BOM = "\ufeff"
    return xmlstr[len(BOM) :] if xmlstr.startswith(BOM) else xmlstr


def correctXML_removeAttributeDuplicates(xmlstr: str, elemType: str) -> str:
    startpos = 0
    while True:
        foundpos = xmlstr.find(f'<C Type="{elemType}"', startpos)
        if foundpos == -1:
            break
        endpos = xmlstr.find(">", foundpos)
        if endpos == -1:
            break

        tagstr = xmlstr[foundpos + 1 : endpos]
        attributes = re.findall(r'(\S+="[^"]*"|\S+=[^"\s]+)', tagstr)
        newattributeArray = []
        seen = {}
        for attrpair in attributes:
            parts = attrpair.split("=", 1)
            if len(parts) != 2:
                continue
            attrname = parts[0]
            if attrname in seen:
                continue
            seen[attrname] = True
            newattributeArray.append(attrpair)

        if len(newattributeArray) < len(attributes):
            fixed = " ".join(newattributeArray)
            xmlstr = xmlstr[: foundpos + 1] + fixed + xmlstr[endpos:]
        startpos = foundpos + 1
    return xmlstr


class _LxmlBackend:
    """Minimal element access over lxml Elements."""

    @staticmethod
    def controls(root):
        return root.findall(".//C[@Type]")

    @staticmethod
    def rooms(root):
        return root.xpath('.//C[@Type="Category" or @Type="Place"]')

    @staticmethod
    def get(el, key, default=None):
        return el.get(key, default)

    @staticmethod
    def child(el, tag):
        return el.find(tag)

    @staticmethod
    def cos(el):
        return el.findall("Co")


class _PygixmlBackend:
    """Element access over pygixml nodes, matching _LxmlBackend semantics.

    Missing attributes/children map to the same None/default the lxml calls
    return, so extractControls produces byte-identical output on either parser.
    """

    @staticmethod
    def controls(root):
        return [xn.node for xn in root.select_nodes(".//C[@Type]")]

    @staticmethod
    def rooms(root):
        return [
            xn.node
            for xn in root.select_nodes('.//C[@Type="Category" or @Type="Place"]')
        ]

    @staticmethod
    def get(el, key, default=None):
        value = el.attribute(key).value
        return value if value is not None else default

    @staticmethod
    def child(el, tag):
        node = el.child(tag)
        return node if not node.is_null() else None

    @staticmethod
    def cos(el):
        return [c for c in el.children() if c.name == "Co"]


def extractControls(root, lox_category_room: dict, backend) -> tuple[dict, dict, dict]:
    controls = {}
    visu_controls = {}
    non_visu_controls = {}
    # Objects that are linked to a visible control are implicitly visible too, even if they have no visu attribute themselves
    linkCofVisuControll = set()

    # Controls
    for obj in backend.controls(root):
        objtype = backend.get(obj, "Type", "")
        if objtype.upper() in config.control.type_blacklist:
            continue  # Blacklist -> überspringen

        uid = str(backend.get(obj, "U")).encode("utf-8")
        if not uid:
            continue

        catname = ""
        rmname = ""
        visu = ""
        visuPwd = ""
        iodata = backend.child(obj, "IoData")
        if iodata is not None:
            cr = backend.get(iodata, "Cr")
            pr = backend.get(iodata, "Pr")
            if cr and cr in lox_category_room:
                catname = lox_category_room[cr]
            if pr and pr in lox_category_room:
                rmname = lox_category_room[pr]
            visu = backend.get(iodata, "Visu", "")
            visuPwd = backend.get(iodata, "VisuPwd", "")

        # Normalize flags to real booleans (avoid truthiness of non-empty strings like "false")
        is_visu_control = str(visu).strip().lower() in ("1", "true", "yes")
        is_visu_pwd_required = str(visuPwd).strip().lower() in ("1", "true", "yes")

        linkc = backend.get(obj, "linkC")
        if is_visu_control and linkc:
            linkCofVisuControll.update(e.encode("utf-8") for e in linkc.split(","))

        # Display-Tag -> Unit
        disp = backend.child(obj, "Display")
        unit = ""
        disp_unit = backend.get(disp, "Unit") if disp is not None else None
        if disp_unit:
            # remove <...> in der Unit
            unit = re.sub(r"<.*?>", "", disp_unit).strip()

        # Min/Max
        analog = backend.get(obj, "Analog") == "true"
        # if not analog and obj.get("Analog"):
        #    minval, maxval = (0, 1)
        # else:
        #    minval = obj.get("MinVal", "U") if obj.get("MinVal") else "U"
        #    maxval = obj.get("MaxVal", "U") if obj.get("MaxVal") else "U"

        base_point = Point.from_dict(
            {
                "measurement": backend.get(obj, "Title", ""),
                "tags": {
                    "name": backend.get(obj, "Title", ""),
                    "description": backend.get(obj, "Desc", ""),
                    "uuid": uid.decode("utf-8"),
                    "statstype": int(backend.get(obj, "StatsType", 0)),
                    "analog": 1 if analog else 0,
                    "type": objtype,
                    "unit": unit,
                    "category": catname,
                    "room": rmname,
                    "visu": visu,
                    "source": "[sourceplaceholder]",
                    "application": "loxInFlux",
                },
                "fields": {},
            }
        )

        point = (
            deepcopy(base_point)
            .field("Default", "[valueplaceholder]")
            .to_line_protocol()
            .encode()
        )

        controls[uid] = {
            "fieldkey": "Default",
            "point": point,
            "point_websocket": point.replace(
                b"[sourceplaceholder]", b"websocket"
            ).replace(b"[valueplaceholder]", b"")[:-2],
            "pointInflux": deepcopy(base_point).field("Default", "[valueplaceholder]"),
            "type": objtype.upper(),
            "visu": is_visu_control,
            "VisuPwd": is_visu_pwd_required,
        }
        if is_visu_control:
            visu_controls[uid] = controls[uid]
        elif objtype.upper() in SYS_BLACKLIST:
            logger.warning(
                "Skipping %s with id %s because it is not capable of being used in the grabber",
                objtype,
                uid,
            )
        else:
            non_visu_controls[uid] = controls[uid]

            # Iterate over Co subelements
        for co in backend.cos(obj):
            co_uid = str(backend.get(co, "U")).encode("utf-8")
            if not co_uid:
                continue

            cokey = backend.get(co, "K", "")
            point = (
                deepcopy(base_point)
                .tag("subuuid", co_uid.decode("utf-8"))
                .field(cokey, "[valueplaceholder]")
                .to_line_protocol()
                .encode()
            )
            # TODO MeterDig states hinzufügen
            controls[co_uid] = {
                "fieldkey": cokey,
                "point": point,
                "point_websocket": point.replace(
                    b"[sourceplaceholder]", b"websocket"
                ).replace(b"[valueplaceholder]", b"")[:-2],
                "pointInflux": deepcopy(base_point)
                .tag("subuuid", co_uid.decode("utf-8"))
                .field(cokey, "[valueplaceholder]"),
                "type": objtype.upper(),
                "parent_uuid": uid,
            }
            if is_visu_control:
                visu_controls[co_uid] = controls[co_uid]

        # Grabber needs only parent uuid
        for uid in linkCofVisuControll:
            if uid in non_visu_controls:
                visu_controls[uid] = non_visu_controls[uid]
                del non_visu_controls[uid]
    return controls, visu_controls, non_visu_controls


@log_performance("extractRoomsAndCategories")
def extractRoomsAndCategories(root, backend):
    lox_category_room = {}

    # Kategorien und Räume
    for elem in backend.rooms(root):
        uid = backend.get(elem, "U")
        if uid:
            lox_category_room[uid] = backend.get(elem, "Title", "")
    logger.debug("lox_category_room: %s", lox_category_room)
    return lox_category_room


def readXMLstring(xml_path: str):
    if not os.path.exists(xml_path):
        print(f"Fehler: {xml_path} nicht gefunden.")
        sys.exit(1)

    with open(xml_path, encoding="utf-8", errors="replace") as f:
        xmlstr = f.read()
    return xmlstr


def _parse_lxml_root(xmlstr: str):
    """Parse via lxml, retrying in recover mode for malformed configs."""
    try:
        root = ET.fromstring(xmlstr.encode("utf-8"))
        logger.debug("XML parsed successfully")
        return root
    except ET.XMLSyntaxError as e:
        logger.warning("Standard XML parsing failed: %s", str(e))
        logger.warning("Attempting XML parsing with recovery mode for malformed XML")

        # Use lxml recovery mode for malformed XML (handles duplicate attributes, encoding issues, etc.)
        parser = ET.XMLParser(recover=True)
        root = ET.fromstring(xmlstr.encode("utf-8"), parser)
        logger.warning("Successfully parsed malformed XML using lxml recovery mode")
        return root


@lru_cache(maxsize=10)  # maxsize begrenzt die Anzahl der gecachten Einträge
@log_performance("getControlsFromConfigXML")
def getControlsFromConfigXML(xmlstr: str):
    xmlstr = remove_bom(xmlstr)
    xmlstr = correctXML_removeAttributeDuplicates(xmlstr, "(LoxAIR|LoxAIRDevice|User)")

    # Fast path: strict pygixml parse + XPath traversal. On a strict-parse error
    # (structurally malformed XML) - or when pygixml is unavailable - fall back
    # to the lxml recover=True path, which yields identical output for the normal
    # case and tolerates the malformed cases pygixml rejects.
    if _PYGIXML_AVAILABLE:
        try:
            doc = pygixml.parse_string(xmlstr)
            root = doc.root
            lox_category_room = extractRoomsAndCategories(root, _PygixmlBackend)
            return extractControls(root, lox_category_room, _PygixmlBackend)
        except PygiXMLError as e:
            logger.warning(
                "pygixml strict parse failed (%s); falling back to lxml recover mode",
                e,
            )

    root = _parse_lxml_root(xmlstr)
    lox_category_room = extractRoomsAndCategories(root, _LxmlBackend)
    return extractControls(root, lox_category_room, _LxmlBackend)


def parseAndGetControls(xml_path: str):
    xmlstr = readXMLstring(xml_path)
    return getControlsFromConfigXML(xmlstr)


async def load_miniserver_config(
    username: str, password: str, persist: bool = False, use_cache: bool = True
) -> tuple[str, dict]:
    """
    Load the most recent version of the currently active configuration file
    and LoxAPP3.json from the Miniserver via the HTTP filesystem API.

    Uses ``/dev/fslist/prog/`` to find the newest ``sps_<ver>_<ts>.zip`` archive
    and ``/dev/fsget/prog/<file>`` to download it (both plain HTTP, BasicAuth).
    This replaces the previous FTP path: it reuses the HTTP port we already talk
    to, drops the aioftp dependency and keeps working when FTP is disabled on
    the Miniserver.

    Returns:
        tuple[str, dict]: A tuple containing (config_content, loxapp3_json)
    """
    global _loxapp3_cache_json_last_modified
    try:
        # Query the miniserver once for the current LoxAPP3.json modification
        # time and reuse it for the freshness checks below. Returns None when the
        # miniserver is unreachable / responds non-200, in which case we skip the
        # cache and fall through to a fresh download.
        remote_loxapp3_last_modified = (
            await get_loxapp3_json_last_modified()
            if use_cache and _loxapp3_cache_json_last_modified
            else None
        )

        # Check cache first if enabled
        if (
            remote_loxapp3_last_modified is not None
            and _loxapp3_cache_json_last_modified >= remote_loxapp3_last_modified
        ):
            cached_files = [
                f
                for f in os.listdir(config.paths.data_dir)
                if f.startswith("sps_") and f.endswith(".xml")
            ]
            if cached_files:
                latest_cache = sorted(cached_files)[-1]
                cached_xml = os.path.join(config.paths.data_dir, latest_cache)
                cached_json = os.path.join(config.paths.data_dir, "LoxAPP3.json")

                # One-shot local filesystem checks during startup; the blocking
                # cost is negligible and not worth async-wrapping the whole path.
                if os.path.exists(cached_xml) and os.path.exists(cached_json):  # noqa: ASYNC240
                    logger.info("Using cached configuration from %s", cached_xml)
                    logger.info("Using cached LoxAPP3.json from %s", cached_json)

                    async with aiofiles.open(cached_json, "rb") as f:
                        json_content = orjson.loads(await f.read())
                        _loxapp3_cache_json_last_modified = datetime.strptime(
                            json_content["lastModified"], "%Y-%m-%d %H:%M:%S"
                        )
                    if (
                        _loxapp3_cache_json_last_modified
                        >= remote_loxapp3_last_modified
                    ):
                        async with aiofiles.open(cached_xml, encoding="utf-8") as f:
                            config_content = await f.read()
                        return config_content, json_content
                else:
                    logger.info(
                        "No cached configuration files found. Downloading new version."
                    )
                    _loxapp3_cache_json_last_modified = None

        # Download the newest config archive via the HTTP filesystem API.
        base_url = _build_base_url()
        auth = aiohttp.BasicAuth(login=username, password=password)
        timeout = aiohttp.ClientTimeout(total=30)
        async with aiohttp.ClientSession(auth=auth, timeout=timeout) as session:
            # List /prog (plain-text listing, one entry per line) and pick the
            # newest sps_<ver>_<ts>.(zip|LoxCC).
            async with session.get(
                f"{base_url}/dev/fslist/prog/",
                allow_redirects=True,
                ssl=False,  # Disable SSL verification
            ) as response:
                response.raise_for_status()
                listing = await response.text()

            filelist = sorted(set(_CONFIG_FILE_PATTERN.findall(listing)))
            if not filelist:
                raise Exception("No configuration files found")

            filename = filelist[-1]
            logger.info("Selected configuration file: %s", filename)

            # Download the selected archive
            async with session.get(
                f"{base_url}/dev/fsget/prog/{filename}",
                allow_redirects=True,
                ssl=False,  # Disable SSL verification
            ) as response:
                response.raise_for_status()
                raw = await response.read()

        # Extract and decompress the configuration
        download_file = BytesIO(raw)
        zf = zipfile.ZipFile(download_file)
        with zf.open("sps0.LoxCC") as f:
            (header,) = struct.unpack("<L", f.read(4))
            if header != 0xAABBCCEE:
                raise Exception("Invalid file format")

            (
                compressedSize,
                uncompressedSize,
                checksum,
            ) = struct.unpack("<LLL", f.read(12))
            data = f.read(compressedSize)

            # Strict payload length validation
            if len(data) != compressedSize:
                raise Exception(
                    f"Payload length mismatch: got {len(data)}, expected {compressedSize}"
                )

            # Decompress using LZ4 library (much faster than manual implementation)
            logger.debug("Using LZ4 decompression")
            resultStr = _decompress_loxcc_block_lz4(data, uncompressedSize)

            if checksum != zlib.crc32(resultStr):
                raise Exception("Checksum verification failed")

            if len(resultStr) != uncompressedSize:
                raise Exception(
                    f"Uncompressed filesize mismatch: {len(resultStr)} != {uncompressedSize}"
                )

            config_content = resultStr.decode("utf-8")

        # Get the LoxAPP3.json content
        with zf.open("LoxAPP3.json") as f:
            json_content = orjson.loads(f.read())

        # Save both files if persist is enabled
        if persist:
            output_xml = os.path.join(
                config.paths.data_dir,
                f"{filename.replace('.zip', '').replace('.LoxCC', '')}.xml",
            )
            output_json = os.path.join(config.paths.data_dir, "LoxAPP3.json")

            async with aiofiles.open(output_xml, "w", encoding="utf-8") as f:
                await f.write(config_content)
            async with aiofiles.open(output_json, "wb") as f:
                await f.write(orjson.dumps(json_content))

            logger.info("Configuration saved to %s", output_xml)
            logger.info("LoxAPP3.json saved to %s", output_json)

        _loxapp3_cache_json_last_modified = datetime.strptime(
            json_content["lastModified"], "%Y-%m-%d %H:%M:%S"
        )
        return config_content, json_content

    except Exception as e:
        logger.error("Error loading miniserver configuration: %s", str(e))
        raise
