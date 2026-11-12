"""Reusable test-support helpers for the loxInFlux test-suite.

Importable as a *top-level* ``support`` package thanks to ``pythonpath=["tests"]``
in ``pyproject.toml`` (mirrored by a ``sys.path`` insert in ``tests/conftest.py``).

Submodules:
- :mod:`support.loxone_xml` - programmatic Loxone config XML (``ControlList``) builder.
- :mod:`support.loxcc`      - LoxCC container + config-ZIP builders and lz4 helpers.
- :mod:`support.fakes`      - test doubles (aiohttp session, telegraf writer, FTP client).
"""
