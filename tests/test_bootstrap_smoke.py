"""Smoke test proving the shared test infrastructure works.

Not a feature test - it only verifies that:
  1. the explicit-init bootstrap in ``conftest.py`` loads the fixture config,
  2. the support helpers round-trip (LoxCC zip + Loxone XML).
"""

from __future__ import annotations

import io
import subprocess
import sys
import zipfile

from support import loxcc, loxone_xml

import loxInFlux


def test_loxinflux_imports_via_bootstrap():
    """The fixture config is initialised, so the singleton is populated."""
    assert loxInFlux.__version__
    assert loxInFlux.get_config().miniserver.host == "127.0.0.1"


def test_bare_import_without_config_has_no_side_effects(tmp_path):
    """Regression (Bug 1): ``import loxInFlux`` must succeed with no config.toml.

    Runs in a fresh interpreter from an empty working directory (no ``config/``
    dir, no ``config.toml``). Importing must not load the config, parse argv, or
    create directories - it used to raise ``FileNotFoundError`` because the
    config was loaded eagerly at import time.

    Bogus argv is injected on purpose: if the import parsed the command line,
    argparse would reject the unknown flag and abort with ``SystemExit``.
    """
    probe = (
        "import sys; sys.argv = ['x', '--totally-unknown-flag']\n"
        "import loxInFlux\n"
        "import loxInFlux.config as c, loxInFlux.telegraf as t\n"
        "assert c._config is None, 'config was loaded at import'\n"
        "assert t._writer is None, 'telegraf writer was built at import'\n"
        "print(loxInFlux.__version__)\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", probe],
        cwd=tmp_path,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    assert "FileNotFoundError" not in result.stderr
    # Import must not have materialised a config/ or data/ directory in the cwd.
    assert not (tmp_path / "config").exists()
    assert not (tmp_path / "data").exists()
    # The XML-backend probe spawns a subprocess and logs; it must stay deferred
    # out of import time, so a bare import emits nothing at all.
    assert "pygixml load probe" not in result.stderr
    assert not result.stderr, f"import emitted output: {result.stderr!r}"


def test_loxcc_zip_round_trips():
    xml = loxone_xml.build_config_xml(
        controls=[loxone_xml.Control(type="InfoOnlyAnalog", title="T", analog=True)]
    )
    zip_bytes = loxcc.build_loxcc_zip(xml, loxcc.build_loxapp3())

    zf = zipfile.ZipFile(io.BytesIO(zip_bytes))
    assert set(zf.namelist()) == {"sps0.LoxCC", "LoxAPP3.json"}
    assert loxcc.decode_loxcc_block(zf.read("sps0.LoxCC")) == xml


def test_loxcc_frame_mode_round_trips():
    xml = "<ControlList/>\n"
    block = loxcc.build_loxcc_block(xml, mode="frame")
    assert loxcc.is_lz4_frame(block[16:])
    assert loxcc.decode_loxcc_block(block) == xml


def test_xml_builder_parses_into_controls(sample_controls):
    assert sample_controls.controls, "expected at least one parsed control"
    # keys are UUID bytes
    assert all(isinstance(k, bytes) for k in sample_controls.controls)
    # the visu Meter/Jalousie land in the websocket bucket, the rest in grabber
    assert sample_controls.websocket_controls
    assert sample_controls.grabber_controls
