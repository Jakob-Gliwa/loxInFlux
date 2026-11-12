"""Integration tests for ``miniserver.load_miniserver_config``.

The production code downloads the active Loxone config over the **HTTP**
filesystem API (aiohttp), not FTP:

    GET {base_url}/dev/fslist/prog/      -> plaintext listing, one file per line
    GET {base_url}/dev/fsget/prog/<file> -> the config ZIP (sps0.LoxCC + LoxAPP3.json)

We drive it with :class:`support.fakes.FakeAiohttpSession` (patched onto
``aiohttp.ClientSession``) and craft the ZIP payloads with
``support.loxcc.build_loxcc_zip``. ``utils.get_loxapp3_json_last_modified`` uses
the *same* aiohttp surface, so a single patched session serves both the
freshness probe (``/jdev/sps/LoxAPPversion3``) and the download routes.

Regressions covered explicitly:
  * miniserver **port** config -> ``_build_base_url`` builds host:port correctly,
  * **payload-length** enforcement in the LoxCC container,
  * the cache-freshness **await** bug (``get_loxapp3_json_last_modified`` must be
    awaited and its ``datetime`` result compared for cache reuse vs refetch).
"""

from __future__ import annotations

import zipfile
from datetime import datetime

import aiohttp
import orjson
import pytest
from support import fakes, loxcc

import loxInFlux.miniserver as miniserver_mod
from loxInFlux.config import get_config
from loxInFlux.miniserver import load_miniserver_config

# The config bootstrapped by conftest: host 127.0.0.1, port 80 -> http, no port.
_LISTING_TWO = (
    "sps_0252_20240101120000.zip\nsps_0252_20240102120000.zip\nsomethingelse.txt\n"
)
_NEWEST = "sps_0252_20240102120000.zip"


def _valid_xml() -> str:
    return '<?xml version="1.0" encoding="utf-8"?>\n<ControlList Version="252"/>\n'


def _patch_session(monkeypatch, session: fakes.FakeAiohttpSession) -> None:
    """Patch ``aiohttp.ClientSession`` (shared by miniserver + utils)."""
    monkeypatch.setattr(
        "loxInFlux.miniserver.aiohttp.ClientSession", session.as_factory()
    )


def _download_session(
    listing: str = _LISTING_TWO,
    *,
    zip_bytes: bytes | None = None,
    xml: str | None = None,
    last_modified: str | None = None,
    **zip_kwargs,
) -> fakes.FakeAiohttpSession:
    if zip_bytes is None:
        zip_bytes = loxcc.build_loxcc_zip(xml or _valid_xml(), **zip_kwargs)
    session = fakes.FakeAiohttpSession()
    if last_modified is not None:
        session.add_route(
            "/jdev/sps/LoxAPPversion3",
            fakes.FakeHTTPResponse(json_data={"LL": {"value": last_modified}}),
        )
    session.add_route("/dev/fslist/prog/", fakes.FakeHTTPResponse(text=listing))
    session.add_route("/dev/fsget/prog/", fakes.FakeHTTPResponse(body=zip_bytes))
    return session


# --------------------------------------------------------------------------- #
# happy path                                                                   #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_happy_path_downloads_and_decodes(monkeypatch):
    xml = _valid_xml()
    session = _download_session(xml=xml)
    _patch_session(monkeypatch, session)

    config_content, loxapp3_json = await load_miniserver_config("admin", "secret")

    assert config_content == xml
    assert loxapp3_json["lastModified"] == "2024-01-01 00:00:00"
    # Two HTTP GETs: list then fetch the newest archive.
    urls = [url for url, _ in session.requests]
    assert urls == [
        "http://127.0.0.1/dev/fslist/prog/",
        f"http://127.0.0.1/dev/fsget/prog/{_NEWEST}",
    ]
    # Freshness probe is skipped when the module cache is empty (default).
    assert not any("LoxAPPversion3" in url for url in urls)


