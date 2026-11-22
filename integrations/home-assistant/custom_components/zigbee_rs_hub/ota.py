"""OTA images for the hub's devices from a provider index.

The hub downloads nothing (no TLS on the hub): Home Assistant fetches the
index the hub names (`ota/status` `settings.index_url`, by default
Koenkk/zigbee-OTA) and, when an update is installed from it, the image;
it checks the SHA-512 the index gives, cuts a vendor prefix before the OTA
header and uploads the file to the hub (`ota/upload/*`, device-support
14.7).
"""

from __future__ import annotations

import base64
import contextlib
import hashlib
import logging
import os
import time
from typing import TYPE_CHECKING, Any

import aiohttp

from homeassistant.exceptions import HomeAssistantError

if TYPE_CHECKING:
    from .hub import ZigbeeRsHub

_LOGGER = logging.getLogger(__name__)

INDEX_TTL = 24 * 3600.0
FETCH_TIMEOUT = aiohttp.ClientTimeout(total=120)
# The Zigbee OTA upgrade file identifier 0x0BEEF11E, little endian.
OTA_MAGIC = b"\x1e\xf1\xee\x0b"
UPLOAD_TIMEOUT = 30.0


def _int(value: Any) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def applicable(entry: dict[str, Any], rec: dict[str, Any]) -> bool:
    """Whether an index entry updates the device of an `ota/devices/list` record."""
    cur = _int(rec.get("current_version"))
    if cur is None or rec.get("manufacturer_code") is None:
        return False
    hw = _int(rec.get("hw_version"))
    lo, hi = _int(entry.get("minFileVersion")), _int(entry.get("maxFileVersion"))
    hw_lo, hw_hi = _int(entry.get("hardwareVersionMin")), _int(entry.get("hardwareVersionMax"))
    version = _int(entry.get("fileVersion"))
    return (
        entry.get("manufacturerCode") == rec.get("manufacturer_code")
        and entry.get("imageType") == rec.get("image_type")
        and version is not None
        and version > cur
        and (lo is None or cur >= lo)
        and (hi is None or cur <= hi)
        and (hw is None or ((hw_lo is None or hw >= hw_lo) and (hw_hi is None or hw <= hw_hi)))
        and isinstance(entry.get("url"), str)
    )


class OtaIndex:
    """The provider index, fetched at most once a day."""

    def __init__(self, session: aiohttp.ClientSession) -> None:
        """An empty index."""
        self.session = session
        self.entries: list[dict[str, Any]] = []
        self.url: str | None = None
        self._fetched = 0.0

    async def refresh(self, url: str | None, force: bool = False) -> bool:
        """Fetch `url` when due; whether the entries changed."""
        if not url:
            changed = bool(self.entries)
            self.entries, self.url = [], None
            return changed
        if not force and url == self.url and time.monotonic() - self._fetched < INDEX_TTL:
            return False
        try:
            async with self.session.get(url, timeout=FETCH_TIMEOUT) as resp:
                resp.raise_for_status()
                data = await resp.json(content_type=None)
        except (aiohttp.ClientError, TimeoutError, ValueError) as err:
            _LOGGER.warning("OTA index %s: %s", url, err)
            return False
        entries = [e for e in data if isinstance(e, dict)] if isinstance(data, list) else []
        self._fetched = time.monotonic()
        changed = entries != self.entries or url != self.url
        self.entries, self.url = entries, url
        _LOGGER.debug("OTA index %s: %d images", url, len(entries))
        return changed

    def best(self, rec: dict[str, Any]) -> dict[str, Any] | None:
        """The newest index entry for the device of `rec`."""
        found = [e for e in self.entries if applicable(e, rec)]
        return max(found, key=lambda e: e["fileVersion"]) if found else None


async def fetch_image(session: aiohttp.ClientSession, entry: dict[str, Any]) -> bytes:
    """Download an index entry's file, check its SHA-512, cut a prefix."""
    try:
        async with session.get(entry["url"], timeout=FETCH_TIMEOUT) as resp:
            resp.raise_for_status()
            data = await resp.read()
    except (aiohttp.ClientError, TimeoutError) as err:
        raise HomeAssistantError(f"download of {entry['url']} failed: {err}") from err
    want = entry.get("sha512")
    if isinstance(want, str) and hashlib.sha512(data).hexdigest() != want.lower():
        raise HomeAssistantError(f"{entry['url']}: SHA-512 differs from the index")
    at = data.find(OTA_MAGIC, 0, 4096)
    if at < 0:
        raise HomeAssistantError(f"{entry['url']}: not a Zigbee OTA file")
    return data[at:]


