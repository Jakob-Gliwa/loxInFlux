"""Programmatic builder for Loxone Config XML (``ControlList``).

Produces XML in the exact shape ``loxInFlux.miniserver.getControlsFromConfigXML``
consumes (verified against the real ``data/sps_*.xml`` archives):

    <ControlList Version="252" LxAV="76" ...>
        <C Type="Category" U="..." Title="..."/>
        <C Type="Place"    U="..." Title="..."/>
        <C Type="InfoOnlyAnalog" U="..." Title="..." Desc="..."
           StatsType="2" Analog="true" linkC="...">
            <IoData Cr="<cat-uuid>" Pr="<room-uuid>" Visu="true" VisuPwd="true"/>
            <Display Unit="&deg;C"/>
            <Co K="AQ" U="..."/>
        </C>
    </ControlList>

The parser reads controls via ``.//C[@Type]`` and rooms/categories via
``.//C[@Type="Category" or @Type="Place"]``; ``IoData/@Cr`` maps to a *category*
and ``IoData/@Pr`` maps to a *place/room*.

Because lxml refuses to *emit* duplicate attributes, everything is rendered by
hand from strings, which also lets us produce malformed variants (duplicate
attributes, BOM, missing declaration) for the recovery/regression tests.
"""

from __future__ import annotations

import uuid as _uuidlib
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from xml.sax.saxutils import quoteattr

__all__ = [
    "Category",
    "Co",
    "Control",
    "Room",
    "build_config_xml",
    "make_uuid",
    "with_bom",
    "loxair_duplicate_attr_control",
]

# A Loxone UUID looks like ``17a05a15-0243-d451-ffff68510c67e724`` (8-4-4-16 hex).
_BOM = "\ufeff"


def make_uuid() -> str:
    """Return a fresh, unique Loxone-style UUID string (8-4-4-16 hex)."""
    h = _uuidlib.uuid4().hex + _uuidlib.uuid4().hex
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:32]}"


@dataclass
class Co:
    """A ``<Co>`` sub-object (state) of a control.

    ``key`` becomes the InfluxDB field key; each Co yields its own entry in the
    parsed ``controls`` dict, keyed by ``uuid`` (as bytes) with ``parent_uuid``
    pointing back at the owning control.
    """

    key: str
    uuid: str = field(default_factory=make_uuid)


@dataclass
class Category:
    """A ``<C Type="Category">`` element (referenced via ``IoData/@Cr``)."""

    title: str = "Kategorie"
    uuid: str = field(default_factory=make_uuid)


@dataclass
class Room:
    """A ``<C Type="Place">`` element (referenced via ``IoData/@Pr``)."""

    title: str = "Raum"
    uuid: str = field(default_factory=make_uuid)


@dataclass
class Control:
    """A single ``<C Type=...>`` control.

    Args:
        type: Loxone control ``Type`` (e.g. ``"InfoOnlyAnalog"``). Compared
            case-insensitively against ``config.control.type_blacklist``.
        uuid: The ``U`` attribute; auto-generated when omitted. NOTE: the parser
            stores control keys as ``uuid.encode("utf-8")`` (i.e. *bytes*).
        title: ``Title`` (measurement name / ``name`` tag).
        desc: ``Desc`` (``description`` tag).
        stats_type: ``StatsType`` integer.
        analog: emits ``Analog="true"`` when True (any other value is analog=0).
        unit: emits a ``<Display Unit="...">`` child; ``<...>`` markup is
            stripped by the parser exactly as in production.
        visu: emits ``IoData/@Visu="true"`` -> control lands in the *websocket*
            (visu) bucket.
        visu_pwd: emits ``IoData/@VisuPwd="true"`` (control["VisuPwd"] == True).
        category: a Category UUID (``IoData/@Cr``).
        room: a Room/Place UUID (``IoData/@Pr``).
        link_c: values for the ``linkC`` attribute (only honoured when visu).
        cos: ``<Co>`` sub-objects.
        extra_attrs: additional literal attributes on the ``<C>`` opening tag.
        raw_attrs: verbatim string spliced into the opening tag *after* the
            normal attributes - use this to inject duplicate/malformed attrs.
        emit_iodata: force-emit (or suppress) the ``<IoData>`` child. When None
            (default) it is emitted iff any of category/room/visu/visu_pwd is set.
    """

    type: str
    uuid: str = field(default_factory=make_uuid)
    title: str = "Control"
    desc: str = ""
    stats_type: int = 0
    analog: bool = False
    unit: str | None = None
    visu: bool = False
    visu_pwd: bool = False
    category: str | None = None
    room: str | None = None
    link_c: Sequence[str] = field(default_factory=tuple)
    cos: Sequence[Co] = field(default_factory=tuple)
    extra_attrs: Mapping[str, object] = field(default_factory=dict)
    raw_attrs: str = ""
    emit_iodata: bool | None = None


def _attr(name: str, value: object) -> str:
    """Render a single ``name="value"`` pair (properly escaped) with a leading space."""
    return f" {name}={quoteattr(str(value))}"


