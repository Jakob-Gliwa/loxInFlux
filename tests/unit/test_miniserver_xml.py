"""Unit tests for the XML handling in ``loxInFlux.miniserver``.

Covers ``remove_bom``, ``extractRoomsAndCategories`` and
``getControlsFromConfigXML``/``extractControls`` (including recovery of malformed
and duplicate-attribute XML).

REGRESSIONS exercised here:
  * VIRTUALTEXTIN controls must be parsed into the full control set but kept OUT
    of the grabber (non-visu) bucket (``SYS_BLACKLIST`` handling).
  * Malformed / duplicate-attribute XML must be parsed instead of crashing, on
    BOTH parser backends (pygixml fast path + lxml ``recover=True`` fallback).
"""

from __future__ import annotations

import sys

import pytest
from lxml import etree
from support import loxone_xml

from loxInFlux.config import get_config
from loxInFlux.miniserver import (
    _LxmlBackend,
    extractControls,
    extractRoomsAndCategories,
    getControlsFromConfigXML,
    remove_bom,
)

# Reach the module object (with its backend globals) so a test can force the lxml
# fallback path.
_miniserver_mod = sys.modules["loxInFlux.miniserver"]

# The parser-parity tests only make sense when pygixml is the *active* backend:
# they compare the pygixml fast path against the forced lxml fallback. If pygixml
# is unavailable on this system the "fast path" already runs on lxml, so forcing
# lxml would compare lxml-with-lxml and pass trivially (false positive). Skip
# instead of certifying a comparison that never exercises pygixml.
# ``pygixml_active()`` (not the raw global) resolves the probe, which is deferred
# out of import time - reading the global here would always see the lxml baseline.
requires_pygixml = pytest.mark.skipif(
    not _miniserver_mod.pygixml_active(),
    reason="pygixml not the active backend (fast path == lxml); parity would be lxml-vs-lxml",
)


def _force_lxml_fallback(monkeypatch) -> None:
    """Pin the lxml ``recover=True`` path for the rest of the test.

    ``_pygixml_resolved`` is pinned too: otherwise ``_ensure_pygixml()`` inside
    ``getControlsFromConfigXML`` could still run the probe and upgrade the backend
    back to pygixml, silently undoing the forced fallback.
    """
    monkeypatch.setattr(_miniserver_mod, "_pygixml_resolved", True)
    monkeypatch.setattr(_miniserver_mod, "_PYGIXML_AVAILABLE", False)


# --------------------------------------------------------------------------- #
# remove_bom                                                                   #
# --------------------------------------------------------------------------- #


def test_remove_bom_strips_leading_bom():
    assert remove_bom("\ufeff<xml/>") == "<xml/>"


def test_remove_bom_leaves_bom_free_string_untouched():
    assert remove_bom("<xml/>") == "<xml/>"


def test_remove_bom_only_removes_leading_bom():
    # A BOM not at the start is preserved.
    assert remove_bom("a\ufeffb") == "a\ufeffb"


def test_remove_bom_via_builder_with_bom():
    xml = loxone_xml.build_config_xml(bom=True)
    assert xml.startswith("\ufeff")
    assert not remove_bom(xml).startswith("\ufeff")


# --------------------------------------------------------------------------- #
# extractRoomsAndCategories (direct, via the lxml backend)                     #
# --------------------------------------------------------------------------- #


def test_extract_rooms_and_categories_maps_uuid_to_title():
    cat = loxone_xml.Category(title="Heizung")
    room = loxone_xml.Room(title="Wohnzimmer")
    xml = loxone_xml.build_config_xml(rooms=[room], categories=[cat])
    root = etree.fromstring(xml.encode("utf-8"))

    mapping = extractRoomsAndCategories(root, _LxmlBackend)

    assert mapping[cat.uuid] == "Heizung"
    assert mapping[room.uuid] == "Wohnzimmer"


# --------------------------------------------------------------------------- #
# getControlsFromConfigXML / extractControls                                   #
# --------------------------------------------------------------------------- #


def test_parse_control_keys_are_uuid_bytes():
    ctrl = loxone_xml.Control(type="InfoOnlyAnalog", title="Temp", analog=True)
    xml = loxone_xml.build_config_xml(controls=[ctrl])
    controls, _visu, _nonvisu = getControlsFromConfigXML(xml)

    assert ctrl.uuid.encode("utf-8") in controls
    assert all(isinstance(k, bytes) for k in controls)
    assert controls[ctrl.uuid.encode("utf-8")]["type"] == "INFOONLYANALOG"


