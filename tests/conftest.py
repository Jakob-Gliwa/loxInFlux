"""Global pytest fixtures + explicit-init bootstrap for the loxInFlux test-suite.

============================================================================
Explicit config/telegraf init (no import-time side effects)
============================================================================
``import loxInFlux`` is side-effect free: neither the config nor the Telegraf
writer are constructed at import time. Instead we:
  1. put ``support`` on ``sys.path`` (top-level import, mirrors loxwebsocket),
  2. generate a throwaway fixture config dir/data dir,
  3. import loxInFlux, and
  4. re-initialise config + Telegraf writer from the fixture dirs before every
     test (autouse ``_init_singletons`` fixture), tearing them down afterwards.

Because ``AppConfig.load`` honours explicit ``config_dir``/``data_dir`` arguments
(and only consults ``get_args`` for values not supplied), the fixture config is
wired via ``init_config(config_dir=..., data_dir=...)``. Nothing in the suite
reaches the ``get_args()`` fallback, so ``sys.argv`` is left untouched - tests
that care about argument parsing drive ``sys.argv`` themselves
(``tests/unit/test_argparser.py``).

Live tests (``@pytest.mark.live``) are opt-in: they are skipped unless
``--run-live`` is passed, and the repo-root ``.env`` is loaded *only* when that
flag is set (no global autoload).

The generated fixture config uses **empty filter blacklists** (production ships a
huge one) so builder-generated controls actually survive parsing.
"""

from __future__ import annotations

import asyncio
import logging
import os
import sys
import tempfile
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

# --------------------------------------------------------------------------- #
# 1. make ``support`` importable as a top-level package                        #
# --------------------------------------------------------------------------- #
_TESTS_DIR = Path(__file__).parent
if str(_TESTS_DIR) not in sys.path:
    sys.path.insert(0, str(_TESTS_DIR))

# --------------------------------------------------------------------------- #
# 2. fixture-config bootstrap (BEFORE importing loxInFlux)                     #
# --------------------------------------------------------------------------- #
_BOOTSTRAP_CONFIG_TOML = """\
[general]
grabber = true
round_floats = false
rounding_precision = 5
grabber_interval = 300

[logging]
level = "INFO"

[miniserver]
host = "127.0.0.1"
port = 80
user = "admin"
password = "admin"
visu_password = "visu"
max_reconnect_attempts = 1

[paths]
data_dir = "data"

[telegraf]
host = "127.0.0.1"
port = 8094
protocol = "udp"
max_retries = 3

[telegraf.mqtt]
host = "127.0.0.1"
port = 1883
username = "mqttuser"
password = "mqttpass"
topic = "loxone/metrics"
client_id = "loxinflux-test"

[filters]
type_blacklist = []

[filters.websocket]
type_blacklist = []
type_whitelist = []
uuid_blacklist = []
uuid_whitelist = []

[filters.grabber]
type_blacklist = []
type_whitelist = []
uuid_blacklist = []
uuid_whitelist = []
"""


def _write_bootstrap_config() -> tuple[Path, Path]:
    """Create a temp ``config/config.toml`` + ``data/`` and return their paths."""
    root = Path(tempfile.mkdtemp(prefix="loxinflux-tests-"))
    config_dir = root / "config"
    data_dir = root / "data"
    config_dir.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.toml").write_text(_BOOTSTRAP_CONFIG_TOML, encoding="utf-8")
    return config_dir, data_dir


#: Public: the fixture config/data dirs the whole suite bootstraps against.
BOOTSTRAP_CONFIG_DIR, BOOTSTRAP_DATA_DIR = _write_bootstrap_config()

# --------------------------------------------------------------------------- #
# 3. import loxInFlux (side-effect free) + resolve real module objects         #
# --------------------------------------------------------------------------- #
from support import fakes, loxone_xml  # noqa: E402

import loxInFlux  # noqa: E402,F401
from loxInFlux.config import init_config, reset_config  # noqa: E402
from loxInFlux.miniserver import getControlsFromConfigXML  # noqa: E402
from loxInFlux.telegraf import init_telegraf, reset_telegraf  # noqa: E402

_config_mod = sys.modules["loxInFlux.config"]
_logger_mod = sys.modules["loxInFlux.logger"]
_miniserver_mod = sys.modules["loxInFlux.miniserver"]
_telegraf_mod = sys.modules["loxInFlux.telegraf"]