def _render_iodata(control: Control) -> str:
    want = control.emit_iodata
    if want is None:
        want = any((control.category, control.room, control.visu, control.visu_pwd))
    if not want:
        return ""
    parts = ["<IoData"]
    if control.category is not None:
        parts.append(_attr("Cr", control.category))
    if control.room is not None:
        parts.append(_attr("Pr", control.room))
    if control.visu:
        parts.append(_attr("Visu", "true"))
    if control.visu_pwd:
        parts.append(_attr("VisuPwd", "true"))
    parts.append("/>")
    return "".join(parts)


def _render_control(control: Control, indent: str = "\t") -> str:
    attrs = [
        _attr("Type", control.type),
        _attr("U", control.uuid),
        _attr("Title", control.title),
        _attr("Desc", control.desc),
        _attr("StatsType", control.stats_type),
        _attr("Analog", "true" if control.analog else "false"),
    ]
    if control.link_c:
        attrs.append(_attr("linkC", ",".join(control.link_c)))
    for name, value in control.extra_attrs.items():
        attrs.append(_attr(name, value))
    opening = f"{indent}<C{''.join(attrs)}"
    if control.raw_attrs:
        opening += " " + control.raw_attrs

    children: list[str] = []
    iodata = _render_iodata(control)
    if iodata:
        children.append(f"{indent}\t{iodata}")
    if control.unit is not None:
        children.append(f"{indent}\t<Display{_attr('Unit', control.unit)}/>")
    for co in control.cos:
        children.append(f"{indent}\t<Co{_attr('K', co.key)}{_attr('U', co.uuid)}/>")

    if not children:
        return opening + "/>"
    body = "\n".join(children)
    return f"{opening}>\n{body}\n{indent}</C>"


def _render_place_or_category(kind: str, uuid: str, title: str, indent: str) -> str:
    return f"{indent}<C{_attr('Type', kind)}{_attr('U', uuid)}{_attr('Title', title)}/>"


def build_config_xml(
    controls: Iterable[Control] = (),
    rooms: Iterable[Room] = (),
    categories: Iterable[Category] = (),
    *,
    version: str = "252",
    lx_av: str = "76",
    declaration: bool = True,
    encoding: str = "utf-8",
    bom: bool = False,
    extra_root_attrs: Mapping[str, object] | None = None,
) -> str:
    """Assemble a complete Loxone ``ControlList`` XML document as a ``str``.

    Args:
        controls: the ``<C>`` controls to emit.
        rooms: ``<C Type="Place">`` elements.
        categories: ``<C Type="Category">`` elements.
        version / lx_av: root ``Version`` / ``LxAV`` attributes.
        declaration: prepend the ``<?xml ...?>`` declaration.
        encoding: value used in the XML declaration.
        bom: prepend a UTF-8 BOM (``\\ufeff``) to exercise ``remove_bom``.
        extra_root_attrs: extra attributes on the ``<ControlList>`` root.

    Returns:
        The XML document as text (encode to bytes yourself when needed).
    """
    root_attrs = [_attr("Version", version), _attr("LxAV", lx_av)]
    for name, value in (extra_root_attrs or {}).items():
        root_attrs.append(_attr(name, value))

    lines: list[str] = []
    if declaration:
        lines.append(f'<?xml version="1.0" encoding="{encoding}"?>')
    lines.append(f"<ControlList{''.join(root_attrs)}>")
    for category in categories:
        lines.append(
            _render_place_or_category("Category", category.uuid, category.title, "\t")
        )
    for room in rooms:
        lines.append(_render_place_or_category("Place", room.uuid, room.title, "\t"))
    for control in controls:
        lines.append(_render_control(control, "\t"))
    lines.append("</ControlList>")

    xml = "\n".join(lines) + "\n"
    if bom:
        xml = _BOM + xml
    return xml


def with_bom(xml: str) -> str:
    """Return ``xml`` prefixed with a UTF-8 BOM (idempotent)."""
    return xml if xml.startswith(_BOM) else _BOM + xml


def loxair_duplicate_attr_control(
    *,
    uuid: str | None = None,
    values: tuple[str, str] = ("first", "second"),
) -> Control:
    """A ``Type="LoxAIR"`` control carrying a *duplicate* ``Title`` attribute.

    Loxone historically emits duplicate attributes on LoxAIR/LoxAIRDevice/User
    tags. This exercises the parser's tolerance for that AND its
    first-occurrence-wins semantics: pygixml accepts duplicates natively and the
    lxml fallback relies on ``recover=True`` - both keep the *first* value.

    ``Title`` is duplicated on purpose because production actually reads it (it
    becomes the point measurement and the ``name`` tag), so the parsed control
    payload observably reflects *which* duplicate won. ``values[0]`` is emitted
    as the genuine (first) ``Title`` and ``values[1]`` is spliced in as a second
    ``Title`` occurrence, so a correct parser must surface ``values[0]``.
    """
    first, second = values
    return Control(
        type="LoxAIR",
        uuid=uuid or make_uuid(),
        title=first,  # genuine first Title="first"
        raw_attrs=f"Title={quoteattr(second)}",  # duplicate Title="second"
    )
