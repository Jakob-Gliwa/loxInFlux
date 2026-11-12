"""Unit tests for ``loxInFlux.config``.

Covers ``FilterConfig`` (``__post_init__`` normalisation, ``should_include_control``,
``filter_controls``), ``TelegrafConfig.from_dict``, ``AppConfig.load`` and the legacy
``get()`` accessor.

The module is reached via ``sys.modules`` so tests can monkeypatch its
module-level ``get_args`` and read ``get_config()``/``get()`` directly.
"""

from __future__ import annotations

import dataclasses
import sys
from types import SimpleNamespace

import pytest

from loxInFlux.config import (
    AppConfig,
    FilterConfig,
    MQTTConfig,
    TelegrafConfig,
)

# Reach the module object so tests can monkeypatch its module-level ``get_args``
# and call ``get_config()``/``get()``.
config_mod = sys.modules["loxInFlux.config"]

# --------------------------------------------------------------------------- #
# FilterConfig.__post_init__                                                   #
# --------------------------------------------------------------------------- #


def test_filterconfig_normalises_types_to_uppercase_sets():
    fc = FilterConfig(type_blacklist=["foo", "Bar"], type_whitelist=["baz"])
    assert fc.type_blacklist == {"FOO", "BAR"}
    assert fc.type_whitelist == {"BAZ"}
    assert isinstance(fc.type_blacklist, set)
    assert isinstance(fc.type_whitelist, set)


def test_filterconfig_uuids_normalised_to_byte_key_sets():
    # UUIDs are normalised to ``bytes`` (never case-folded) so str-configured
    # filters match the bytes-keyed controls from ``getControlsFromConfigXML``.
    fc = FilterConfig(uuid_blacklist=["Abc", "def"], uuid_whitelist=["XYZ"])
    assert fc.uuid_blacklist == {b"Abc", b"def"}
    assert fc.uuid_whitelist == {b"XYZ"}


def test_filterconfig_uuids_accept_mixed_str_and_bytes():
    # A blacklist mixing str and bytes collapses to a single byte-key set.
    fc = FilterConfig(uuid_blacklist=["from-str", b"from-bytes"])
    assert fc.uuid_blacklist == {b"from-str", b"from-bytes"}


def test_filterconfig_is_frozen():
    fc = FilterConfig()
    with pytest.raises(dataclasses.FrozenInstanceError):
        fc.type_blacklist = {"X"}  # type: ignore[misc]


# --------------------------------------------------------------------------- #
# FilterConfig.should_include_control                                          #
# --------------------------------------------------------------------------- #


def test_should_include_empty_filters_includes_everything():
    fc = FilterConfig()
    assert fc.should_include_control("Meter", "uuid-1") is True


def test_should_include_type_blacklist_excludes_case_insensitively():
    fc = FilterConfig(type_blacklist=["meter"])
    assert fc.should_include_control("METER", "uuid-1") is False
    assert fc.should_include_control("Jalousie", "uuid-1") is True


def test_should_include_type_whitelist_only_admits_listed_types():
    fc = FilterConfig(type_whitelist=["Meter"])
    assert fc.should_include_control("meter", "uuid-1") is True
    # Anything not in the whitelist is rejected, even if not blacklisted.
    assert fc.should_include_control("Jalousie", "uuid-1") is False


def test_should_include_whitelist_takes_precedence_over_blacklist():
    # When a whitelist is present the blacklist branch is skipped entirely.
    fc = FilterConfig(type_whitelist=["Meter"], type_blacklist=["Meter"])
    assert fc.should_include_control("Meter", "uuid-1") is True


def test_should_include_uuid_blacklist():
    fc = FilterConfig(uuid_blacklist=["bad-uuid"])
    assert fc.should_include_control("Meter", "bad-uuid") is False
    assert fc.should_include_control("Meter", "good-uuid") is True


def test_should_include_uuid_whitelist():
    fc = FilterConfig(uuid_whitelist=["good-uuid"])
    assert fc.should_include_control("Meter", "good-uuid") is True
    assert fc.should_include_control("Meter", "other-uuid") is False


# --------------------------------------------------------------------------- #
# FilterConfig.filter_controls                                                 #
# --------------------------------------------------------------------------- #


def test_filter_controls_keeps_passing_parents_and_their_subcontrols():
    controls = {
        "p1": {"type": "Meter"},
        "p2": {"type": "Jalousie"},
        "s1": {"type": "Meter", "parent_uuid": "p1"},
        "s2": {"type": "Meter", "parent_uuid": "p2"},
        "s3": {"type": "Meter", "parent_uuid": "missing"},
    }
    fc = FilterConfig(type_blacklist=["Jalousie"])
    result = fc.filter_controls(controls)

    assert "p1" in result  # parent passes
    assert "p2" not in result  # blacklisted type
    assert "s1" in result  # parent p1 included
    assert "s2" not in result  # parent p2 excluded
    assert "s3" not in result  # parent missing entirely