@pytest.mark.asyncio
async def test_selects_newest_matching_archive(monkeypatch):
    listing = (
        "sps_0252_20200101000000.zip\n"
        "sps_0100_20260101000000.zip\n"  # newest by timestamp, LOWER version prefix
        "sps_0252_20240102120000.LoxCC\n"
        "README\n"
    )
    session = _download_session(listing=listing)
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    # The 2026 archive is newest by timestamp even though its version prefix
    # (0100) sorts before the others - a plain lexicographic sort would wrongly
    # pick "sps_0252_20240102120000.LoxCC".
    fetched = session.requests[-1][0]
    assert fetched.endswith("/dev/fsget/prog/sps_0100_20260101000000.zip")


# --------------------------------------------------------------------------- #
# A4: archive selection by timestamp (not lexicographic filename sort)         #
# --------------------------------------------------------------------------- #
def test_select_newest_by_timestamp_version_rollover():
    # Version rolled over 9 -> 10: the newer config (v10) carries the newer
    # timestamp but its "sps_10_..." name sorts BEFORE "sps_9_..." because the
    # version prefix dominates a string sort ("1" < "9"). Timestamp-based
    # selection must still pick the v10 archive.
    listing = "sps_10_20250101000000.zip\nsps_9_20240101000000.zip\n"

    # Sanity: a plain lexicographic sort would pick the WRONG (older v9) file.
    assert sorted(listing.split())[-1] == "sps_9_20240101000000.zip"

    assert (
        miniserver_mod._select_newest_config_file(listing)
        == "sps_10_20250101000000.zip"
    )


def test_select_newest_by_timestamp_unequal_digit_lengths():
    # Unequal timestamp digit lengths: "100" > "9" numerically, but lexically
    # "..._100" < "..._9" (compares char-by-char, "1" < "9"). Numeric parse wins.
    listing = "sps_9_9.zip\nsps_9_100.zip\n"

    assert sorted(listing.split())[-1] == "sps_9_9.zip"  # lexicographic (wrong)
    assert miniserver_mod._select_newest_config_file(listing) == "sps_9_100.zip"


def test_select_newest_tiebreak_by_version():
    # Identical timestamps -> the higher version wins the tiebreak.
    listing = "sps_9_20240101000000.zip\nsps_11_20240101000000.LoxCC\n"
    assert (
        miniserver_mod._select_newest_config_file(listing)
        == "sps_11_20240101000000.LoxCC"
    )


def test_select_newest_returns_none_without_matches():
    assert miniserver_mod._select_newest_config_file("README\nnotes.txt\n") is None


@pytest.mark.asyncio
async def test_version_rollover_downloads_newest_by_timestamp(monkeypatch):
    # Full flow: the rolled-over v10 archive (newer timestamp, lower-sorting
    # name) is the one fetched.
    listing = "sps_10_20250101000000.zip\nsps_9_20240101000000.zip\n"
    session = _download_session(listing=listing)
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    assert session.requests[-1][0].endswith("/dev/fsget/prog/sps_10_20250101000000.zip")


@pytest.mark.asyncio
async def test_frame_mode_payload_decodes(monkeypatch):
    xml = _valid_xml()
    session = _download_session(xml=xml, mode="frame")
    _patch_session(monkeypatch, session)

    config_content, _ = await load_miniserver_config("admin", "secret")
    assert config_content == xml


@pytest.mark.asyncio
async def test_persist_writes_files(monkeypatch, tmp_path):
    get_config().paths.data_dir = str(tmp_path)
    xml = _valid_xml()
    session = _download_session(xml=xml)
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret", persist=True)

    out_xml = tmp_path / f"{_NEWEST.replace('.zip', '')}.xml"
    out_json = tmp_path / "LoxAPP3.json"
    assert out_xml.read_text(encoding="utf-8") == xml
    assert orjson.loads(out_json.read_bytes())["lastModified"] == "2024-01-01 00:00:00"


