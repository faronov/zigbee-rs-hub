"""Definitions catalog client (device-support 9.3, 9.4, 14.4).

The hub downloads nothing itself: this fetches the signed release files from
the catalog mirror (the hub's `catalog/config` `base_url`, GitHub Pages by
default) and uploads them through the API, as `hubctl catalog-push` does:

1. `manifest.cose` → target `manifest`; the commit's `wants` names the bank
   for the hub's slot size (if newer) and the web tools (if they differ);
2. those files → targets `bank` / `web`;
3. `catalog/status` `missing` (devices that resolve to generic only, with
   the shards holding their keys) → each `shard-hh.cbor` → target `shard`.

Uploads go in 4 KiB chunks (the most the hub accepts); after a dropped
connection they continue where `catalog/status` `upload.written` says. The
hub verifies every file against the signed manifest.
"""

from __future__ import annotations

import asyncio
import base64
from collections.abc import Callable, Iterable
from datetime import datetime
import hashlib
import logging
import re
import time
from typing import TYPE_CHECKING, Any

import aiohttp

from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.event import async_call_later, async_track_time_interval
from homeassistant.util import dt as dt_util

from . import const
from .api import CannotConnect, HubApiError, HubClient, HubError
from .const import (
    CATALOG_CHUNK,
    CATALOG_CHUNK_TIMEOUT,
    CATALOG_COMMIT_TIMEOUT,
    CATALOG_DEFAULT_BASE,
    CATALOG_DOWNLOAD_TIMEOUT,
    CATALOG_MAX_FILE,
    CATALOG_RESUMES,
    DOMAIN,
)

if TYPE_CHECKING:
    from .hub import ZigbeeRsHub

_LOGGER = logging.getLogger(__name__)

# Release file names the hub may ask for (from the signed manifest).
_ASSET_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_STATUS_EVENTS = frozenset(
    {"catalog_manifest", "catalog_updated", "web_tools_updated", "definition_fetched"}
)


class CatalogError(HomeAssistantError):
    """The release files could not be fetched or do not fit the manifest."""


def _shards(values: Any) -> set[int]:
    if not isinstance(values, list):
        return set()
    return {n for n in values if isinstance(n, int) and not isinstance(n, bool) and 0 <= n < 256}


