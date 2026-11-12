"""Test doubles for external I/O touched by loxInFlux.

The most important double is :class:`FakeAiohttpSession`: the current
``miniserver.load_miniserver_config`` downloads the config over **HTTP**
(aiohttp), *not* FTP - it lists ``/dev/fslist/prog/`` and downloads
``/dev/fsget/prog/<file>``. ``utils.get_loxapp3_json_last_modified`` uses the
same aiohttp surface. Patch ``loxInFlux.miniserver.aiohttp.ClientSession`` (and
``loxInFlux.utils.aiohttp.ClientSession``) with ``FakeAiohttpSession.as_factory()``.
"""

from __future__ import annotations

import json as _json
from collections.abc import Callable, Iterable
from typing import Any

__all__ = [
    "FakeTelegrafWriter",
    "FakeDatagramTransport",
    "FakeStreamWriter",
    "FakeStreamReader",
    "FakeHTTPResponse",
    "FakeAiohttpSession",
]

_UNSET = object()


# --------------------------------------------------------------------------- #
# Telegraf                                                                     #
# --------------------------------------------------------------------------- #
class FakeTelegrafWriter:
    """Drop-in for the Telegraf writer returned by ``get_telegraf()``.

    Records every ``write(point)`` in ``.writes`` (a list of ``bytes``) and
    tracks lifecycle calls. Satisfies the ``TelegrafWriter`` surface
    (``initialize``/``connect``/``close``/``write`` + async-context-manager).
    """

    def __init__(self) -> None:
        self.writes: list[bytes] = []
        self.initialized = False
        self.connected = False
        self.closed = False

    async def initialize(self) -> FakeTelegrafWriter:
        self.initialized = True
        return self

    async def connect(self) -> None:
        self.connected = True

    async def close(self) -> None:
        self.closed = True

    async def write(self, point: bytes) -> None:
        self.writes.append(point)

    async def __aenter__(self) -> FakeTelegrafWriter:
        await self.initialize()
        return self

    async def __aexit__(self, *exc: object) -> bool:
        await self.close()
        return False


class FakeDatagramTransport:
    """Stand-in for the transport returned by ``loop.create_datagram_endpoint``.

    Only the surface ``UDPTelegrafWriter`` touches: ``sendto`` + ``close``.
    """

    def __init__(self, *, fail_sendto: BaseException | None = None) -> None:
        self.sent: list[bytes] = []
        self.closed = False
        self._fail_sendto = fail_sendto

    def sendto(self, data: bytes, addr: Any = None) -> None:
        if self._fail_sendto is not None:
            raise self._fail_sendto
        self.sent.append(bytes(data))

    def close(self) -> None:
        self.closed = True


class FakeStreamReader:
    """Minimal ``asyncio.StreamReader`` stand-in (unused by the TCP writer path)."""

    def __init__(self, chunks: Iterable[bytes] = ()) -> None:
        self._chunks = list(chunks)

    async def read(self, n: int = -1) -> bytes:
        return self._chunks.pop(0) if self._chunks else b""


class FakeStreamWriter:
    """Stand-in for the ``asyncio.StreamWriter`` used by ``TCPTelegrafWriter``.

    Records writes in ``.data`` and tracks drain/close. Set ``fail_write`` /
    ``fail_drain`` to exercise the writer's reconnect path.
    """

    def __init__(
        self,
        *,
        fail_write: BaseException | None = None,
        fail_drain: BaseException | None = None,
    ) -> None:
        self.data = bytearray()
        self.drained = 0
        self.closed = False
        self.wait_closed_called = False
        self._fail_write = fail_write
        self._fail_drain = fail_drain

    def write(self, data: bytes) -> None:
        if self._fail_write is not None:
            raise self._fail_write
        self.data.extend(data)

    async def drain(self) -> None:
        if self._fail_drain is not None:
            raise self._fail_drain
        self.drained += 1

    def close(self) -> None:
        self.closed = True

    async def wait_closed(self) -> None:
        self.wait_closed_called = True


