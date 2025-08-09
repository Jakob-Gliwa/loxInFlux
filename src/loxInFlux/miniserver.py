from datetime import datetime
import os
import sys
import re
import logging
import struct
import zipfile
import zlib
from io import BytesIO
import os.path
from functools import lru_cache
from influxdb_client import Point
from lxml import etree as ET
from loxInFlux.utils import log_performance, get_loxapp3_json_last_modified
from loxInFlux.config import config
import aioftp  
import aiofiles  
import orjson  
from copy import deepcopy

logger = logging.getLogger(__name__)

_loxapp3_cache_json_last_modified = None

valueplaceholder = b'[valueplaceholder]'
sourceplaceholder = b'[sourceplaceholder]'

# Blacklist for controls that are not capable of being used in the grabber as their API is not consistent with the other controls.
# Currently I'm just aware of the VIRTUALTEXTIN control, but there might be more.
# When VIRTUALTEXTIN is polled by the grabber, the /all command is set as its text value.
# I don't know wether there is a way to get the current value of the control.
SYS_BLACKLIST = ["VIRTUALTEXTIN"]

def remove_bom(xmlstr: str) -> str:
    BOM = "\ufeff"
    return xmlstr[len(BOM):] if xmlstr.startswith(BOM) else xmlstr

def correctXML_removeAttributeDuplicates(xmlstr: str, elemType: str) -> str:
    startpos = 0
    while True:
        foundpos = xmlstr.find(f'<C Type="{elemType}"', startpos)
        if foundpos == -1:
            break
        endpos = xmlstr.find('>', foundpos)
        if endpos == -1:
            break

        tagstr = xmlstr[foundpos+1 : endpos]
        attributes = re.findall(r'(\S+="[^"]*"|\S+=[^"\s]+)', tagstr)
        newattributeArray = []
        seen = {}
        for attrpair in attributes:
            parts = attrpair.split('=', 1)
            if len(parts) != 2:
                continue
            attrname = parts[0]
            if attrname in seen:
                continue
            seen[attrname] = True
            newattributeArray.append(attrpair)

        if len(newattributeArray) < len(attributes):
            fixed = ' '.join(newattributeArray)
            xmlstr = xmlstr[:foundpos+1] + fixed + xmlstr[endpos:]
        startpos = foundpos + 1
    return xmlstr