# --------------------------------------------------------------------------- #
# REGRESSION: miniserver port config (base_url = host + port)                  #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_port_config_builds_url_with_custom_port(monkeypatch):
    get_config().miniserver.host = "loxone.fritz.box"
    get_config().miniserver.port = 8080
    session = _download_session()
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    urls = [url for url, _ in session.requests]
    assert urls[0] == "http://loxone.fritz.box:8080/dev/fslist/prog/"
    assert urls[1].startswith("http://loxone.fritz.box:8080/dev/fsget/prog/")


@pytest.mark.asyncio
async def test_port_443_builds_https_without_port(monkeypatch):
    get_config().miniserver.host = "secure.example"
    get_config().miniserver.port = 443
    session = _download_session()
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    assert session.requests[0][0] == "https://secure.example/dev/fslist/prog/"


@pytest.mark.asyncio
async def test_port_80_builds_http_without_port(monkeypatch):
    get_config().miniserver.host = "plain.example"
    get_config().miniserver.port = 80
    session = _download_session()
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    assert session.requests[0][0] == "http://plain.example/dev/fslist/prog/"


# --------------------------------------------------------------------------- #
# REGRESSION: LoxCC payload-length enforcement                                 #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_payload_length_mismatch_declared_too_large(monkeypatch):
    # Declare a compressedSize far larger than the actual payload: miniserver
    # reads only the real bytes and must reject the short read.
    session = _download_session(compressed_size=1_000_000)
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="Payload length mismatch"):
        await load_miniserver_config("admin", "secret")


@pytest.mark.asyncio
async def test_declared_short_but_corrupt_payload_rejected(monkeypatch):
    # Payload bytes that pass the length check (declared == actual) but are
    # truncated lz4 -> decompression must fail rather than silently succeed.
    block_ok = loxcc.build_loxcc_block(_valid_xml())
    real_payload = block_ok[16:]
    session = _download_session(payload=real_payload[:-3])
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="LZ4 decompression failed"):
        await load_miniserver_config("admin", "secret")


# --------------------------------------------------------------------------- #
# other container error paths                                                  #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_invalid_header_rejected(monkeypatch):
    session = _download_session(header=0x12345678)
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="Invalid file format"):
        await load_miniserver_config("admin", "secret")


@pytest.mark.asyncio
async def test_checksum_failure_rejected(monkeypatch):
    session = _download_session(checksum=0xDEADBEEF)
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="Checksum verification failed"):
        await load_miniserver_config("admin", "secret")


@pytest.mark.asyncio
async def test_uncompressed_size_mismatch_rejected(monkeypatch):
    # Frame payload decompresses regardless of the declared uncompressedSize,
    # so the checksum passes and the size check is the one that fires.
    session = _download_session(mode="frame", uncompressed_size=999_999)
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="Uncompressed filesize mismatch"):
        await load_miniserver_config("admin", "secret")


@pytest.mark.asyncio
async def test_missing_loxcc_member_rejected(monkeypatch):
    session = _download_session(loxcc_name="wrong.LoxCC")
    _patch_session(monkeypatch, session)

    with pytest.raises(KeyError):
        await load_miniserver_config("admin", "secret")


@pytest.mark.asyncio
async def test_empty_listing_raises(monkeypatch):
    session = _download_session(listing="no matching entries here\n")
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="No configuration files found"):
        await load_miniserver_config("admin", "secret")


@pytest.mark.asyncio
async def test_listing_http_error_propagates(monkeypatch):
    session = fakes.FakeAiohttpSession()
    session.add_route(
        "/dev/fslist/prog/",
        fakes.FakeHTTPResponse(raise_for_status=RuntimeError("HTTP 500")),
    )
    _patch_session(monkeypatch, session)

    with pytest.raises(RuntimeError, match="HTTP 500"):
        await load_miniserver_config("admin", "secret")