# --------------------------------------------------------------------------- #
# live opt-in: --run-live flag + collection skip + conditional .env load        #
# --------------------------------------------------------------------------- #
def pytest_addoption(parser: pytest.Parser) -> None:
    """Register ``--run-live`` to explicitly opt into live Miniserver tests."""
    parser.addoption(
        "--run-live",
        action="store_true",
        default=False,
        help="run @pytest.mark.live tests against a real Miniserver "
        "(loads the repo-root .env for LOXINFLUX_* credentials)",
    )


def pytest_configure(config: pytest.Config) -> None:
    """Load the repo-root ``.env`` only when live tests are opted into.

    Without ``--run-live`` there is no global autoload, so ordinary runs never
    pick up ambient credentials. Missing ``python-dotenv`` is tolerated.
    """
    if not config.getoption("--run-live"):
        return
    try:
        from dotenv import load_dotenv
    except ImportError:
        return
    load_dotenv(_TESTS_DIR.parent / ".env")


def pytest_collection_modifyitems(
    config: pytest.Config, items: list[pytest.Item]
) -> None:
    """Skip ``@pytest.mark.live`` items unless ``--run-live`` is passed."""
    if config.getoption("--run-live"):
        return
    skip_live = pytest.mark.skip(reason="live test: pass --run-live to enable")
    for item in items:
        if "live" in item.keywords:
            item.add_marker(skip_live)


# --------------------------------------------------------------------------- #
# autouse resets                                                               #
# --------------------------------------------------------------------------- #
@pytest.fixture(autouse=True)
def _init_singletons():
    """(Re-)initialise the config + Telegraf writer from the fixture dirs.

    Each test gets a freshly loaded config (so in-test mutations never leak) and
    a default (UDP) Telegraf writer; ``mock_telegraf`` overrides the writer where
    needed. Both are torn down afterwards so a stray ``get_config()`` in the next
    test's collection/setup fails loudly instead of leaking state.
    """
    init_config(config_dir=str(BOOTSTRAP_CONFIG_DIR), data_dir=str(BOOTSTRAP_DATA_DIR))
    init_telegraf()
    yield
    reset_telegraf()
    reset_config()


@pytest.fixture(autouse=True)
def _reset_miniserver_cache():
    """Clear the ``getControlsFromConfigXML`` lru-cache and the LoxAPP3 freshness
    cache so control parsing and cache-hit logic start clean each test."""
    _miniserver_mod.getControlsFromConfigXML.cache_clear()
    _miniserver_mod._loxapp3_cache_json_last_modified = None
    yield
    _miniserver_mod.getControlsFromConfigXML.cache_clear()
    _miniserver_mod._loxapp3_cache_json_last_modified = None


@pytest.fixture(autouse=True)
def _reset_logging_state():
    """Reset loxInFlux's logging configuration flag between tests.

    Deliberately does *not* clear the root logger's handlers, so pytest's own
    log-capture handler survives. Re-runs of ``configure_logging`` will refresh
    handlers themselves (production clears+re-adds on each run).
    """
    yield
    _logger_mod._configured = False
    logging.getLogger().setLevel(logging.WARNING)
    for lazy in _logger_mod._lazy_loggers.values():
        lazy._update_level_cache()


# --------------------------------------------------------------------------- #
# bootstrap path fixtures                                                      #
# --------------------------------------------------------------------------- #
@pytest.fixture
def bootstrap_config_dir() -> Path:
    """Path to the generated fixture ``config/`` dir (contains ``config.toml``)."""
    return BOOTSTRAP_CONFIG_DIR


@pytest.fixture
def bootstrap_data_dir() -> Path:
    """Path to the generated fixture ``data/`` dir (used as ``config.paths.data_dir``)."""
    return BOOTSTRAP_DATA_DIR