def _asset_digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class CatalogUpdater:
    """Keeps one hub's definitions catalog current."""

    def __init__(
        self, hass: HomeAssistant, hub: ZigbeeRsHub, session: aiohttp.ClientSession
    ) -> None:
        """Updater of `hub`; downloads through `session`."""
        self.hass = hass
        self.hub = hub
        self._session = session
        # Last `catalog/status`; `None` while unknown.
        self.status: dict[str, Any] | None = None
        # `False` once the hub answered `catalog/status` with an error.
        self.supported: bool | None = None
        self.last_check: datetime | None = None
        self.last_error: str | None = None
        self.last_result: dict[str, Any] = {}
        self._lock = asyncio.Lock()
        self._listeners: list[Callable[[], None]] = []
        # Device → monotonic time of the last fetch attempt.
        self._fetch_attempts: dict[str, float] = {}
        # `definition_needed` not handled yet: device → shards.
        self._pending: dict[str, set[int]] = {}
        self._fetch_task: asyncio.Task[None] | None = None
        self._refresh_task: asyncio.Task[None] | None = None

    # Settings the hub stores (`catalog/config`).

    @property
    def config(self) -> dict[str, Any]:
        """`catalog/status` `config`."""
        cfg = (self.status or {}).get("config")
        return cfg if isinstance(cfg, dict) else {}

    @property
    def base_url(self) -> str:
        """Mirror of the releases."""
        url = self.config.get("base_url")
        return (url if isinstance(url, str) and url else CATALOG_DEFAULT_BASE).rstrip("/")

    @property
    def auto_update(self) -> bool:
        """Check daily and install newer catalogs (hub default on)."""
        return self.config.get("auto_update", True) is not False

    @property
    def auto_fetch(self) -> bool:
        """Fetch definitions of devices that resolve to generic only."""
        return self.config.get("auto_fetch", True) is not False

    @property
    def installed_version(self) -> str | None:
        """`catalog_date.serial` of the active bank."""
        inst = (self.status or {}).get("installed")
        if not isinstance(inst, dict) or inst.get("catalog_date") is None:
            return None
        return f"{inst['catalog_date']}.{inst.get('catalog_serial', 0)}"

    # Listeners (the sensor).

    @callback
    def add_listener(self, cb: Callable[[], None]) -> Callable[[], None]:
        """`cb` runs when the status or the last run's outcome changes."""
        self._listeners.append(cb)
        return lambda: self._listeners.remove(cb)

    @callback
    def _notify(self) -> None:
        for cb in list(self._listeners):
            cb()

    # Life cycle.

    async def async_connected(self, client: HubClient) -> None:
        """A (re)connection: read the status, follow the `catalog` topic."""
        try:
            status = await client.request("catalog/status")
        except HubApiError as err:
            if self.supported is not False:
                _LOGGER.debug("No catalog on the hub at %s: %s", self.hub.url, err)
            self.supported = False
            self.status = None
            return
        self.supported = isinstance(status, dict)
        self.status = status if isinstance(status, dict) else None
        if not self.supported:
            return
        try:
            await client.subscribe(["catalog"], self._on_event)
        except HubApiError as err:
            _LOGGER.debug("Cannot subscribe to the catalog topic: %s", err)
        self._notify()

    @callback
    def async_start(self) -> list[Callable[[], None]]:
        """The first check soon, then a daily one; returns the cancellers."""
        return [
            async_call_later(self.hass, const.CATALOG_FIRST_CHECK, self._scheduled),
            async_track_time_interval(self.hass, self._scheduled, const.CATALOG_INTERVAL),
        ]

    @callback
    def _scheduled(self, _now: datetime) -> None:
        self.hub.entry.async_create_background_task(
            self.hass, self.async_auto_check(), f"{DOMAIN} catalog check"
        )

    async def async_auto_check(self) -> None:
        """Install a newer catalog (`auto_update`) and missing definitions (`auto_fetch`)."""
        if not self.supported or not self.hub.connected:
            return
        try:
            await self.async_refresh()
        except HubError as err:
            _LOGGER.debug("catalog/status failed: %s", err)
            return
        if not self.auto_update and not self.auto_fetch:
            return
        try:
            await self.async_update(update=self.auto_update, fetch=self.auto_fetch)
        except HomeAssistantError as err:
            _LOGGER.warning("Catalog update of the hub at %s failed: %s", self.hub.url, err)

    # Status.

    async def async_refresh(self) -> dict[str, Any] | None:
        """Read `catalog/status` again."""
        st = await self._request("catalog/status")
        self.status = st if isinstance(st, dict) else None
        self._notify()
        return self.status

    @callback
    def _schedule_refresh(self) -> None:
        if self._lock.locked() or (self._refresh_task and not self._refresh_task.done()):
            return
        self._refresh_task = self.hub.entry.async_create_background_task(
            self.hass, self._quiet_refresh(), f"{DOMAIN} catalog status"
        )

    async def _quiet_refresh(self) -> None:
        try:
            await self.async_refresh()
        except HubError as err:
            _LOGGER.debug("catalog/status failed: %s", err)

    # Events of the `catalog` topic.

    @callback
    def _on_event(self, event: dict[str, Any], _seq: int) -> None:
        topic = event.get("topic")
        if topic == "definition_needed":
            self._definition_needed(event)
        elif topic in _STATUS_EVENTS:
            self._schedule_refresh()

    @callback
    def _definition_needed(self, event: dict[str, Any]) -> None:
        device = event.get("device")
        shards = _shards(event.get("shards"))
        if not isinstance(device, str) or not shards:
            return
        last = self._fetch_attempts.get(device)
        if last is not None and time.monotonic() - last < const.CATALOG_FETCH_RETRY:
            _LOGGER.debug("Definition of %s fetched less than an hour ago", device)
            return
        self._pending.setdefault(device, set()).update(shards)
        if self._fetch_task is None or self._fetch_task.done():
            self._fetch_task = self.hub.entry.async_create_background_task(
                self.hass, self._fetch_pending(), f"{DOMAIN} definition fetch"
            )

    async def _fetch_pending(self) -> None:
        # Devices announced together (after a manifest) share one batch.
        await asyncio.sleep(const.CATALOG_FETCH_DEBOUNCE)
        async with self._lock:
            pending, self._pending = self._pending, {}
            try:
                # The setting may have changed on the hub (web UI) since.
                await self.async_refresh()
                if not self.auto_fetch:
                    return
                now = time.monotonic()
                for device in pending:
                    self._fetch_attempts[device] = now
                await self._fetch_shards(self.base_url, set().union(*pending.values()))
                self.last_error = None
            except (HubError, CatalogError) as err:
                self.last_error = str(err)
                _LOGGER.warning(
                    "Fetching definitions for the hub at %s failed: %s", self.hub.url, err
                )
            finally:
                self.last_check = dt_util.utcnow()
                await self._refresh_after()

    # The flow.

    async def async_update(
        self,
        force: bool = False,
        update: bool = True,
        fetch: bool = True,
        base_url: str | None = None,
    ) -> dict[str, Any]:
        """Run the catalog flow now.

        `update`: a newer manifest, bank and web tools; `fetch`: the
        definitions of devices that resolve to generic only. `force` passes
        the hub's anti-rollback check of a bank; `base_url` replaces the
        hub's mirror for this run. Raises `HomeAssistantError` when
        something failed.
        """
        if not self.supported:
            raise HomeAssistantError(f"The hub at {self.hub.url} has no definitions catalog")
        async with self._lock:
            result: dict[str, Any] = {}
            try:
                st = await self.async_refresh() or {}
                base = base_url.rstrip("/") if base_url else self.base_url
                if update:
                    result.update(await self._update(base, st, force))
                if fetch:
                    st = await self.async_refresh() or {}
                    missing = st.get("missing")
                    shards: set[int] = set()
                    now = time.monotonic()
                    for item in missing if isinstance(missing, list) else []:
                        if isinstance(item, dict) and isinstance(item.get("device"), str):
                            self._fetch_attempts[item["device"]] = now
                            shards |= _shards(item.get("shards"))
                    result["shards"] = await self._fetch_shards(base, shards)
            except HubError as err:
                self.last_error = str(err)
                raise HomeAssistantError(f"Catalog update failed: {err}") from err
            except CatalogError as err:
                self.last_error = str(err)
                raise
            else:
                self.last_error = None
                self.last_result = result
            finally:
                self.last_check = dt_util.utcnow()
                await self._refresh_after()
            return result

    async def _refresh_after(self) -> None:
        try:
            await self.async_refresh()
        except HubError:
            self._notify()

    async def _update(self, base: str, st: dict[str, Any], force: bool) -> dict[str, Any]:
        result: dict[str, Any] = {}
        # An upload a dropped connection interrupted (also in an earlier run):
        # continue it before the manifest upload replaces it.
        up = st.get("upload")
        wants = st.get("wants") if isinstance(st.get("wants"), dict) else {}
        if isinstance(up, dict) and up.get("target") in ("bank", "web"):
            target = up["target"]
            if isinstance(asset := wants.get(target), dict):
                written = up.get("written")
                result[target] = await self._install_asset(
                    base, target, asset, force, written if isinstance(written, int) else 0
                )
        manifest = await self._download(base, "manifest.cose")
        m = await self._upload("manifest", manifest, {})
        m = m if isinstance(m, dict) else {}
        result["manifest"] = {k: m.get(k) for k in ("version", "catalog_date", "catalog_serial")}
        _LOGGER.debug("Catalog manifest %s installed on %s", m.get("version"), self.hub.url)
        wants = m.get("wants") if isinstance(m.get("wants"), dict) else {}
        for target in ("bank", "web"):
            if isinstance(asset := wants.get(target), dict):
                result[target] = await self._install_asset(base, target, asset, force)
        return result

    async def _install_asset(
        self, base: str, target: str, asset: dict[str, Any], force: bool, start: int = 0
    ) -> Any:
        name = asset.get("name")
        if not isinstance(name, str) or not _ASSET_NAME.match(name):
            raise CatalogError(f"The hub wants an invalid file name {name!r}")
        data = await self._download(base, name)
        size, sha = asset.get("size"), asset.get("sha256")
        if isinstance(size, int) and size != len(data):
            raise CatalogError(f"{name}: {len(data)} bytes, the manifest lists {size}")
        if isinstance(sha, str):
            digest = await self.hass.async_add_executor_job(_asset_digest, data)
            if digest != sha.lower():
                raise CatalogError(f"{name}: SHA-256 differs from the manifest")
        commit: dict[str, Any] = {"force": True} if force and target == "bank" else {}
        start = start if 0 <= start <= len(data) else 0
        result = await self._upload(
            target, data, commit, start, sha if isinstance(sha, str) else None
        )
        _LOGGER.info("%s (%d bytes) installed on the hub at %s", name, len(data), self.hub.url)
        return result

    async def _fetch_shards(self, base: str, shards: Iterable[int]) -> dict[str, Any]:
        results: dict[str, Any] = {}
        errors: list[str] = []
        for n in sorted(shards):
            name = f"shard-{n:02x}.cbor"
            try:
                data = await self._download(base, name)
                r = await self._upload("shard", data, {"shard": n})
            except HubApiError as err:
                # Not in the manifest, or a hash mismatch: the other shards go on.
                errors.append(f"{name}: {err}")
                continue
            except CatalogError as err:
                errors.append(str(err))
                continue
            results[f"{n:02x}"] = r
            stored = r.get("definitions") if isinstance(r, dict) else None
            _LOGGER.info("Shard %02x on the hub at %s: definitions %s", n, self.hub.url, stored)
        if errors:
            raise CatalogError("; ".join(errors))
        return results

    # Transport.

    async def _request(self, req_type: str, timeout: float | None = None, **params: Any) -> Any:
        client = self.hub.client
        if client is None or not self.hub.connected:
            raise CannotConnect("not connected")
        if timeout is None:
            return await client.request(req_type, **params)
        return await client.request(req_type, timeout=timeout, **params)

    async def _download(self, base: str, name: str) -> bytes:
        url = f"{base}/{name}"
        try:
            async with self._session.get(
                url, timeout=aiohttp.ClientTimeout(total=CATALOG_DOWNLOAD_TIMEOUT)
            ) as resp:
                resp.raise_for_status()
                if (resp.content_length or 0) > CATALOG_MAX_FILE:
                    raise CatalogError(f"{url}: larger than {CATALOG_MAX_FILE} bytes")
                data = bytearray()
                async for chunk in resp.content.iter_chunked(64 * 1024):
                    data += chunk
                    if len(data) > CATALOG_MAX_FILE:
                        raise CatalogError(f"{url}: larger than {CATALOG_MAX_FILE} bytes")
                return bytes(data)
        except (aiohttp.ClientError, TimeoutError) as err:
            raise CatalogError(f"{url}: {err or type(err).__name__}") from err

    async def _upload(
        self,
        target: str,
        data: bytes,
        commit: dict[str, Any],
        start: int = 0,
        sha256: str | None = None,
    ) -> Any:
        """Chunks from `start`, then the commit; resumes after a dropped connection."""
        offset = start
        resumes = 0
        while True:
            try:
                while offset == 0 or offset < len(data):
                    chunk = data[offset : offset + CATALOG_CHUNK]
                    params: dict[str, Any] = {
                        "target": target,
                        "offset": offset,
                        "data": base64.b64encode(chunk).decode(),
                    }
                    if offset == 0:
                        params["size"] = len(data)
                        if sha256:
                            params["sha256"] = sha256
                    r = await self._request(
                        "catalog/upload_chunk", timeout=CATALOG_CHUNK_TIMEOUT, **params
                    )
                    written = r.get("written") if isinstance(r, dict) else None
                    offset = written if isinstance(written, int) else offset + len(chunk)
                    if not chunk:
                        break
                return await self._request(
                    "catalog/commit", timeout=CATALOG_COMMIT_TIMEOUT, target=target, **commit
                )
            except CannotConnect:
                resumes += 1
                if resumes > CATALOG_RESUMES or self.hub.stopping:
                    raise
                _LOGGER.info("Connection lost during the %s upload; waiting to resume", target)
                await self._wait_connected()
                st = await self.async_refresh() or {}
                up = st.get("upload")
                written = up.get("written") if isinstance(up, dict) else None
                if (
                    isinstance(up, dict)
                    and up.get("target") == target
                    and isinstance(written, int)
                    and 0 < written <= len(data)
                ):
                    offset = written
                else:
                    offset = 0
                _LOGGER.debug("Resuming the %s upload at %d", target, offset)

    async def _wait_connected(self) -> None:
        if self.hub.connected:
            return
        connected = asyncio.Event()

        @callback
        def on_change() -> None:
            if self.hub.connected:
                connected.set()

        remove = self.hub.add_hub_listener(on_change)
        try:
            async with asyncio.timeout(const.CATALOG_RECONNECT_WAIT):
                await connected.wait()
        except TimeoutError as err:
            raise CannotConnect("the hub did not come back") from err
        finally:
            remove()

    # Diagnostics.

    def as_dict(self) -> dict[str, Any]:
        """State for the diagnostics."""
        return {
            "supported": self.supported,
            "status": self.status,
            "last_check": self.last_check.isoformat() if self.last_check else None,
            "last_error": self.last_error,
            "last_result": self.last_result,
            "pending": {d: sorted(s) for d, s in self._pending.items()},
        }