async def upload(hub: ZigbeeRsHub, data: bytes, name: str, meta: dict[str, Any]) -> dict[str, Any]:
    """Upload an OTA file to the hub; the stored image's record."""
    begin = await hub.async_request(
        "ota/upload/begin",
        timeout=UPLOAD_TIMEOUT,
        size=len(data),
        name=name[:48],
        sha256=hashlib.sha256(data).hexdigest(),
        **{k: v for k, v in meta.items() if v is not None},
    )
    upload_id, offset = begin["upload"], int(begin.get("offset") or 0)
    chunk = int(begin.get("chunk_max") or 4096)
    while offset < len(data):
        end = min(offset + chunk, len(data))
        await hub.async_request(
            "ota/upload/chunk",
            timeout=UPLOAD_TIMEOUT,
            upload=upload_id,
            offset=offset,
            data=base64.b64encode(data[offset:end]).decode(),
        )
        offset = end
    return await hub.async_request("ota/upload/commit", timeout=UPLOAD_TIMEOUT, upload=upload_id)


class OtaStreams:
    """Images Home Assistant serves to a hub without an image store.

    The 4 MB layout: `ota/stream/offer`, then `ota/block_data` for every
    `ota/block_needed`. Files are kept on disk too, so a download goes on
    after Home Assistant or the hub restarted (the hub keeps the session
    two minutes; later the upgrade is started again).
    """

    def __init__(self, hub: ZigbeeRsHub, cache_dir: str) -> None:
        """Streams of `hub`, files cached in `cache_dir`."""
        self.hub = hub
        self.cache_dir = cache_dir
        # Session → file.
        self.sessions: dict[int, bytes] = {}

    @staticmethod
    def key(manufacturer: int, image_type: int, version: int) -> str:
        """Cache file name of an image."""
        return f"{manufacturer:04x}-{image_type:04x}-{version:08x}.ota"

    def _path(self, name: str) -> str:
        return os.path.join(self.cache_dir, name)

    def _save(self, name: str, data: bytes) -> None:
        os.makedirs(self.cache_dir, exist_ok=True)
        with open(self._path(name), "wb") as f:
            f.write(data)

    def _load(self, name: str) -> bytes | None:
        try:
            with open(self._path(name), "rb") as f:
                return f.read()
        except OSError:
            return None

    async def offer(self, data: bytes, name: str, meta: dict[str, Any]) -> int:
        """Offer `data` (an OTA file) as a stream; its session ID."""
        res = await self.hub.async_request(
            "ota/stream/offer",
            size=len(data),
            name=name[:48],
            header=base64.b64encode(data[:256]).decode(),
            **{k: v for k, v in meta.items() if v is not None},
        )
        session = int(res["session"])
        self.sessions[session] = data
        key = self.key(res["manufacturer_code"], res["image_type"], res["file_version"])
        await self.hub.hass.async_add_executor_job(self._save, key, data)
        return session

    def block_needed(self, msg: dict[str, Any]) -> None:
        """Answer one `ota/block_needed`."""
        data = self.sessions.get(msg.get("session"))  # type: ignore[arg-type]
        if data is None:
            return
        off, length = int(msg.get("offset", 0)), int(msg.get("len", 0))
        chunk = base64.b64encode(data[off : off + length]).decode()
        self.hub.entry.async_create_background_task(
            self.hub.hass,
            self.hub.async_request(
                "ota/block_data",
                timeout=UPLOAD_TIMEOUT,
                session=msg["session"],
                offset=off,
                data=chunk,
            ),
            f"OTA block {off}",
        )

    async def resume(self) -> None:
        """Offer the cached files of running jobs again after a (re)connect.

        Jobs whose session is gone are started again.
        """
        for rec in list(self.hub.ota.values()):
            job = rec.get("job") or {}
            if (
                job.get("state") not in ("waiting", "downloading")
                or job.get("file_version") is None
            ):
                continue
            if rec.get("manufacturer_code") is None:
                continue
            name = self.key(rec["manufacturer_code"], rec["image_type"], job["file_version"])
            data = await self.hub.hass.async_add_executor_job(self._load, name)
            if data is None:
                continue
            try:
                session = await self.offer(data, name, {})
                if job.get("image") != session:
                    await self.hub.async_request(
                        "ota/start", device=rec["device"], image=session, force=True
                    )
            except HomeAssistantError as err:
                _LOGGER.warning("OTA stream for %s not resumed: %s", rec["device"], err)

    async def finished(self, rec: dict[str, Any]) -> None:
        """A job ended: close its stream, drop the cached file."""
        job = rec.get("job") or {}
        session = job.get("image")
        if session not in self.sessions or job.get("state") not in ("done", "failed", "aborted"):
            return
        del self.sessions[session]
        with contextlib.suppress(HomeAssistantError):
            await self.hub.async_request("ota/stream/close", session=session)
        if job.get("state") == "done" and rec.get("manufacturer_code") is not None:
            name = self.key(rec["manufacturer_code"], rec["image_type"], job["file_version"])
            with contextlib.suppress(OSError):
                await self.hub.hass.async_add_executor_job(os.remove, self._path(name))
