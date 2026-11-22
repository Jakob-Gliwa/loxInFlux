#!/usr/bin/env python3
"""Offline schema parity on a real XML export; run in loxInFlux's Python env.

Usage: python compare_legacy.py LEGACY_REPO CONFIG_TEMPLATE XML RUST_INSPECT_BINARY
No network requests; reports counts only, never control names or credentials.
"""
import json
import logging
import pathlib
import shutil
import subprocess
import sys
import tempfile
import time

legacy, template, xml_path, rust_binary = map(pathlib.Path, sys.argv[1:])
sys.path.insert(0, str(legacy / "src"))
from loxInFlux.config import init_config
from loxInFlux.miniserver import getControlsFromConfigXML

with tempfile.TemporaryDirectory() as tmp:
    shutil.copyfile(template, pathlib.Path(tmp) / "config.toml")
    init_config(config_dir=tmp, data_dir=tmp)
    logging.disable(logging.CRITICAL)
    started = time.perf_counter()
    all_controls, _, grabber = getControlsFromConfigXML(xml_path.read_text())
    duration = time.perf_counter() - started
    expected_ws = {k.decode(): v["point_websocket"].decode() for k, v in all_controls.items()}
    expected_grab = {
        k.decode(): {
            "prefix": v["pointInflux"].tag("source", "grabber").field("Default", 0.0).to_line_protocol().split(" Default=")[0] + " ",
            "secured": v["VisuPwd"],
        }
        for k, v in grabber.items()
    }
    actual = json.loads(subprocess.check_output([str(rust_binary), str(template), str(xml_path), "--json"]))
    print(f"Legacy parse: {duration:.3f}s; websocket routes: {len(expected_ws)}; grabber: {len(expected_grab)}")
    failed = False
    for name, expected in [("websocket", expected_ws), ("grabber", expected_grab)]:
        got = actual[name]
        missing = expected.keys() - got.keys()
        extra = got.keys() - expected.keys()
        different = sum(expected[k] != got[k] for k in expected.keys() & got.keys())
        print(f"{name}: missing={len(missing)}, extra={len(extra)}, different={different}")
        failed |= bool(missing or extra or different)
    sys.exit(1 if failed else 0)