# --------------------------------------------------------------------------- #
# aiohttp (HTTP filesystem API used by miniserver.load_miniserver_config)      #
# --------------------------------------------------------------------------- #
class FakeHTTPResponse:
    """Stand-in for an aiohttp response used as ``async with ... as response``.

    Supports the surface the loxInFlux code touches:
    ``raise_for_status()``, ``await text()``, ``await read()``,
    ``await json(loads=...)`` and the ``.status`` attribute.
    """

    def __init__(
        self,
        *,
        status: int = 200,
        body: bytes | str = b"",
        text: str | None = None,
        json_data: Any = _UNSET,
        raise_for_status: BaseException | None = None,
    ) -> None:
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.status = status
        self._body = bytes(body)
        self._text = text if text is not None else self._body.decode("utf-8", "replace")
        self._json = json_data
        self._raise = raise_for_status

    async def __aenter__(self) -> FakeHTTPResponse:
        return self

    async def __aexit__(self, *exc: object) -> bool:
        return False

    def raise_for_status(self) -> None:
        if self._raise is not None:
            raise self._raise

    async def text(self) -> str:
        return self._text

    async def read(self) -> bytes:
        return self._body

    async def json(
        self, *, loads: Callable[[bytes], Any] | None = None, **_: Any
    ) -> Any:
        if self._json is not _UNSET:
            return self._json
        loads = loads or _json.loads
        return loads(self._body)


# A route is (matcher, response); matcher is a URL substring or a predicate.
_Route = tuple[Any, "FakeHTTPResponse | Callable[..., FakeHTTPResponse]"]


class FakeAiohttpSession:
    """Scriptable stand-in for ``aiohttp.ClientSession``.

    Usage::

        session = FakeAiohttpSession()
        session.add_route("/dev/fslist/prog/", FakeHTTPResponse(text="sps_1_2.zip\\n"))
        session.add_route("/dev/fsget/prog/", FakeHTTPResponse(body=zip_bytes))
        monkeypatch.setattr(
            "loxInFlux.miniserver.aiohttp.ClientSession", session.as_factory()
        )

    ``add_route`` matchers are matched in order; a ``str`` matches as a URL
    substring, a callable is used as a predicate ``matcher(url) -> bool``.
    Requested ``(url, kwargs)`` pairs are recorded in ``.requests``.

    The ``ClientSession(...)`` **constructor** args/kwargs (``auth=``,
    ``timeout=``, ``headers=``, ...) captured by :meth:`as_factory` are recorded
    on ``.session_args`` / ``.session_kwargs`` (the most recent construction) and
    appended to ``.constructions`` (one ``(args, kwargs)`` tuple per call), so
    tests can assert the HTTP transport contract (auth, timeout, ...).
    """

    def __init__(
        self,
        routes: Iterable[_Route] | None = None,
        *,
        default: FakeHTTPResponse | None = None,
    ) -> None:
        self.routes: list[_Route] = list(routes or [])
        self.default = default
        self.requests: list[tuple[str, dict[str, Any]]] = []
        self.entered = 0
        self.closed = False
        # Constructor args/kwargs captured by ``as_factory`` (see class docstring).
        self.session_args: tuple[Any, ...] = ()
        self.session_kwargs: dict[str, Any] = {}
        self.constructions: list[tuple[tuple[Any, ...], dict[str, Any]]] = []

    def add_route(
        self,
        matcher: Any,
        response: FakeHTTPResponse | Callable[..., FakeHTTPResponse],
    ) -> FakeAiohttpSession:
        self.routes.append((matcher, response))
        return self

    def _match(self, url: str) -> FakeHTTPResponse | Callable[..., FakeHTTPResponse]:
        for matcher, response in self.routes:
            if isinstance(matcher, str):
                if matcher in url:
                    return response
            elif callable(matcher) and matcher(url):
                return response
        if self.default is not None:
            return self.default
        raise AssertionError(f"FakeAiohttpSession: no route for URL {url!r}")

    def get(self, url: str, **kwargs: Any) -> FakeHTTPResponse:
        self.requests.append((url, kwargs))
        response = self._match(url)
        if not isinstance(response, FakeHTTPResponse) and callable(response):
            response = response(url, **kwargs)
        return response

    async def __aenter__(self) -> FakeAiohttpSession:
        self.entered += 1
        return self

    async def __aexit__(self, *exc: object) -> bool:
        self.closed = True
        return False

    def as_factory(self) -> Callable[..., FakeAiohttpSession]:
        """Return a callable suitable for patching ``aiohttp.ClientSession``.

        The callable **records** the constructor args (``auth=``, ``timeout=``,
        ``headers=``, ...) on the session and returns *this* session, so the same
        scripted routes serve every ``async with ClientSession(...) as session``
        block in the call under test while tests can still assert what the caller
        passed to the constructor.
        """
        session = self

        def factory(*args: Any, **kwargs: Any) -> FakeAiohttpSession:
            session.session_args = args
            session.session_kwargs = kwargs
            session.constructions.append((args, kwargs))
            return session

        return factory