def test_parse_visu_and_nonvisu_bucketing():
    visu = loxone_xml.Control(type="Meter", title="Strom", visu=True)
    nonvisu = loxone_xml.Control(type="InfoOnlyDigital", title="Fenster")
    xml = loxone_xml.build_config_xml(controls=[visu, nonvisu])

    controls, visu_controls, nonvisu_controls = getControlsFromConfigXML(xml)

    assert visu.uuid.encode("utf-8") in visu_controls
    assert nonvisu.uuid.encode("utf-8") in nonvisu_controls
    assert controls[visu.uuid.encode("utf-8")]["visu"] is True
    assert controls[nonvisu.uuid.encode("utf-8")]["visu"] is False


def test_parse_visu_pwd_flag():
    ctrl = loxone_xml.Control(type="Jalousie", title="Rollo", visu=True, visu_pwd=True)
    xml = loxone_xml.build_config_xml(controls=[ctrl])
    controls, _v, _n = getControlsFromConfigXML(xml)
    assert controls[ctrl.uuid.encode("utf-8")]["VisuPwd"] is True


def test_parse_co_subcontrols_get_parent_uuid():
    co = loxone_xml.Co(key="AQ")
    ctrl = loxone_xml.Control(type="Meter", title="Zaehler", cos=[co])
    xml = loxone_xml.build_config_xml(controls=[ctrl])
    controls, _v, _n = getControlsFromConfigXML(xml)

    parent_key = ctrl.uuid.encode("utf-8")
    co_key = co.uuid.encode("utf-8")
    assert parent_key in controls
    assert co_key in controls
    assert controls[co_key]["parent_uuid"] == parent_key
    assert controls[co_key]["fieldkey"] == "AQ"


def test_parse_unit_strips_markup_and_resolves_category_room():
    get_config().control.type_blacklist = {"CATEGORY", "PLACE"}
    cat = loxone_xml.Category(title="Heizung")
    room = loxone_xml.Room(title="Wohnzimmer")
    ctrl = loxone_xml.Control(
        type="InfoOnlyAnalog",
        title="Temp",
        unit="kWh<br>",
        analog=True,
        category=cat.uuid,
        room=room.uuid,
    )
    xml = loxone_xml.build_config_xml(controls=[ctrl], rooms=[room], categories=[cat])
    controls, _v, _n = getControlsFromConfigXML(xml)

    point = controls[ctrl.uuid.encode("utf-8")]["point"]
    assert b"unit=kWh" in point
    assert b"<br>" not in point
    assert b"category=Heizung" in point
    assert b"room=Wohnzimmer" in point


def test_type_blacklist_excludes_categories_and_places():
    get_config().control.type_blacklist = {"CATEGORY", "PLACE"}
    cat = loxone_xml.Category(title="Heizung")
    room = loxone_xml.Room(title="Wohnzimmer")
    ctrl = loxone_xml.Control(type="Meter", title="Strom")
    xml = loxone_xml.build_config_xml(controls=[ctrl], rooms=[room], categories=[cat])
    controls, _v, _n = getControlsFromConfigXML(xml)

    assert ctrl.uuid.encode("utf-8") in controls
    assert cat.uuid.encode("utf-8") not in controls
    assert room.uuid.encode("utf-8") not in controls


# --------------------------------------------------------------------------- #
# REGRESSION: VIRTUALTEXTIN control-type handling                              #
# --------------------------------------------------------------------------- #


def test_virtualtextin_excluded_from_grabber_bucket(caplog):
    """A (non-visu) VIRTUALTEXTIN control is present in the full control set but
    must NOT land in the grabber (non-visu) bucket, and warns while skipping."""
    vt = loxone_xml.Control(type="VirtualTextIn", title="VT")
    xml = loxone_xml.build_config_xml(controls=[vt])

    with caplog.at_level("WARNING"):
        controls, visu_controls, nonvisu_controls = getControlsFromConfigXML(xml)

    key = vt.uuid.encode("utf-8")
    assert key in controls
    assert key not in nonvisu_controls  # excluded from grabber
    assert key not in visu_controls
    assert controls[key]["type"] == "VIRTUALTEXTIN"
    assert "not capable of being used in the grabber" in caplog.text


