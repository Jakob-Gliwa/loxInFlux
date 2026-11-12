"""Unit tests for the LoxCC LZ4 helpers in ``loxInFlux.miniserver``.

Covers ``_is_lz4_frame`` (frame vs block vs skippable/legacy magic, short input)
and ``_decompress_loxcc_block_lz4`` (block roundtrip, frame roundtrip, and the
error/fallback path that ends in ``ValueError``).
"""

from __future__ import annotations

import pytest
from support import loxcc

from loxInFlux.miniserver import _decompress_loxcc_block_lz4, _is_lz4_frame

_RAW = b"the quick brown fox jumps over the lazy dog " * 40


# --------------------------------------------------------------------------- #
# _is_lz4_frame                                                                #
# --------------------------------------------------------------------------- #


def test_is_lz4_frame_true_for_frame_payload():
    assert _is_lz4_frame(loxcc.compress_lz4_frame(_RAW)) is True


def test_is_lz4_frame_false_for_block_payload():
    assert _is_lz4_frame(loxcc.compress_lz4_block(_RAW)) is False


def test_is_lz4_frame_false_for_short_input():
    assert _is_lz4_frame(b"\x01\x02\x03") is False


def test_is_lz4_frame_true_for_skippable_frame_magic():
    # 0x184D2A50 (little-endian) marks a skippable frame.
    assert _is_lz4_frame(b"\x50\x2a\x4d\x18" + b"\x00\x00\x00\x00") is True


def test_is_lz4_frame_true_for_legacy_frame_magic():
    # 0x184C2102 (little-endian) is the legacy frame magic.
    assert _is_lz4_frame(b"\x02\x21\x4c\x18") is True


# --------------------------------------------------------------------------- #
# _decompress_loxcc_block_lz4                                                  #
# --------------------------------------------------------------------------- #


def test_decompress_block_roundtrip():
    comp = loxcc.compress_lz4_block(_RAW)
    assert _decompress_loxcc_block_lz4(comp, len(_RAW)) == _RAW


def test_decompress_frame_roundtrip():
    comp = loxcc.compress_lz4_frame(_RAW)
    # The frame branch ignores the declared uncompressed size.
    assert _decompress_loxcc_block_lz4(comp, len(_RAW)) == _RAW


def test_decompress_frame_roundtrip_ignores_wrong_size():
    comp = loxcc.compress_lz4_frame(_RAW)
    assert _decompress_loxcc_block_lz4(comp, 999999) == _RAW


def test_decompress_raises_valueerror_on_garbage():
    with pytest.raises(ValueError, match="LZ4 decompression failed"):
        _decompress_loxcc_block_lz4(b"not-lz4-data-garbage-xxxx", 1000)


def test_decompress_block_with_generous_size_still_roundtrips():
    # lz4 block decompression tolerates an oversized target buffer hint.
    comp = loxcc.compress_lz4_block(_RAW)
    assert _decompress_loxcc_block_lz4(comp, len(_RAW) + 1024) == _RAW