# --------------------------------------------------------------------------- #
# REGRESSION: cache-freshness / await bug                                      #
# --------------------------------------------------------------------------- #
def _write_cache(tmp_path, *, last_modified: str) -> str:
    xml_name = "sps_0252_20240102120000.xml"
    (tmp_path / xml_name).write_text(
        "<ControlList Version='cached'/>", encoding="utf-8"
    )
    (tmp_path / "LoxAPP3.json").write_bytes(
        orjson.dumps({"lastModified": last_modified, "controls": {}})
    )
    return xml_name


@pytest.mark.asyncio
async def test_cache_hit_reuses_local_files_without_download(monkeypatch, tmp_path):
    # Local cache is at least as fresh as the remote LoxAPP3.json -> reuse it and
    # perform NO download. Requires get_loxapp3_json_last_modified() to be
    # awaited and its datetime compared (the historical await bug).
    get_config().paths.data_dir = str(tmp_path)
    _write_cache(tmp_path, last_modified="2024-01-02 00:00:00")
    miniserver_mod._loxapp3_cache_json_last_modified = datetime(2024, 1, 2, 0, 0, 0)

    session = fakes.FakeAiohttpSession()
    session.add_route(
        "/jdev/sps/LoxAPPversion3",
        fakes.FakeHTTPResponse(json_data={"LL": {"value": "2024-01-01 00:00:00"}}),
    )
    # NOTE: no fslist/fsget routes -> any download attempt raises loudly.
    _patch_session(monkeypatch, session)

    config_content, loxapp3_json = await load_miniserver_config("admin", "secret")

    assert config_content == "<ControlList Version='cached'/>"
    assert loxapp3_json["lastModified"] == "2024-01-02 00:00:00"
    # Only the freshness probe was issued; nothing was downloaded.
    assert [url for url, _ in session.requests] == [
        "http://127.0.0.1/jdev/sps/LoxAPPversion3"
    ]


@pytest.mark.asyncio
async def test_cache_stale_refetches_when_remote_is_newer(monkeypatch, tmp_path):
    # Remote LoxAPP3.json is newer than the local cache -> re-download. If the
    # freshness result were left un-awaited, the datetime comparison would raise
    # a TypeError instead of downloading, so this pins the await/compare logic.
    get_config().paths.data_dir = str(tmp_path)
    _write_cache(tmp_path, last_modified="2024-01-01 00:00:00")
    miniserver_mod._loxapp3_cache_json_last_modified = datetime(2024, 1, 1, 0, 0, 0)

    fresh_xml = "<?xml version='1.0'?>\n<ControlList Version='fresh'/>\n"
    session = _download_session(xml=fresh_xml, last_modified="2024-01-02 00:00:00")
    _patch_session(monkeypatch, session)

    config_content, _ = await load_miniserver_config("admin", "secret")

    assert config_content == fresh_xml
    urls = [url for url, _ in session.requests]
    assert any("/dev/fsget/prog/" in url for url in urls)


@pytest.mark.asyncio
async def test_use_cache_false_skips_freshness_probe(monkeypatch, tmp_path):
    # Even with a populated module cache, use_cache=False must download directly
    # and never issue the LoxAPPversion3 freshness probe.
    get_config().paths.data_dir = str(tmp_path)
    _write_cache(tmp_path, last_modified="2024-01-02 00:00:00")
    miniserver_mod._loxapp3_cache_json_last_modified = datetime(2024, 1, 2, 0, 0, 0)

    session = _download_session()  # no LoxAPPversion3 route
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret", use_cache=False)

    urls = [url for url, _ in session.requests]
    assert not any("LoxAPPversion3" in url for url in urls)
    assert any("/dev/fsget/prog/" in url for url in urls)