def test_virtualtextin_visu_still_bucketed_as_visu():
    # When flagged visu, VIRTUALTEXTIN takes the visu branch (before the blacklist).
    vt = loxone_xml.Control(type="VirtualTextIn", title="VT", visu=True)
    xml = loxone_xml.build_config_xml(controls=[vt])
    _controls, visu_controls, _nonvisu = getControlsFromConfigXML(xml)
    assert vt.uuid.encode("utf-8") in visu_controls


# --------------------------------------------------------------------------- #
# linkC promotion: a hidden control referenced by a visu control's ``linkC``   #
# is implicitly visible and must be moved from the grabber (non-visu) bucket   #
# into the visu bucket. The promotion runs ONCE after all controls are         #
# collected, so it must be independent of document order.                      #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "target_first",
    [
        pytest.param(False, id="source_before_target"),
        pytest.param(True, id="target_before_source"),
    ],
)
def test_extract_controls_linkc_promotes_hidden_target(target_first):
    """A hidden control named in a visu control's ``linkC`` is promoted to visu.

    Parametrised over BOTH document orders (linked target before *and* after the
    referring visu control) to lock in that the promotion resolves against the
    fully-collected control set - not whatever happened to exist mid-traversal.
    """
    hidden = loxone_xml.Control(type="InfoOnlyDigital", title="HiddenPump")
    visu = loxone_xml.Control(
        type="Meter", title="Strom", visu=True, link_c=(hidden.uuid,)
    )
    order = [hidden, visu] if target_first else [visu, hidden]
    xml = loxone_xml.build_config_xml(controls=order)
    root = etree.fromstring(xml.encode("utf-8"))

    controls, visu_controls, nonvisu_controls = extractControls(root, {}, _LxmlBackend)

    hkey = hidden.uuid.encode("utf-8")
    vkey = visu.uuid.encode("utf-8")
    assert vkey in visu_controls  # explicitly-visible source stays visu
    assert hkey in visu_controls  # hidden target promoted into visu ...
    assert hkey not in nonvisu_controls  # ... and pulled out of the grabber bucket
    assert visu_controls[hkey] is controls[hkey]  # same object, not a copy


def test_linkc_promotes_multiple_comma_separated_targets():
    # ``linkC`` is a comma-separated list; every hidden target is promoted.
    h1 = loxone_xml.Control(type="InfoOnlyDigital", title="H1")
    h2 = loxone_xml.Control(type="InfoOnlyAnalog", title="H2", analog=True)
    visu = loxone_xml.Control(
        type="Meter", title="Strom", visu=True, link_c=(h1.uuid, h2.uuid)
    )
    xml = loxone_xml.build_config_xml(controls=[visu, h1, h2])

    _controls, visu_controls, nonvisu_controls = getControlsFromConfigXML(xml)

    for target in (h1, h2):
        key = target.uuid.encode("utf-8")
        assert key in visu_controls
        assert key not in nonvisu_controls


def test_linkc_ignored_when_source_is_not_visu():
    # Promotion only fires when the *referring* control is itself visu; a hidden
    # control carrying ``linkC`` must not drag its target into the visu bucket.
    hidden = loxone_xml.Control(type="InfoOnlyDigital", title="Hidden")
    nonvisu_src = loxone_xml.Control(
        type="InfoOnlyAnalog", title="Src", analog=True, link_c=(hidden.uuid,)
    )
    xml = loxone_xml.build_config_xml(controls=[nonvisu_src, hidden])

    _controls, visu_controls, nonvisu_controls = getControlsFromConfigXML(xml)

    hkey = hidden.uuid.encode("utf-8")
    assert hkey in nonvisu_controls
    assert hkey not in visu_controls


def test_linkc_target_already_visu_is_unaffected():
    # If the linked target is already visu it is not in the non-visu bucket, so
    # the promotion is a harmless no-op (target stays visu, nothing crashes).
    target = loxone_xml.Control(type="Meter", title="AlreadyVisu", visu=True)
    src = loxone_xml.Control(
        type="Jalousie", title="Src", visu=True, link_c=(target.uuid,)
    )
    xml = loxone_xml.build_config_xml(controls=[src, target])

    _controls, visu_controls, nonvisu_controls = getControlsFromConfigXML(xml)

    tkey = target.uuid.encode("utf-8")
    assert tkey in visu_controls
    assert tkey not in nonvisu_controls