def extractControls(root, lox_category_room: dict) -> tuple[dict, dict, dict]:
    controls = {}
    visu_controls = {}
    non_visu_controls = {}
    # Objects that are linked to a visible control are implicitly visible too, even if they have no visu attribute themselves
    linkCofVisuControll = set()

    # Controls
    for obj in root.findall('.//C[@Type]'):
        objtype = obj.get("Type", "")
        if objtype.upper() in config.control.type_blacklist:
            continue  # Blacklist -> überspringen

        uid = str(obj.get("U")).encode('utf-8')
        if not uid:
            continue

        catname = ""
        rmname = ""
        visu = ""
        visuPwd = ""
        iodata = obj.find("IoData")
        if iodata is not None:
            cr = iodata.get("Cr")
            pr = iodata.get("Pr")
            if cr and cr in lox_category_room:
                catname = lox_category_room[cr]
            if pr and pr in lox_category_room:
                rmname = lox_category_room[pr]
            visu = iodata.get("Visu", "")
            visuPwd = iodata.get("VisuPwd", "")

        # Normalize flags to real booleans (avoid truthiness of non-empty strings like "false")
        is_visu_control = str(visu).strip().lower() in ("1", "true", "yes")
        is_visu_pwd_required = str(visuPwd).strip().lower() in ("1", "true", "yes")

        if is_visu_control and obj.get("linkC"):
            linkCofVisuControll.update(e.encode('utf-8') for e in obj.get("linkC").split(","))

        # Display-Tag -> Unit
        disp = obj.find("Display")
        unit = ""
        if disp is not None and disp.get("Unit"):
            # remove <...> in der Unit
            unit = re.sub(r'<.*?>', '', disp.get("Unit")).strip()

        # Min/Max
        analog = (obj.get("Analog") == "true")
        #if not analog and obj.get("Analog"):
        #    minval, maxval = (0, 1)
        #else:
        #    minval = obj.get("MinVal", "U") if obj.get("MinVal") else "U"
        #    maxval = obj.get("MaxVal", "U") if obj.get("MaxVal") else "U"

        base_point = Point.from_dict({
                "measurement": obj.get("Title", ""),
                "tags": {
                    "name": obj.get("Title", ""),
                    "description": obj.get("Desc", ""),
                    "uuid": uid.decode('utf-8'),
                    "statstype": int(obj.get("StatsType", 0)),
                    "analog": 1 if analog else 0,
                    "type": objtype,
                    "unit": unit,
                    "category": catname,
                    "room": rmname,
                    "visu": visu,
                    "source": "[sourceplaceholder]",
                    "application": "loxInFlux"
                },
                "fields": {
                },
            })

        point = deepcopy(base_point).field("Default", "[valueplaceholder]").to_line_protocol().encode()

        controls[uid] = {
            "fieldkey": "Default",
            "point": point,
            "point_websocket": point.replace(b"[sourceplaceholder]", b"websocket").replace(b"[valueplaceholder]", b"")[:-2],
            "pointInflux": deepcopy(base_point).field("Default", "[valueplaceholder]"),
            "type": objtype.upper(),
            "visu": is_visu_control,
            "VisuPwd": is_visu_pwd_required,
        }
        if is_visu_control:
            visu_controls[uid] = controls[uid]
        elif objtype.upper() in SYS_BLACKLIST:
            logger.warning(f"Skipping {objtype} with id {uid} because it is not capable of being used in the grabber")
        else:
            non_visu_controls[uid] = controls[uid]

            # Iterate over Co subelements
        for co in obj.findall("Co"):
            co_uid = str(co.get("U")).encode('utf-8')
            if not co_uid:
                continue
            
            point = deepcopy(base_point).tag("subuuid", co_uid.decode('utf-8')).field(co.get("K", ""), "[valueplaceholder]").to_line_protocol().encode()
            #TODO MeterDig states hinzufügen
            controls[co_uid] = {
                "fieldkey": co.get("K", ""),
                "point": point,
                "point_websocket": point.replace(b"[sourceplaceholder]", b"websocket").replace(b"[valueplaceholder]", b"")[:-2],
                "pointInflux": deepcopy(base_point).tag("subuuid", co_uid.decode('utf-8')).field(co.get("K", ""), "[valueplaceholder]"),
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
def extractRoomsAndCategories(root):
    lox_category_room = {}

    # Kategorien und Räume
    for elem in root.xpath('.//C[@Type="Category" or @Type="Place"]'):
        uid = elem.get("U")
        if uid:
            lox_category_room[uid] = elem.get("Title", "")
    logger.debug(f"lox_category_room: {lox_category_room}")
    return lox_category_room

def readXMLstring(xml_path: str):
    if not os.path.exists(xml_path):
        print(f"Fehler: {xml_path} nicht gefunden.")
        sys.exit(1)

    with open(xml_path, "r", encoding="utf-8", errors="replace") as f:
        xmlstr = f.read()
    return xmlstr

@lru_cache(maxsize=10)  # maxsize begrenzt die Anzahl der gecachten Einträge
@log_performance("getControlsFromConfigXML")
def getControlsFromConfigXML(xmlstr: str):
    xmlstr = remove_bom(xmlstr)
    xmlstr = correctXML_removeAttributeDuplicates(xmlstr, "(LoxAIR|LoxAIRDevice|User)")
    
    try:    
        root = ET.fromstring(xmlstr.encode('utf-8'))
        logger.debug(f"XML parsed successfully")
    except ET.XMLSyntaxError as e:
        logger.warning(f"Standard XML parsing failed: {str(e)}")
        logger.warning("Attempting XML parsing with recovery mode for malformed XML")
        
        # Use lxml recovery mode for malformed XML (handles duplicate attributes, encoding issues, etc.)
        parser = ET.XMLParser(recover=True)
        root = ET.fromstring(xmlstr.encode('utf-8'), parser)
        logger.warning("Successfully parsed malformed XML using lxml recovery mode")

    
    lox_category_room = extractRoomsAndCategories(root)
    
    return extractControls(root, lox_category_room)

def parseAndGetControls(xml_path: str):
    xmlstr = readXMLstring(xml_path)
    return getControlsFromConfigXML(xmlstr)


async def load_miniserver_config(ip: str, username: str, password: str, persist: bool = False, use_cache: bool = True) -> tuple[str, dict]:
    """
    Load the most recent version of the currently active configuration file
    and LoxAPP3.json from the Miniserver via FTP.
    
    Returns:
        tuple[str, dict]: A tuple containing (config_content, loxapp3_json)
    """
    global _loxapp3_cache_json_last_modified
    try:
        # Check cache first if enabled
        if use_cache and _loxapp3_cache_json_last_modified and _loxapp3_cache_json_last_modified >= get_loxapp3_json_last_modified():
            cached_files = [f for f in os.listdir(config.paths.data_dir) 
                          if f.startswith('sps_') and f.endswith('.xml')]
            if cached_files:
                latest_cache = sorted(cached_files)[-1]
                cached_xml = os.path.join(config.paths.data_dir, latest_cache)
                cached_json = os.path.join(config.paths.data_dir, 'LoxAPP3.json')
                
                if os.path.exists(cached_xml) and os.path.exists(cached_json):
                    logger.info(f"Using cached configuration from {cached_xml}")
                    logger.info(f"Using cached LoxAPP3.json from {cached_json}")
                    
                    async with aiofiles.open(cached_json, 'rb') as f:
                        json_content = orjson.loads(await f.read())
                        _loxapp3_cache_json_last_modified = datetime.strptime(json_content["lastModified"], "%Y-%m-%d %H:%M:%S")
                    if _loxapp3_cache_json_last_modified >= get_loxapp3_json_last_modified():
                        async with aiofiles.open(cached_xml, 'r', encoding='utf-8') as f:
                            config_content = await f.read()
                        return config_content, json_content
                else:
                    logger.info("No cached configuration files found. Downloading new version.")
                    _loxapp3_cache_json_last_modified = None

        # Connect using aioftp
        async with aioftp.Client.context(ip, user=username, password=password) as client:
            # Change to prog directory
            await client.change_directory("prog")
            
            # Find the most recent configuration file
            filelist = []
            async for path, info in client.list():
                # Convert Path object to string for comparison
                path_str = str(path)
                if path_str.startswith('sps_') and (path_str.endswith('.zip') or path_str.endswith('.LoxCC')):
                    filelist.append(path_str)
            
            if not filelist:
                raise Exception("No configuration files found")
                    
            filename = sorted(filelist)[-1]
            logger.info(f"Selected configuration file: {filename}")
            
            # Download the file
            download_file = BytesIO()
            async with client.download_stream(f"/prog/{filename}") as stream:
                async for block in stream.iter_by_block():
                    download_file.write(block)
            
            download_file.seek(0)

            # Extract and decompress the configuration
            zf = zipfile.ZipFile(download_file)
            with zf.open('sps0.LoxCC') as f:
                header, = struct.unpack('<L', f.read(4))
                if header != 0xaabbccee:
                    raise Exception("Invalid file format")
                    
                compressedSize, uncompressedSize, checksum, = struct.unpack('<LLL', f.read(12))
                data = f.read(compressedSize)
                
                # Decompress the data
                index = 0
                resultStr = bytearray()
                while index < len(data):
                    byte, = struct.unpack('<B', data[index:index+1])
                    index += 1
                    copyBytes = byte >> 4
                    byte &= 0xf
                    
                    if copyBytes == 15:
                        while True:
                            addByte = data[index]
                            copyBytes += addByte
                            index += 1
                            if addByte != 0xff:
                                break
                            
                    if copyBytes > 0:
                        resultStr += data[index:index+copyBytes]
                        index += copyBytes
                        
                    if index >= len(data):
                        break
                        
                    bytesBack, = struct.unpack('<H', data[index:index+2])
                    index += 2
                    bytesBackCopied = 4 + byte
                    
                    if byte == 15:
                        while True:
                            val, = struct.unpack('<B', data[index:index+1])
                            bytesBackCopied += val
                            index += 1
                            if val != 0xff:
                                break
                            
                    while bytesBackCopied > 0:
                        if -bytesBack+1 == 0:
                            resultStr += resultStr[-bytesBack:]
                        else:
                            resultStr += resultStr[-bytesBack:-bytesBack+1]
                        bytesBackCopied -= 1
                        
                if checksum != zlib.crc32(resultStr):
                    raise Exception('Checksum verification failed')
                    
                if len(resultStr) != uncompressedSize:
                    raise Exception(f'Uncompressed filesize mismatch: {len(resultStr)} != {uncompressedSize}')
                    
                config_content = resultStr.decode('utf-8')
            
            # Get the LoxAPP3.json content
            with zf.open('LoxAPP3.json') as f:
                json_content = orjson.loads(f.read())
            
            # Save both files if persist is enabled
            if persist:
                output_xml = os.path.join(config.paths.data_dir, 
                                        f'{filename.replace(".zip", "").replace(".LoxCC", "")}.xml')
                output_json = os.path.join(config.paths.data_dir, 'LoxAPP3.json')
                
                async with aiofiles.open(output_xml, 'w', encoding='utf-8') as f:
                    await f.write(config_content)
                async with aiofiles.open(output_json, 'wb') as f:
                    await f.write(orjson.dumps(json_content))
                    
                logger.info(f"Configuration saved to {output_xml}")
                logger.info(f"LoxAPP3.json saved to {output_json}")
            
            _loxapp3_cache_json_last_modified = datetime.strptime(json_content["lastModified"], "%Y-%m-%d %H:%M:%S")
            return config_content, json_content
                    
    except Exception as e:
        logger.error(f"Error loading miniserver configuration: {str(e)}")
        raise