def test_filter_controls_respects_uuid_blacklist_on_parent():
    controls = {
        "p1": {"type": "Meter"},
        "s1": {"type": "Meter", "parent_uuid": "p1"},
    }
    fc = FilterConfig(uuid_blacklist=["p1"])
    result = fc.filter_controls(controls)
    # Parent excluded by uuid -> subcontrol dropped too.
    assert result == {}


def test_filter_controls_empty_filter_returns_all():
    controls = {
        "p1": {"type": "Meter"},
        "s1": {"type": "Meter", "parent_uuid": "p1"},
    }
    fc = FilterConfig()
    assert fc.filter_controls(controls) == controls


def test_should_include_uuid_blacklist_matches_bytes_uuid():
    # Regression: str-configured blacklist must reject the bytes UUID that
    # ``getControlsFromConfigXML`` produces (previously slipped through).
    fc = FilterConfig(uuid_blacklist=["bad"])
    assert fc.should_include_control("Meter", b"bad") is False
    assert fc.should_include_control("Meter", b"good") is True


def test_filter_controls_bytes_keyed_with_mixed_black_and_whitelist():
    # Controls keyed by UUID bytes (incl. bytes ``parent_uuid``), matching
    # ``getControlsFromConfigXML``. The blacklist mixes str and bytes entries.
    controls = {
        b"keep": {"type": "Meter"},
        b"drop": {"type": "Meter"},
        b"sub-keep": {"type": "Meter", "parent_uuid": b"keep"},
        b"sub-drop": {"type": "Meter", "parent_uuid": b"drop"},
    }
    fc = FilterConfig(uuid_blacklist=["drop", b"another-bad"])
    result = fc.filter_controls(controls)

    assert set(result) == {b"keep", b"sub-keep"}
    # Parent excluded by (str-configured) blacklist -> its subcontrol drops too.
    assert b"drop" not in result
    assert b"sub-drop" not in result


def test_filter_controls_bytes_keyed_uuid_whitelist():
    # A str-configured whitelist must admit the matching bytes-keyed controls
    # (each control is checked against the whitelist by its own UUID) while
    # rejecting everything else.
    controls = {
        b"allowed": {"type": "Meter"},
        b"other": {"type": "Meter"},
        b"sub-allowed": {"type": "Meter", "parent_uuid": b"allowed"},
    }
    fc = FilterConfig(uuid_whitelist=["allowed", b"sub-allowed"])
    result = fc.filter_controls(controls)

    assert set(result) == {b"allowed", b"sub-allowed"}
    assert b"other" not in result


# --------------------------------------------------------------------------- #
# TelegrafConfig.from_dict                                                     #
# --------------------------------------------------------------------------- #


def test_telegraf_from_dict_defaults():
    tc = TelegrafConfig.from_dict({})
    assert tc.host == "0.0.0.0"
    assert tc.port == 8086
    assert tc.protocol == "udp"
    assert tc.max_retries == 100
    assert isinstance(tc.mqtt, MQTTConfig)
    assert tc.mqtt.host == "localhost"


def test_telegraf_from_dict_reads_scalar_fields():
    tc = TelegrafConfig.from_dict(
        {"host": "1.2.3.4", "port": 9000, "protocol": "tcp", "max_retries": 5}
    )
    assert tc.host == "1.2.3.4"
    assert tc.port == 9000
    assert tc.protocol == "tcp"
    assert tc.max_retries == 5


def test_telegraf_from_dict_ignores_mqtt_block_for_non_mqtt_protocol():
    tc = TelegrafConfig.from_dict(
        {"protocol": "tcp", "mqtt": {"host": "should-be-ignored"}}
    )
    # Non-mqtt protocols get default MQTT config, not the supplied block.
    assert tc.mqtt.host == "localhost"


def test_telegraf_from_dict_builds_mqtt_config_for_mqtt_protocol():
    tc = TelegrafConfig.from_dict(
        {"protocol": "mqtt", "mqtt": {"host": "broker", "port": 1884}}
    )
    assert tc.protocol == "mqtt"
    assert tc.mqtt.host == "broker"
    assert tc.mqtt.port == 1884


# --------------------------------------------------------------------------- #
# AppConfig.load                                                               #
# --------------------------------------------------------------------------- #