def test_linkc_dangling_reference_is_noop():
    # A ``linkC`` pointing at a non-existent uuid must not crash nor invent keys,
    # and must leave unrelated controls in their original bucket.
    ghost = loxone_xml.make_uuid()
    visu = loxone_xml.Control(type="Meter", title="Strom", visu=True, link_c=(ghost,))
    hidden = loxone_xml.Control(type="InfoOnlyDigital", title="Hidden")
    xml = loxone_xml.build_config_xml(controls=[visu, hidden])

    controls, visu_controls, nonvisu_controls = getControlsFromConfigXML(xml)

    gkey = ghost.encode("utf-8")
    assert gkey not in controls
    assert gkey not in visu_controls
    assert hidden.uuid.encode("utf-8") in nonvisu_controls


# --------------------------------------------------------------------------- #
# REGRESSION: malformed / duplicate-attribute XML recovery                     #
# --------------------------------------------------------------------------- #


@pytest.mark.parametrize(
    "force_lxml",
    [
        pytest.param(False, id="active_parser"),
        pytest.param(True, id="lxml_fallback"),
    ],
)
def test_duplicate_attribute_loxair_first_value_wins(force_lxml, monkeypatch):
    """A ``<C>`` tag with a duplicate attribute must NOT crash the pipeline, and
    the parser must keep the *first* occurrence of the duplicated attribute.

    We used to pre-clean these via ``correctXML_removeAttributeDuplicates`` (a
    no-op that never actually fired). Duplicates are now handled by the parsers
    themselves, so this asserts BOTH backends cope identically:

      * ``force_lxml=False``: the active parser (pygixml fast path when present,
        which tolerates duplicate attributes natively),
      * ``force_lxml=True``:  the forced lxml ``recover=True`` fallback.

    We duplicate ``Title`` (``Title="first" ... Title="second"``) because it is
    an attribute production *reads* - it becomes the point measurement and the
    ``name`` tag. So we can assert concretely that the first value ("first")
    survives into the payload and the second ("second") is discarded, rather than
    merely checking the control exists.
    """
    if force_lxml:
        _force_lxml_fallback(monkeypatch)

    dup = loxone_xml.loxair_duplicate_attr_control(values=("first", "second"))
    xml = loxone_xml.build_config_xml(controls=[dup])

    # The call itself must not raise - that is the "do we hit the wall" check.
    controls, _v, _n = getControlsFromConfigXML(xml)
    key = dup.uuid.encode("utf-8")
    assert key in controls

    # First value wins: Title -> measurement + ``name`` tag show "first", and the
    # discarded "second" occurrence appears nowhere in the emitted line protocol.
    line = controls[key]["pointInflux"].to_line_protocol()
    assert line.startswith("first,"), line  # measurement is the first Title
    assert "name=first" in line
    assert "second" not in line


def test_structurally_malformed_xml_recovers():
    # An unclosed IoData attribute list -> strict parsers reject, lxml recovers.
    bad = (
        '<?xml version="1.0"?>\n'
        '<ControlList Version="1" LxAV="1">\n'
        '<C Type="InfoOnlyAnalog" U="u1" Title="T" Analog="true"><IoData Cr="x"\n'
        "</ControlList>\n"
    )
    # Should recover rather than raise.
    controls, _v, _n = getControlsFromConfigXML(bad)
    assert b"u1" in controls


def test_extract_controls_direct_via_lxml_backend():
    # Exercise extractControls directly against an lxml root for a focused check.
    ctrl = loxone_xml.Control(type="Meter", title="M", visu=True)
    xml = loxone_xml.build_config_xml(controls=[ctrl])
    root = etree.fromstring(xml.encode("utf-8"))
    controls, visu_controls, nonvisu_controls = extractControls(root, {}, _LxmlBackend)

    key = ctrl.uuid.encode("utf-8")
    assert key in controls
    assert key in visu_controls
    assert key not in nonvisu_controls


def test_getcontrols_is_cached_for_identical_input():
    ctrl = loxone_xml.Control(type="Meter", title="Cached")
    xml = loxone_xml.build_config_xml(controls=[ctrl])
    first = getControlsFromConfigXML(xml)
    second = getControlsFromConfigXML(xml)
    # lru_cache -> identical object returned for identical input.
    assert first is second


# --------------------------------------------------------------------------- #
# REGRESSION: parser parity (pygixml fast path vs forced lxml fallback)        #
# --------------------------------------------------------------------------- #


