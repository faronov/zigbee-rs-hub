"""Firmware of the hub's devices: one `update` entity per device with an OTA client.

The hub is the Zigbee OTA Upgrade server (cluster 0x0019, API `ota/*`):
`ota/devices/list` and the `ota` topic give each device's running file
version, the newest stored image it qualifies for and the running job;
installing calls `ota/start`. The hub downloads nothing itself: a newer
image in the provider index the hub names (Koenkk/zigbee-OTA by default)
is downloaded by Home Assistant at install time and checked; a hub with
an image store (8 MB) gets it uploaded, a hub without one (4 MB) gets it
streamed: Home Assistant serves the blocks while the device downloads.
"""

from __future__ import annotations

from datetime import timedelta
from typing import Any

from homeassistant.components.update import (
    UpdateDeviceClass,
    UpdateEntity,
    UpdateEntityFeature,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.event import async_call_later, async_track_time_interval

from . import ZigbeeRsHubConfigEntry
from .const import COMMAND_TIMEOUT, DOMAIN
from .hub import ZigbeeRsHub
from .ota import fetch_image, upload

# Job states of a running upgrade (device-support 14.7).
RUNNING = frozenset({"waiting", "queued", "downloading", "installing"})
INDEX_REFRESH = timedelta(hours=24)
# The first index fetch waits a minute after set-up (not on HA's start path).
INDEX_FIRST = 60.0


def file_version(value: Any) -> str | None:
    """An OTA file version as ZHA shows it (`0x01020304`)."""
    if not isinstance(value, int) or isinstance(value, bool):
        return None
    return f"0x{value:08x}"


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Update entities, now and whenever the hub reports another OTA device."""
    hub = entry.runtime_data

    @callback
    def add(device_ids: list[str]) -> None:
        async_add_entities([ZigbeeRsUpdate(hub, d) for d in device_ids])

    hub.register_update_platform(add)
    if hub.supports_ota:

        async def refresh(_now: Any = None) -> None:
            await hub.async_refresh_ota_index()

        entry.async_on_unload(async_call_later(hass, INDEX_FIRST, refresh))
        entry.async_on_unload(async_track_time_interval(hass, refresh, INDEX_REFRESH))


class ZigbeeRsUpdate(UpdateEntity):
    """The firmware of one device, upgraded by the hub's OTA server."""

    _attr_has_entity_name = True
    _attr_should_poll = False
    _attr_translation_key = "firmware"
    _attr_device_class = UpdateDeviceClass.FIRMWARE
    _attr_entity_category = EntityCategory.CONFIG
    _attr_supported_features = UpdateEntityFeature.INSTALL | UpdateEntityFeature.PROGRESS

    def __init__(self, hub: ZigbeeRsHub, device_id: str) -> None:
        """Entity of `device_id`."""
        self.hub = hub
        self.device_id = device_id
        self._attr_unique_id = f"{device_id}:ota"
        self._attr_device_info = DeviceInfo(identifiers={(DOMAIN, device_id)})
        # A newer image in the provider index than any on the hub.
        self._index_entry: dict[str, Any] | None = None
        self._apply()

    def _apply(self) -> None:
        rec = self.hub.ota.get(self.device_id) or {}
        available = rec.get("available") or {}
        job = rec.get("job") or {}
        installed = file_version(rec.get("current_version"))
        on_hub = available.get("file_version")
        idx = self.hub.ota_index.best(rec)
        newer = idx is not None and (on_hub is None or idx["fileVersion"] > on_hub)
        self._index_entry = idx if newer else None
        self._attr_installed_version = installed
        if idx is not None and newer:
            self._attr_latest_version = file_version(idx["fileVersion"])
            self._attr_release_summary = f"{idx.get('fileName') or idx['url']} (OTA index)"
        else:
            self._attr_latest_version = file_version(on_hub) or installed
            self._attr_release_summary = available.get("label") or None
        state = job.get("state")
        self._attr_in_progress = state in RUNNING
        # A percentage only while blocks flow; waiting for the device's
        # query and its reboot are indeterminate.
        progress = job.get("progress") if state == "downloading" else None
        self._attr_update_percentage = progress if isinstance(progress, int) else None

    @property
    def available(self) -> bool:
        """Connected and the device is available."""
        dev = self.hub.devices.get(self.device_id)
        return self.hub.connected and dev is not None and dev.get("available") is not False

    async def async_added_to_hass(self) -> None:
        """Follow the device's OTA record."""
        self.async_on_remove(self.hub.add_listener(self._attr_unique_id, self._updated))

    @callback
    def _updated(self) -> None:
        self._apply()
        self.async_write_ha_state()

    async def async_install(self, version: str | None, backup: bool, **kwargs: Any) -> None:
        """Upload the index's image if the hub lacks it, then `ota/start`."""
        if version is not None and version != self.latest_version:
            raise HomeAssistantError(
                f"only the newest image ({self.latest_version}) can be installed"
            )
        params: dict[str, Any] = {"device": self.device_id}
        if (entry := self._index_entry) is not None:
            data = await fetch_image(self.hub.ota_index.session, entry)
            name = str(entry.get("fileName") or "image.ota")
            meta = {
                "min_file_version": entry.get("minFileVersion"),
                "max_file_version": entry.get("maxFileVersion"),
            }
            if self.hub.ota_store:
                params["image"] = (await upload(self.hub, data, name, meta))["image"]
            else:
                # No image store on the hub: Home Assistant serves the file
                # while the device downloads it.
                params["image"] = await self.hub.ota_streams.offer(data, name, meta)
        await self.hub.async_request("ota/start", timeout=COMMAND_TIMEOUT, **params)
