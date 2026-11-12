"""Builders for the Loxone *LoxCC* container and the config ZIP.

Mirrors what ``loxInFlux.miniserver.load_miniserver_config`` expects:

* The downloaded archive is a **ZIP** containing two members:
  ``sps0.LoxCC`` (the compressed config XML) and ``LoxAPP3.json``.
* ``sps0.LoxCC`` is a little-endian binary blob::

      uint32 header            == 0xAABBCCEE
      uint32 compressedSize    (length of the payload that follows)
      uint32 uncompressedSize  (length of the decompressed XML)
      uint32 crc32             (zlib.crc32 over the *decompressed* bytes)
      bytes  payload           (lz4 block or lz4 frame)

* The payload is either an **lz4 block** (``lz4.block.compress(..,
  store_size=False)``, decompressed with the explicit ``uncompressed_size``) or
  an **lz4 frame** (auto-detected by miniserver via the frame magic). The crc32
  is computed over the *decompressed* XML, and ``len(payload)`` must equal
  ``compressedSize`` (miniserver enforces this strictly).

All builders accept overrides so tests can construct malformed variants
(bad header, wrong sizes, corrupt crc, truncated payload, missing ZIP member).
"""

from __future__ import annotations

import io
import struct
import zipfile
import zlib
from collections.abc import Mapping, Sequence

import lz4.block as _lz4b
import lz4.frame as _lz4f
import orjson as _orjson

__all__ = [
    "LOXCC_HEADER",
    "compress_lz4_block",
    "compress_lz4_frame",
    "is_lz4_frame",
    "build_loxcc_block",
    "decode_loxcc_block",
    "build_loxapp3",
    "build_loxcc_zip",
    "SPS0_LOXCC_NAME",
    "LOXAPP3_JSON_NAME",
]

LOXCC_HEADER = 0xAABBCCEE
SPS0_LOXCC_NAME = "sps0.LoxCC"
LOXAPP3_JSON_NAME = "LoxAPP3.json"


def compress_lz4_block(raw: bytes) -> bytes:
    """Compress as a raw lz4 *block* (no stored size).

    Matches miniserver's ``lz4.block.decompress(data, uncompressed_size=...)``
    call, which requires the size *not* to be embedded in the payload.
    """
    return _lz4b.compress(raw, store_size=False)


def compress_lz4_frame(raw: bytes) -> bytes:
    """Compress as an lz4 *frame* (self-describing; detected via magic bytes)."""
    return _lz4f.compress(raw)


def is_lz4_frame(data: bytes) -> bool:
    """Same lz4-frame detection miniserver uses (incl. skippable frames)."""
    if len(data) < 4:
        return False
    m = int.from_bytes(data[:4], "little")
    return m in (0x184D2204, 0x184C2102) or 0x184D2A50 <= m <= 0x184D2A5F


def _as_bytes(xml: str | bytes) -> bytes:
    return xml.encode("utf-8") if isinstance(xml, str) else bytes(xml)


def build_loxcc_block(
    xml: str | bytes,
    *,
    mode: str = "block",
    header: int = LOXCC_HEADER,
    compressed_size: int | None = None,
    uncompressed_size: int | None = None,
    checksum: int | None = None,
    payload: bytes | None = None,
) -> bytes:
    """Build the ``sps0.LoxCC`` binary blob for ``xml``.

    Args:
        xml: the config XML (str or bytes) to embed.
        mode: ``"block"`` (default) or ``"frame"`` lz4 payload. Ignored when an
            explicit ``payload`` is supplied.
        header: header magic (default ``0xAABBCCEE``); override to test the
            "Invalid file format" path.
        compressed_size: overrides the declared payload length (override to test
            the payload-length-mismatch path).
        uncompressed_size: overrides the declared decompressed length (override
            to test the uncompressed-size-mismatch path).
        checksum: overrides the crc32 (override to test checksum failure).
        payload: supply raw payload bytes directly (e.g. truncated / garbage).

    Returns:
        The little-endian ``sps0.LoxCC`` bytes.
    """
    raw = _as_bytes(xml)
    if payload is None:
        payload = (
            compress_lz4_frame(raw) if mode == "frame" else compress_lz4_block(raw)
        )
    if uncompressed_size is None:
        uncompressed_size = len(raw)
    if compressed_size is None:
        compressed_size = len(payload)
    if checksum is None:
        checksum = zlib.crc32(raw)

    return (
        struct.pack("<L", header)
        + struct.pack("<LLL", compressed_size, uncompressed_size, checksum)
        + payload
    )


