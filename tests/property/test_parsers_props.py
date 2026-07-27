"""Property-based tests (Hypothesis) for loxInFlux's pure parsers/converters.

Covers, mirroring the loxwebsocket property-test idioms, but asserting real
*invariants* rather than mere crash-freedom:

* ``miniserver._decompress_loxcc_block_lz4``: lz4 block/frame roundtrip against
  arbitrary bytes compressed via the ``support.loxcc`` helpers.
* ``miniserver._is_lz4_frame``: classifies by leading magic - it recognises the
  standard/legacy/skippable frame magics (and anything ``compress_lz4_frame``
  produces) and rejects every other 4-byte prefix.
* ``utils.get_numeric_value_if_possible``: its int/float/passthrough result
  matches Python's own ``int``/``float`` coercion funnel exactly.
"""

from __future__ import annotations

import math

from hypothesis import assume, given
from hypothesis import strategies as st
from support.loxcc import compress_lz4_block, compress_lz4_frame

from loxInFlux.config import get_config
from loxInFlux.miniserver import _decompress_loxcc_block_lz4, _is_lz4_frame
from loxInFlux.utils import get_numeric_value_if_possible


# --------------------------------------------------------------------------- #
# lz4 roundtrip                                                               #
# --------------------------------------------------------------------------- #
@given(st.binary(max_size=4096))
def test_lz4_block_roundtrip(raw):
    """block-compress -> _decompress_loxcc_block_lz4 == original."""
    compressed = compress_lz4_block(raw)
    assert _decompress_loxcc_block_lz4(compressed, len(raw)) == raw


@given(st.binary(max_size=4096))
def test_lz4_frame_roundtrip(raw):
    """frame-compress -> _decompress_loxcc_block_lz4 == original (size ignored)."""
    compressed = compress_lz4_frame(raw)
    assert _decompress_loxcc_block_lz4(compressed, len(raw)) == raw


# --------------------------------------------------------------------------- #
# _is_lz4_frame classification invariant                                      #
# --------------------------------------------------------------------------- #
# The magics miniserver treats as an lz4 *frame*: standard (0x184D2204), legacy
# (0x184C2102) and the 16 skippable-frame magics (0x184D2A50..0x184D2A5F).
_FRAME_MAGICS = (0x184D2204, 0x184C2102, *range(0x184D2A50, 0x184D2A60))


@given(st.sampled_from(_FRAME_MAGICS), st.binary(max_size=64))
def test_is_lz4_frame_recognises_frame_magics(magic, tail):
    """Every recognised magic (little-endian) is classified as a frame."""
    data = magic.to_bytes(4, "little") + tail
    assert _is_lz4_frame(data) is True


@given(st.integers(min_value=0, max_value=0xFFFFFFFF), st.binary(max_size=64))
def test_is_lz4_frame_rejects_non_frame_prefixes(word, tail):
    """Any 4-byte prefix that is not a known magic is rejected."""
    assume(word not in (0x184D2204, 0x184C2102))
    assume(not 0x184D2A50 <= word <= 0x184D2A5F)
    data = word.to_bytes(4, "little") + tail
    assert _is_lz4_frame(data) is False


@given(st.binary(max_size=3))
def test_is_lz4_frame_false_for_short_input(data):
    """Fewer than 4 bytes can never carry a magic - always False."""
    assert _is_lz4_frame(data) is False


@given(st.binary(max_size=4096))
def test_frame_compressed_payload_is_classified_as_frame(raw):
    """Whatever ``compress_lz4_frame`` emits must be detected as a frame (it
    always starts with the standard frame magic)."""
    assert _is_lz4_frame(compress_lz4_frame(raw)) is True


# --------------------------------------------------------------------------- #
# get_numeric_value_if_possible                                               #
# --------------------------------------------------------------------------- #
def _looks_numeric(text: str) -> bool:
    """True iff ``int`` or ``float`` would accept *text* (same funnel the SUT uses)."""
    try:
        int(text)
        return True
    except ValueError:
        pass
    try:
        float(text)
        return True
    except ValueError:
        return False


@given(st.integers(min_value=-(10**18), max_value=10**18))
def test_integer_strings_coerce_to_int(n):
    result = get_numeric_value_if_possible(str(n))
    assert result == n
    assert isinstance(result, int)


@given(st.floats(allow_nan=False, allow_infinity=False))
def test_float_strings_coerce_to_float(f):
    # round_floats is off in the fixture config; pin it so the result is exact.
    get_config().general.round_floats = False
    result = get_numeric_value_if_possible(repr(f))
    assert result == f


@given(st.text(max_size=64))
def test_non_numeric_text_passes_through(text):
    assume(not _looks_numeric(text))
    result = get_numeric_value_if_possible(text)
    assert result is text


@given(st.text(max_size=64))
def test_numeric_coercion_matches_python_semantics(text):
    """Real invariant (not just crash-freedom): the result equals what Python's
    own ``int``/``float`` funnel yields - ``int`` first, then ``float``, else the
    original string returned unchanged."""
    get_config().general.round_floats = False  # pin: no rounding, exact results
    result = get_numeric_value_if_possible(text)

    try:
        expected_int = int(text)
    except ValueError:
        pass
    else:
        assert isinstance(result, int)
        assert result == expected_int
        return

    try:
        expected_float = float(text)
    except ValueError:
        assert result is text
    else:
        assert isinstance(result, float)
        if math.isnan(expected_float):
            assert math.isnan(result)  # NaN != NaN, so compare structurally
        else:
            assert result == expected_float