# --------------------------------------------------------------------------- #
# domain fixtures                                                             #
# --------------------------------------------------------------------------- #
@pytest.fixture
def sample_controls() -> SimpleNamespace:
    """A realistic set of parsed controls from builder-generated config XML.

    Returns a ``SimpleNamespace`` with:
      * ``xml``: the source XML string,
      * ``controls``: the full ``{uuid_bytes: control}`` dict,
      * ``websocket_controls``: the visu subset (app's websocket bucket),
      * ``grabber_controls``: the non-visu subset (app's grabber bucket).

    Control keys are UUID **bytes** (utf-8), matching ``getControlsFromConfigXML``.
    ``Category``/``Place`` types are blacklisted for this parse so only the real
    controls end up in the result (mirrors production behaviour).
    """
    category = loxone_xml.Category(title="Heizung")
    room = loxone_xml.Room(title="Wohnzimmer")
    controls = [
        loxone_xml.Control(
            type="InfoOnlyAnalog",
            title="Temperatur",
            desc="Raumtemperatur",
            unit="\u00b0C",
            analog=True,
            stats_type=2,
            category=category.uuid,
            room=room.uuid,
        ),
        loxone_xml.Control(
            type="InfoOnlyDigital",
            title="Fensterkontakt",
            room=room.uuid,
        ),
        loxone_xml.Control(
            type="Meter",
            title="Stromzaehler",
            analog=True,
            visu=True,
            category=category.uuid,
            cos=[loxone_xml.Co(key="AQ")],
        ),
        loxone_xml.Control(
            type="Jalousie",
            title="Rollo",
            visu=True,
            visu_pwd=True,
            room=room.uuid,
        ),
    ]
    xml = loxone_xml.build_config_xml(
        controls=controls, rooms=[room], categories=[category]
    )
    # Keep Category/Place elements out of the *control* set (they also match
    # ``.//C[@Type]``); production relies on the type_blacklist for this.
    _config_mod.get_config().control.type_blacklist = {"CATEGORY", "PLACE"}
    all_controls, websocket_controls, grabber_controls = getControlsFromConfigXML(xml)
    return SimpleNamespace(
        xml=xml,
        controls=all_controls,
        websocket_controls=websocket_controls,
        grabber_controls=grabber_controls,
    )


@pytest.fixture
def mock_telegraf(monkeypatch) -> fakes.FakeTelegrafWriter:
    """Install a recording fake as the process Telegraf writer.

    App code resolves the writer through ``get_telegraf()`` (which returns the
    module-level ``_writer``), so patching ``loxInFlux.telegraf._writer`` swaps in
    the fake everywhere. Assert on ``.writes`` (list of bytes points).
    """
    fake = fakes.FakeTelegrafWriter()
    monkeypatch.setattr(_telegraf_mod, "_writer", fake)
    return fake


@pytest.fixture
def local_sleep_patch(monkeypatch):
    """Factory that replaces ``asyncio.sleep`` with an instant (but still
    loop-yielding) stand-in, copying loxwebsocket's semantics.

    The replacement records each requested delay in ``.calls`` and awaits a real
    zero-length sleep so background tasks keep progressing (a non-yielding no-op
    would starve the very tasks a retry-loop test observes).

    Call it to apply the patch and get the fake back::

        sleeper = local_sleep_patch("loxInFlux.grabber", "loxInFlux.telegraf")
        ...
        assert sleeper.calls == [5]

    With no module paths it patches the global ``asyncio.sleep`` (process-wide
    for the test), which covers every module that calls ``asyncio.sleep``.
    """
    real_sleep = asyncio.sleep
    calls: list[float] = []

    async def fake_sleep(delay, result=None):
        calls.append(delay)
        return await real_sleep(0, result)

    fake_sleep.calls = calls  # type: ignore[attr-defined]

    def apply(*module_paths: str):
        if not module_paths:
            monkeypatch.setattr(asyncio, "sleep", fake_sleep)
        else:
            # Resolve each module via ``sys.modules`` and patch the ``asyncio``
            # the module actually imported (its module-level reference), rather
            # than letting monkeypatch attribute-walk a dotted string.
            for module_path in module_paths:
                module = sys.modules[module_path]
                monkeypatch.setattr(module.asyncio, "sleep", fake_sleep)
        return fake_sleep

    apply.calls = calls  # type: ignore[attr-defined]
    return apply


@pytest.fixture
def set_tz(monkeypatch):
    """Factory to switch the process timezone (restored on teardown).

    Usage: ``set_tz("Europe/Berlin")``. Unix-only (uses ``time.tzset``).
    """
    applied = False

    def _set(tz: str):
        nonlocal applied
        monkeypatch.setenv("TZ", tz)
        time.tzset()
        applied = True

    yield _set
    if applied:
        monkeypatch.undo()
        time.tzset()


@pytest.fixture
def live_credentials() -> SimpleNamespace:
    """Credentials for ``@pytest.mark.live`` tests; skips when env is incomplete.

    Reads ``LOXINFLUX_URL`` / ``LOXINFLUX_USERNAME`` / ``LOXINFLUX_PASSWORD``
    and returns them as ``SimpleNamespace(url, username, password)``. Skips the
    test (rather than failing) when any is missing.
    """
    url = os.getenv("LOXINFLUX_URL")
    username = os.getenv("LOXINFLUX_USERNAME")
    password = os.getenv("LOXINFLUX_PASSWORD")
    if not (url and username and password):
        pytest.skip(
            "live credentials not set "
            "(LOXINFLUX_URL / LOXINFLUX_USERNAME / LOXINFLUX_PASSWORD)"
        )
    return SimpleNamespace(url=url, username=username, password=password)