def decode_loxcc_block(block: bytes) -> str:
    """Decode a ``sps0.LoxCC`` blob back to XML text (mirrors miniserver).

    Validates header, payload length, crc32 and uncompressed size, then returns
    the decoded UTF-8 string. Raises ``ValueError`` on any mismatch. Handy for
    round-trip assertions in tests.
    """
    if len(block) < 16:
        raise ValueError("LoxCC block too short")
    (header,) = struct.unpack("<L", block[:4])
    if header != LOXCC_HEADER:
        raise ValueError(f"Invalid header: {header:#010x}")
    compressed_size, uncompressed_size, checksum = struct.unpack("<LLL", block[4:16])
    data = block[16 : 16 + compressed_size]
    if len(data) != compressed_size:
        raise ValueError(
            f"Payload length mismatch: got {len(data)}, expected {compressed_size}"
        )

    if is_lz4_frame(data):
        result = _lz4f.decompress(data)
    else:
        result = _lz4b.decompress(data, uncompressed_size=uncompressed_size)

    if checksum != zlib.crc32(result):
        raise ValueError("Checksum verification failed")
    if len(result) != uncompressed_size:
        raise ValueError(
            f"Uncompressed size mismatch: {len(result)} != {uncompressed_size}"
        )
    return result.decode("utf-8")


def build_loxapp3(
    controls: Mapping[str, object] | Sequence[str] | None = None,
    *,
    last_modified: str = "2024-01-01 00:00:00",
    **extra: object,
) -> dict:
    """Build a minimal-but-valid ``LoxAPP3.json`` dict.

    Args:
        controls: either a mapping ``{uuid: control_dict}`` or a sequence of
            uuid strings (converted to a mapping). ``app.py`` iterates the keys
            and strips a leading ``"U:"`` from each, so keys must be strings.
        last_modified: value for ``lastModified`` (``"%Y-%m-%d %H:%M:%S"``);
            miniserver parses this to drive its freshness cache.
        **extra: additional top-level keys merged into the result.

    Returns:
        A JSON-serialisable dict with at least ``lastModified`` and ``controls``.
    """
    if controls is None:
        controls_map: dict[str, object] = {}
    elif isinstance(controls, Mapping):
        controls_map = dict(controls)
    else:
        controls_map = {u: {"uuidAction": u, "name": u} for u in controls}

    payload: dict[str, object] = {
        "lastModified": last_modified,
        "controls": controls_map,
    }
    payload.update(extra)
    return payload


def build_loxcc_zip(
    xml: str | bytes,
    loxapp3: Mapping[str, object] | bytes | None = None,
    *,
    loxcc_name: str = SPS0_LOXCC_NAME,
    json_name: str = LOXAPP3_JSON_NAME,
    extra_files: Mapping[str, bytes] | None = None,
    **block_kwargs: object,
) -> bytes:
    """Build an in-memory ZIP with ``sps0.LoxCC`` + ``LoxAPP3.json``.

    Args:
        xml: config XML embedded in the LoxCC member.
        loxapp3: a dict (json-encoded), pre-encoded ``bytes``, or None
            (a default is built via :func:`build_loxapp3`).
        loxcc_name / json_name: member names - override to test missing/renamed
            members (e.g. ``loxcc_name="wrong.LoxCC"``).
        extra_files: additional ``{name: bytes}`` members to include.
        **block_kwargs: forwarded to :func:`build_loxcc_block` (mode, header,
            sizes, checksum, payload) for malformed-container tests.

    Returns:
        The ZIP archive as ``bytes``.
    """
    block = build_loxcc_block(xml, **block_kwargs)  # type: ignore[arg-type]

    if loxapp3 is None:
        loxapp3 = build_loxapp3()
    if isinstance(loxapp3, (bytes, bytearray)):
        json_bytes = bytes(loxapp3)
    else:
        json_bytes = _orjson.dumps(loxapp3)

    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as zf:
        zf.writestr(loxcc_name, block)
        zf.writestr(json_name, json_bytes)
        for name, content in (extra_files or {}).items():
            zf.writestr(name, content)
    return buffer.getvalue()
