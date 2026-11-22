"""WebSocket JSON API client of the zigbee-rs hub (device-support spec 14).

One connection: `auth_required` → `auth` → `auth_ok`, then requests with
client-chosen ids answered by `result` frames, and `event` frames for
subscriptions (the first one is the snapshot).
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable
import contextlib
import json
import logging
from typing import Any

import aiohttp

from .const import KEEPALIVE, REQUEST_TIMEOUT

_LOGGER = logging.getLogger(__name__)

type EventCallback = Callable[[dict[str, Any], int], None]


class HubError(Exception):
    """Base error."""


class CannotConnect(HubError):
    """The hub cannot be reached or closed the connection."""


class InvalidAuth(HubError):
    """The hub refused the token."""


class HubApiError(HubError):
    """A request failed on the hub (`success: false`)."""

    def __init__(self, code: str, message: str) -> None:
        """Error with the hub's code and message."""
        super().__init__(f"{code}: {message}" if message else code)
        self.code = code
        self.message = message


def hub_url(host: str, port: int, path: str) -> str:
    """WebSocket URL of the API."""
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    if not path.startswith("/"):
        path = "/" + path
    return f"ws://{host}:{port}{path}"


class HubClient:
    """One authenticated WebSocket connection."""

    def __init__(
        self,
        session: aiohttp.ClientSession,
        url: str,
        token: str,
        on_close: Callable[[], None] | None = None,
    ) -> None:
        """Client for `url`; `on_close` runs once the connection is gone."""
        self._session = session
        self.url = url
        self._token = token
        self._on_close = on_close
        self._ws: aiohttp.ClientWebSocketResponse | None = None
        self._reader: asyncio.Task[None] | None = None
        self._keepalive: asyncio.Task[None] | None = None
        self._keepalive_closing = False
        self._next_id = 1
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._subscriptions: dict[int, EventCallback] = {}
        # Other unsolicited messages (`ota/block_needed`).
        self.on_message: Callable[[dict[str, Any]], None] | None = None
        self.hello: dict[str, Any] = {}
        self.closed = asyncio.Event()

    @property
    def connected(self) -> bool:
        """Whether the socket is open."""
        return self._ws is not None and not self._ws.closed and not self.closed.is_set()

    async def connect(self) -> dict[str, Any]:
        """Open the socket and authenticate; returns the `auth_ok` frame."""
        try:
            self._ws = await self._session.ws_connect(
                self.url,
                timeout=aiohttp.ClientWSTimeout(ws_close=REQUEST_TIMEOUT),
                max_msg_size=0,
            )
        except (aiohttp.ClientError, TimeoutError, OSError) as err:
            raise CannotConnect(str(err)) from err
        try:
            async with asyncio.timeout(REQUEST_TIMEOUT):
                first = await self._receive()
                if first.get("type") != "auth_required":
                    raise CannotConnect(f"unexpected first frame {first.get('type')}")
                await self._ws.send_str(json.dumps({"type": "auth", "token": self._token}))
                reply = await self._receive()
            if reply.get("type") == "auth_invalid":
                raise InvalidAuth(reply.get("message", "unauthorized"))
            if reply.get("type") != "auth_ok":
                raise CannotConnect(f"unexpected auth reply {reply.get('type')}")
        except TimeoutError as err:
            await self._abort()
            raise CannotConnect("no answer to auth") from err
        except BaseException:
            # Also on cancellation (unload during a reconnect): no open socket left.
            await self._abort()
            raise
        self.hello = reply
        self._reader = asyncio.create_task(self._read_loop(), name="zigbee_rs_hub reader")
        self._keepalive = asyncio.create_task(
            self._keepalive_loop(), name="zigbee_rs_hub keepalive"
        )
        return reply

    async def _keepalive_loop(self) -> None:
        """A request now and then: a hub that stopped answering is dropped.

        An application-level check rather than WebSocket pings, so it does
        not depend on how the hub's HTTP server treats control frames.
        """
        while self.connected:
            await asyncio.sleep(KEEPALIVE)
            try:
                await self.request("hub/info", timeout=REQUEST_TIMEOUT)
            except HubApiError as err:
                if err.code != "timeout":
                    continue  # it answered
                _LOGGER.warning("The hub at %s stopped answering", self.url)
            except CannotConnect:
                pass
            else:
                continue
            if self._ws is not None:
                # Not cancelled by `_finish` half-way through the close.
                self._keepalive_closing = True
                await self._ws.close()
            return

    async def _receive(self) -> dict[str, Any]:
        assert self._ws is not None
        msg = await self._ws.receive()
        if msg.type in (
            aiohttp.WSMsgType.CLOSE,
            aiohttp.WSMsgType.CLOSED,
            aiohttp.WSMsgType.CLOSING,
        ):
            raise CannotConnect("connection closed")
        if msg.type == aiohttp.WSMsgType.ERROR:
            raise CannotConnect(str(self._ws.exception()))
        if msg.type != aiohttp.WSMsgType.TEXT:
            raise CannotConnect(f"unexpected frame type {msg.type}")
        try:
            data = json.loads(msg.data)
        except ValueError as err:
            raise CannotConnect(f"invalid JSON: {err}") from err
        if not isinstance(data, dict):
            raise CannotConnect("frame is not an object")
        return data

    async def _abort(self) -> None:
        if self._ws is not None:
            with contextlib.suppress(Exception):
                await self._ws.close()
        self.closed.set()

    async def _read_loop(self) -> None:
        assert self._ws is not None
        try:
            async for msg in self._ws:
                if msg.type != aiohttp.WSMsgType.TEXT:
                    if msg.type == aiohttp.WSMsgType.ERROR:
                        _LOGGER.debug("WebSocket error: %s", self._ws.exception())
                        break
                    continue
                try:
                    frame = json.loads(msg.data)
                except ValueError:
                    _LOGGER.warning("Ignoring a frame that is not JSON: %.200s", msg.data)
                    continue
                if isinstance(frame, dict):
                    self._dispatch(frame)
        except Exception:
            _LOGGER.exception("WebSocket reader failed")
        finally:
            self._finish()

    def _dispatch(self, frame: dict[str, Any]) -> None:
        kind = frame.get("type")
        if kind == "result":
            fut = self._pending.pop(frame.get("id"), None)
            if fut is None or fut.done():
                return
            if frame.get("success"):
                fut.set_result(frame.get("result"))
            else:
                err = frame.get("error") or {}
                fut.set_exception(
                    HubApiError(str(err.get("code", "error")), str(err.get("message", "")))
                )
        elif kind == "event":
            cb = self._subscriptions.get(frame.get("id"))
            event = frame.get("event")
            if cb is not None and isinstance(event, dict):
                try:
                    cb(event, int(frame.get("seq", 0)))
                except Exception:
                    _LOGGER.exception("Error handling hub event %s", event.get("topic"))
        elif self.on_message is not None:
            try:
                self.on_message(frame)
            except Exception:
                _LOGGER.exception("Error handling hub message %s", kind)

    def _finish(self) -> None:
        self.closed.set()
        if (
            self._keepalive is not None
            and self._keepalive is not asyncio.current_task()
            and not self._keepalive_closing
        ):
            self._keepalive.cancel()
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(CannotConnect("connection closed"))
        self._pending.clear()
        if self._on_close is not None:
            cb, self._on_close = self._on_close, None
            cb()

    async def close(self) -> None:
        """Close the connection (does not call `on_close`)."""
        self._on_close = None
        if self._ws is not None:
            with contextlib.suppress(Exception):
                await self._ws.close()
        if self._reader is not None:
            self._reader.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await self._reader
        self._finish()
        if self._keepalive is not None:
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await self._keepalive

    async def request(self, req_type: str, timeout: float = REQUEST_TIMEOUT, **params: Any) -> Any:
        """Send one request and wait for its result."""
        rid = self._next_id
        self._next_id += 1
        return await self._call(rid, req_type, timeout, params)

    async def subscribe(self, topics: list[str], callback: EventCallback) -> int:
        """Subscribe; `callback` gets the snapshot, then the deltas."""
        rid = self._next_id
        self._next_id += 1
        # Registered before sending: the snapshot follows the result at once.
        self._subscriptions[rid] = callback
        try:
            await self._call(rid, "subscribe", REQUEST_TIMEOUT, {"topics": topics})
        except HubError:
            self._subscriptions.pop(rid, None)
            raise
        return rid

    async def _call(self, rid: int, req_type: str, timeout: float, params: dict[str, Any]) -> Any:
        if not self.connected or self._ws is None:
            raise CannotConnect("not connected")
        fut: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._pending[rid] = fut
        try:
            await self._ws.send_str(json.dumps({"id": rid, "type": req_type, **params}))
            async with asyncio.timeout(timeout):
                return await fut
        except TimeoutError as err:
            raise HubApiError("timeout", f"no answer to {req_type}") from err
        except (aiohttp.ClientError, ConnectionError, RuntimeError) as err:
            raise CannotConnect(str(err)) from err
        finally:
            self._pending.pop(rid, None)