# --------------------------------------------------------------------------- #
# Cold-cache start: no usable local files -> download + persist                #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_cold_cache_missing_files_downloads_and_persists(
    monkeypatch, tmp_path, caplog
):
    # The module cache claims to be fresh (>= remote), and a stale ``sps_*.xml``
    # is present, but ``LoxAPP3.json`` is gone -> we must NOT reuse the cache;
    # instead fall through the "No cached configuration files found" branch to a
    # fresh download + persist.
    get_config().paths.data_dir = str(tmp_path)
    (tmp_path / "sps_0100_20200101000000.xml").write_text(
        "<ControlList Version='stale'/>", encoding="utf-8"
    )  # non-empty cached_files, but no LoxAPP3.json alongside it
    miniserver_mod._loxapp3_cache_json_last_modified = datetime(2024, 1, 2, 0, 0, 0)

    fresh_xml = "<?xml version='1.0'?>\n<ControlList Version='fresh'/>\n"
    session = _download_session(xml=fresh_xml, last_modified="2024-01-01 00:00:00")
    _patch_session(monkeypatch, session)

    with caplog.at_level("INFO"):
        config_content, _ = await load_miniserver_config(
            "admin", "secret", persist=True
        )

    assert "No cached configuration files found" in caplog.text
    assert config_content == fresh_xml
    # A real download happened despite the "fresh" module cache ...
    assert any("/dev/fsget/prog/" in url for url, _ in session.requests)
    # ... and the freshly downloaded config was persisted.
    out_xml = tmp_path / f"{_NEWEST.replace('.zip', '')}.xml"
    assert out_xml.read_text(encoding="utf-8") == fresh_xml
    assert (tmp_path / "LoxAPP3.json").exists()


# --------------------------------------------------------------------------- #
# HTTP transport contract (auth / timeout / ssl / raise_for_status / cleanup)  #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_session_built_with_basic_auth_header_and_timeout(monkeypatch):
    # E1: credentials are sent as a static ``Authorization: Basic ...`` header
    # (aiohttp.BasicAuth is deprecated), and a 30s ClientTimeout is configured.
    session = _download_session()
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    kwargs = session.session_kwargs
    assert kwargs["headers"]["Authorization"] == aiohttp.encode_basic_auth(
        "admin", "secret"
    )
    # No deprecated ``auth=`` kwarg is used any more.
    assert "auth" not in kwargs
    assert isinstance(kwargs["timeout"], aiohttp.ClientTimeout)
    assert kwargs["timeout"].total == 30


@pytest.mark.asyncio
async def test_every_get_disables_ssl_verification(monkeypatch):
    # Loxone miniservers use self-signed certs; each GET must pass ssl=False.
    session = _download_session()
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    assert session.requests  # both the listing and the download were issued
    assert all(kwargs.get("ssl") is False for _url, kwargs in session.requests)


@pytest.mark.asyncio
async def test_session_is_entered_and_closed(monkeypatch):
    # The ClientSession is used as an async-context-manager: entered once and
    # cleanly closed on the way out.
    session = _download_session()
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    assert session.entered == 1
    assert session.closed is True


@pytest.mark.asyncio
async def test_non_200_download_triggers_raise_for_status(monkeypatch):
    # A non-200 on the archive download must surface via raise_for_status()
    # rather than being decoded as a config.
    session = fakes.FakeAiohttpSession()
    session.add_route("/dev/fslist/prog/", fakes.FakeHTTPResponse(text=_LISTING_TWO))
    session.add_route(
        "/dev/fsget/prog/",
        fakes.FakeHTTPResponse(
            status=500, raise_for_status=RuntimeError("HTTP 500 Server Error")
        ),
    )
    _patch_session(monkeypatch, session)

    with pytest.raises(RuntimeError, match="HTTP 500"):
        await load_miniserver_config("admin", "secret")

    # Cleanup still runs even on the error path.
    assert session.closed is True