def test_appconfig_load_reads_bootstrap_config(
    bootstrap_config_dir, bootstrap_data_dir
):
    # Both dirs are passed explicitly so load() never falls back to get_args()
    # (which would parse pytest's own argv).
    cfg = AppConfig.load(
        config_dir=str(bootstrap_config_dir), data_dir=str(bootstrap_data_dir)
    )
    assert isinstance(cfg, AppConfig)
    assert cfg.miniserver.host == "127.0.0.1"
    assert cfg.miniserver.port == 80
    assert cfg.miniserver.user == "admin"
    assert cfg.telegraf.port == 8094
    assert cfg.telegraf.protocol == "udp"
    assert cfg.general.grabber_interval == 300


def test_appconfig_load_raises_when_config_missing(monkeypatch, tmp_path):
    empty = tmp_path / "empty-config"
    monkeypatch.setattr(
        config_mod,
        "get_args",
        lambda: SimpleNamespace(config_dir=str(empty), data_dir=None),
    )
    with pytest.raises(FileNotFoundError):
        AppConfig.load()


def test_appconfig_load_explicit_dirs_take_precedence_over_args(monkeypatch, tmp_path):
    # Regression (Bug 2): explicitly passed config_dir/data_dir must be honoured
    # and get_args() must NOT be consulted (previously the args overrode them).
    cfg_dir = tmp_path / "explicit-cfg"
    cfg_dir.mkdir()
    (cfg_dir / "config.toml").write_text(
        '[miniserver]\nhost = "9.9.9.9"\n', encoding="utf-8"
    )
    data_dir = tmp_path / "explicit-data"

    def _no_args():
        raise AssertionError("get_args() must not be called when dirs are explicit")

    monkeypatch.setattr(config_mod, "get_args", _no_args)

    cfg = AppConfig.load(config_dir=str(cfg_dir), data_dir=str(data_dir))

    assert cfg.miniserver.host == "9.9.9.9"
    assert cfg.paths.data_dir == str(data_dir)


def test_appconfig_load_data_dir_override(monkeypatch, tmp_path):
    cfg_dir = tmp_path / "cfg"
    cfg_dir.mkdir()
    (cfg_dir / "config.toml").write_text(
        '[miniserver]\nhost = "1.2.3.4"\n', encoding="utf-8"
    )
    data_dir = tmp_path / "custom-data"

    monkeypatch.setattr(
        config_mod,
        "get_args",
        lambda: SimpleNamespace(config_dir=str(cfg_dir), data_dir=str(data_dir)),
    )
    cfg = AppConfig.load()
    assert cfg.miniserver.host == "1.2.3.4"
    assert cfg.paths.data_dir == str(data_dir)


def test_appconfig_load_builds_filter_configs_for_websocket_and_grabber(
    monkeypatch, tmp_path
):
    # load() must read config.toml and build a working FilterConfig for each of
    # the websocket and grabber buckets from the ``[filters.*]`` sections.
    cfg_dir = tmp_path / "cfg"
    cfg_dir.mkdir()
    (cfg_dir / "config.toml").write_text(
        "[miniserver]\n"
        'host = "1.2.3.4"\n'
        "\n"
        "[filters]\n"
        'type_blacklist = ["Category"]\n'
        "\n"
        "[filters.websocket]\n"
        'type_blacklist = ["Jalousie"]\n'
        'uuid_blacklist = ["ws-bad"]\n'
        "\n"
        "[filters.grabber]\n"
        'type_whitelist = ["Meter"]\n'
        'uuid_whitelist = ["grab-good"]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(
        config_mod,
        "get_args",
        lambda: SimpleNamespace(config_dir=str(cfg_dir), data_dir=None),
    )
    cfg = AppConfig.load()

    assert isinstance(cfg.control.websocket, FilterConfig)
    assert isinstance(cfg.control.grabber, FilterConfig)
    # The top-level type_blacklist is normalised to an uppercase set.
    assert cfg.control.type_blacklist == {"CATEGORY"}

    # websocket bucket: type + uuid rules apply against bytes-keyed controls.
    ws = cfg.control.websocket
    assert ws.should_include_control("Jalousie", b"anything") is False
    assert ws.should_include_control("Meter", b"ws-bad") is False
    assert ws.should_include_control("Meter", b"ws-ok") is True

    # grabber bucket: whitelist only admits the listed type + UUID.
    grab = cfg.control.grabber
    assert grab.should_include_control("Meter", b"grab-good") is True
    assert grab.should_include_control("Meter", b"grab-other") is False
    assert grab.should_include_control("Jalousie", b"grab-good") is False


# --------------------------------------------------------------------------- #
# get() legacy accessor                                                        #
# --------------------------------------------------------------------------- #


def test_get_returns_live_config_value():
    assert (
        config_mod.get("miniserver", "host") == config_mod.get_config().miniserver.host
    )


def test_get_unknown_section_returns_default():
    assert config_mod.get("nope", "whatever", "fallback") == "fallback"


def test_get_unknown_key_returns_default():
    assert config_mod.get("miniserver", "does_not_exist", "fallback") == "fallback"
