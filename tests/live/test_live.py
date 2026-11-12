"""Live smoke test against a real Miniserver.

Marker-gated (``@pytest.mark.live``) and credential-gated: the ``live_credentials``
fixture SKIPS the test cleanly unless ``LOXINFLUX_URL`` / ``LOXINFLUX_USERNAME`` /
``LOXINFLUX_PASSWORD`` are all set, so CI (which runs ``-m "not live"`` and has no
creds) never collects-and-fails here - it just skips.

Smoke: point ``config.miniserver`` at the live URL and pull the config +
``LoxAPP3.json`` via ``miniserver.load_miniserver_config`` over HTTP.
"""

from __future__ import annotations

import tomllib
from pathlib import Path
from urllib.parse import urlsplit

import pytest

from loxInFlux.config import get_config
from loxInFlux.miniserver import getControlsFromConfigXML, load_miniserver_config

# ``tests/live/test_live.py`` -> ``tests`` -> ``tests/..`` == project root.
_PROJECT_ROOT = Path(__file__).resolve().parents[2]
_DEFAULT_CONFIG = _PROJECT_ROOT / "config" / "default_config.toml"

# Structural (non-)controls that also match ``.//C[@Type]`` but are NOT usable
# Loxone controls. Production filters them out via the shipped type_blacklist; if
# that blacklist were ever unavailable we still must not let these count as
# "usable controls" in the smoke assertions below.
_STRUCTURAL_TYPES = {"CATEGORY", "PLACE"}


def _apply_live_target(url: str) -> None:
    """Parse ``LOXINFLUX_URL`` into ``config.miniserver.host``/``port``."""
    if "://" not in url:
        url = "http://" + url
    parts = urlsplit(url)
    cfg = get_config()
    cfg.miniserver.host = parts.hostname or url
    if parts.port is not None:
        cfg.miniserver.port = parts.port
    else:
        cfg.miniserver.port = 443 if parts.scheme == "https" else 80


def _apply_production_filters() -> None:
    """Load the SHIPPED production ``type_blacklist`` onto the config singleton.

    The test bootstrap (conftest) runs with an EMPTY blacklist so builder XML
    survives parsing. But ``.//C[@Type]`` also matches ``Category``/``Place``
    elements, so with an empty blacklist the live smoke could "pass" on nothing
    but structural elements. Loading the real ``default_config.toml`` blacklist
    makes the live parse behave exactly like production. Falls back to at least
    excluding the structural types if the shipped config is unavailable.
    """
    try:
        with open(_DEFAULT_CONFIG, "rb") as f:
            filters = tomllib.load(f).get("filters", {})
        blacklist = set(map(str.upper, filters.get("type_blacklist", [])))
    except OSError:
        blacklist = set()
    get_config().control.type_blacklist = blacklist | _STRUCTURAL_TYPES


@pytest.mark.live
@pytest.mark.asyncio
async def test_load_miniserver_config_live(live_credentials):
    """Fetch the real config + LoxAPP3.json from a live Miniserver."""
    _apply_live_target(live_credentials.url)
    # Parse under production filters so structural Category/Place elements are
    # excluded exactly as they would be in a real deployment.
    _apply_production_filters()

    config_content, loxapp3_json = await load_miniserver_config(
        live_credentials.username,
        live_credentials.password,
        persist=False,
        use_cache=False,
    )

    assert isinstance(config_content, str)
    assert config_content.strip(), "expected non-empty config XML"
    assert isinstance(loxapp3_json, dict)
    assert loxapp3_json, "expected a non-empty LoxAPP3.json payload"

    # Parse the live config through the real extraction pipeline and confirm it
    # yields usable controls (not just a downloadable-but-unparseable blob).
    controls, visu_controls, nonvisu_controls = getControlsFromConfigXML(config_content)
    assert controls, "expected non-empty parsed controls from the live config"
    assert visu_controls or nonvisu_controls, (
        "expected at least one visu/nonvisu control bucket to be populated"
    )

    # Guard against a false-positive smoke: ``.//C[@Type]`` also matches
    # Category/Place elements. Require at least one REAL control type and assert
    # no structural element leaked past the production filter.
    parsed_types = {c["type"] for c in controls.values()}
    assert parsed_types - _STRUCTURAL_TYPES, (
        "only structural (Category/Place) elements were parsed - no usable "
        f"Loxone control extracted: {sorted(parsed_types)}"
    )
    leaked = parsed_types & _STRUCTURAL_TYPES
    assert not leaked, (
        f"structural elements leaked past the production filter: {sorted(leaked)}"
    )