# --------------------------------------------------------------------------- #
# REGRESSION: firmware 17 writes a bare .LoxCC next to the deployment ZIP      #
#                                                                             #
# Since Miniserver firmware 17 (verified on 17.1.6.30) /prog holds TWO files   #
# per program state: the ZIP deployed from Loxone Config plus a bare .LoxCC    #
# container the Miniserver itself writes a few seconds later for the running   #
# program. The .LoxCC therefore has the NEWER timestamp and wins the "newest"  #
# selection - and it is not a ZIP, so handing it to zipfile used to abort the  #
# whole config load with "File is not a zip file". A bare container also has   #
# no LoxAPP3.json member, so the structure file must be fetched separately.    #
# --------------------------------------------------------------------------- #
_LISTING_FW17 = (
    "sps_0272_20260727223718.zip\n"  # deployment package from Loxone Config
    "sps_0272_20260727223721.LoxCC\n"  # running program, written by the Miniserver
    "Emergency.LoxCC\n"  # must never be selected
    "Music.json\n"
)
_FW17_NEWEST = "sps_0272_20260727223721.LoxCC"


def _loxcc_session(
    listing: str = _LISTING_FW17,
    *,
    xml: str | None = None,
    loxapp3: object = None,
    loxcc_bytes: bytes | None = None,
    **block_kwargs,
) -> fakes.FakeAiohttpSession:
    """Session serving a BARE .LoxCC from /prog plus /data/LoxAPP3.json."""
    if loxcc_bytes is None:
        loxcc_bytes = loxcc.build_loxcc_block(xml or _valid_xml(), **block_kwargs)
    if loxapp3 is None:
        loxapp3 = loxcc.build_loxapp3(last_modified="2026-07-27 22:37:21")
    session = fakes.FakeAiohttpSession()
    session.add_route("/dev/fslist/prog/", fakes.FakeHTTPResponse(text=listing))
    session.add_route("/dev/fsget/prog/", fakes.FakeHTTPResponse(body=loxcc_bytes))
    session.add_route(
        "/data/LoxAPP3.json", fakes.FakeHTTPResponse(body=orjson.dumps(loxapp3))
    )
    return session


@pytest.mark.asyncio
async def test_bare_loxcc_payload_decodes_and_fetches_loxapp3(monkeypatch):
    xml = _valid_xml()
    session = _loxcc_session(xml=xml)
    _patch_session(monkeypatch, session)

    config_content, loxapp3_json = await load_miniserver_config("admin", "secret")

    assert config_content == xml
    assert loxapp3_json["lastModified"] == "2026-07-27 22:37:21"
    # The .LoxCC is newer than the .zip, so it is the one downloaded, and
    # LoxAPP3.json needs its own request because the container has no such member.
    urls = [url for url, _ in session.requests]
    assert urls == [
        "http://127.0.0.1/dev/fslist/prog/",
        f"http://127.0.0.1/dev/fsget/prog/{_FW17_NEWEST}",
        "http://127.0.0.1/data/LoxAPP3.json",
    ]


def test_fw17_listing_selects_the_loxcc_not_the_zip():
    # Selection is purely by the trailing timestamp, so the .LoxCC the Miniserver
    # wrote 3 seconds after the ZIP wins. Emergency.LoxCC / Music.json must not
    # match the pattern at all.
    assert miniserver_mod._select_newest_config_file(_LISTING_FW17) == _FW17_NEWEST


@pytest.mark.asyncio
async def test_zip_payload_still_needs_no_extra_loxapp3_request(monkeypatch):
    # Guard the other direction: when /prog yields the ZIP, LoxAPP3.json comes
    # from inside it and /data/LoxAPP3.json must NOT be requested.
    session = _download_session()
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret")

    assert not any("/data/LoxAPP3.json" in url for url, _ in session.requests)