@requires_pygixml
def test_parser_parity_pygixml_vs_forced_lxml_for_rich_config(monkeypatch):
    """A rich config must parse IDENTICALLY on both XML backends.

    ``getControlsFromConfigXML`` has two parse paths: the pygixml fast path (used
    when the native lib loads on this CPU) and the lxml ``recover=True`` fallback
    (forced here via ``_force_lxml_fallback``). They must produce byte-for-byte
    identical controls/visu/nonvisu dicts, otherwise the active parser silently
    changes what gets ingested. This guards against that divergence for a config
    exercising multiple controls, rooms, categories and ``<Co>`` subcontrols.
    """
    get_config().control.type_blacklist = {"CATEGORY", "PLACE"}
    category = loxone_xml.Category(title="Heizung")
    room = loxone_xml.Room(title="Wohnzimmer")
    controls = [
        loxone_xml.Control(
            type="InfoOnlyAnalog",
            title="Temperatur",
            desc="Raumtemperatur",
            unit="\u00b0C<br>",
            analog=True,
            stats_type=2,
            category=category.uuid,
            room=room.uuid,
        ),
        loxone_xml.Control(
            type="InfoOnlyDigital", title="Fensterkontakt", room=room.uuid
        ),
        loxone_xml.Control(
            type="Meter",
            title="Stromzaehler",
            analog=True,
            visu=True,
            category=category.uuid,
            cos=[loxone_xml.Co(key="AQ"), loxone_xml.Co(key="AP")],
        ),
        loxone_xml.Control(
            type="Jalousie", title="Rollo", visu=True, visu_pwd=True, room=room.uuid
        ),
    ]
    xml = loxone_xml.build_config_xml(
        controls=controls, rooms=[room], categories=[category]
    )

    # Active parser (pygixml fast path when the native lib loaded, else lxml).
    _miniserver_mod.getControlsFromConfigXML.cache_clear()
    fast = getControlsFromConfigXML(xml)

    # Force the lxml recover=True fallback and re-parse the SAME XML. The cache
    # must be cleared first, else the fast-path result would be returned verbatim.
    _force_lxml_fallback(monkeypatch)
    _miniserver_mod.getControlsFromConfigXML.cache_clear()
    slow = getControlsFromConfigXML(xml)

    all_fast, visu_fast, nonvisu_fast = fast
    # The config is genuinely rich -> every bucket is populated.
    assert all_fast and visu_fast and nonvisu_fast
    # Identical bucket membership ...
    assert [set(b) for b in fast] == [set(b) for b in slow]
    # ... and identical control payloads (influxdb ``Point`` has value equality).
    assert fast == slow


@requires_pygixml
def test_parser_parity_pygixml_vs_forced_lxml_with_linkc_promotion(monkeypatch):
    """``linkC`` promotion must resolve IDENTICALLY on both XML backends.

    A hidden (non-visu) control referenced by a visu control's ``linkC`` is
    promoted into the visu bucket. That decision hinges on ``Visu``/``linkC``
    attribute reads, which travel different code paths on the pygixml fast path
    vs the lxml ``recover=True`` fallback. We assert both backends (a) actually
    perform the promotion and (b) agree on final bucket membership + payloads.
    """
    hidden = loxone_xml.Control(type="InfoOnlyDigital", title="HiddenPump")
    visu = loxone_xml.Control(
        type="Meter",
        title="Strom",
        visu=True,
        link_c=(hidden.uuid,),
        cos=[loxone_xml.Co(key="AQ")],
    )
    # Target listed BEFORE its referring visu control to also exercise ordering.
    xml = loxone_xml.build_config_xml(controls=[hidden, visu])

    _miniserver_mod.getControlsFromConfigXML.cache_clear()
    fast = getControlsFromConfigXML(xml)

    _force_lxml_fallback(monkeypatch)
    _miniserver_mod.getControlsFromConfigXML.cache_clear()
    slow = getControlsFromConfigXML(xml)

    hkey = hidden.uuid.encode("utf-8")
    # Promotion actually happened on the active/fast path ...
    assert hkey in fast[1] and hkey not in fast[2]
    # ... identically on the forced-lxml path ...
    assert hkey in slow[1] and hkey not in slow[2]
    # ... and full bucket membership + payloads match byte-for-byte.
    assert [set(b) for b in fast] == [set(b) for b in slow]
    assert fast == slow