@pytest.mark.asyncio
async def test_persist_strips_loxcc_extension(monkeypatch, tmp_path):
    get_config().paths.data_dir = str(tmp_path)
    xml = _valid_xml()
    session = _loxcc_session(xml=xml)
    _patch_session(monkeypatch, session)

    await load_miniserver_config("admin", "secret", persist=True)

    assert (tmp_path / f"{_FW17_NEWEST.replace('.LoxCC', '')}.xml").read_text(
        encoding="utf-8"
    ) == xml


@pytest.mark.asyncio
async def test_bare_loxcc_container_errors_still_enforced(monkeypatch):
    # The bare-container path must run the exact same integrity checks as the
    # ZIP member path (shared _read_loxcc_stream), not a laxer variant.
    session = _loxcc_session(checksum=0xDEADBEEF)
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="Checksum verification failed"):
        await load_miniserver_config("admin", "secret")


# --------------------------------------------------------------------------- #
# REGRESSION: HTTP 200 + LL error body must not surface as a zip/lz4 error     #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_ll_error_body_reported_as_unexpected_payload(monkeypatch):
    # The Miniserver answers a rejected /dev/... request with HTTP 200 and an LL
    # error body (e.g. code 403 without full-access rights), which
    # raise_for_status() cannot see. The payload is neither ZIP nor LoxCC, so we
    # must say so instead of failing with "File is not a zip file".
    body = b'{"LL": {"control": "dev/fsget/prog/", "code": "403"}}'
    session = _loxcc_session(loxcc_bytes=body)
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="Unexpected configuration payload"):
        await load_miniserver_config("admin", "secret")


@pytest.mark.asyncio
async def test_loxapp3_error_body_reported_clearly(monkeypatch):
    # Same trap on the separate LoxAPP3.json fetch: an LL error body parses as
    # JSON, so guard on 'lastModified' rather than dying later with a KeyError.
    session = _loxcc_session(loxapp3={"LL": {"code": "403"}})
    _patch_session(monkeypatch, session)

    with pytest.raises(Exception, match="missing 'lastModified'"):
        await load_miniserver_config("admin", "secret")


# --------------------------------------------------------------------------- #
# unit-level: payload dispatch is by magic bytes, not by filename              #
# --------------------------------------------------------------------------- #
def test_decode_config_payload_accepts_bare_loxcc():
    xml = _valid_xml()
    config_content, json_content = miniserver_mod._decode_config_payload(
        loxcc.build_loxcc_block(xml)
    )
    assert config_content == xml
    # No LoxAPP3.json inside a bare container -> caller has to fetch it.
    assert json_content is None


def test_decode_config_payload_accepts_zip():
    xml = _valid_xml()
    config_content, json_content = miniserver_mod._decode_config_payload(
        loxcc.build_loxcc_zip(xml)
    )
    assert config_content == xml
    assert json_content["lastModified"] == "2024-01-01 00:00:00"


@pytest.mark.parametrize(
    "payload",
    [
        pytest.param(b"", id="empty"),
        pytest.param(b"\xee\xcc", id="truncated-loxcc-magic"),
        pytest.param(b"\xee\xcc\xbb", id="loxcc-magic-one-byte-short"),
        pytest.param(b'{"LL": {"code": "403"}}', id="ll-error-body"),
        pytest.param(b"<html>login</html>", id="html"),
    ],
)
def test_decode_config_payload_rejects_unknown_shapes(payload):
    # Anything that carries neither magic is reported as such, with the leading
    # bytes included so an LL error body is recognisable in the log.
    with pytest.raises(Exception, match="Unexpected configuration payload"):
        miniserver_mod._decode_config_payload(payload)


def test_decode_config_payload_zip_magic_without_archive():
    # ``b"PK"`` alone carries the ZIP magic, so dispatch commits to the ZIP path
    # and the failure comes from zipfile itself rather than the magic check.
    with pytest.raises(zipfile.BadZipFile):
        miniserver_mod._decode_config_payload(b"PK")
