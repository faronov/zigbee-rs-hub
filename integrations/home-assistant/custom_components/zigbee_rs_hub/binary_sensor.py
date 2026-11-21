"""Binary sensors, plus the hub's own connection sensor."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from homeassistant.components.binary_sensor import (
    BinarySensorDeviceClass,
    BinarySensorEntity,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.event import async_track_point_in_utc_time
from homeassistant.util import dt as dt_util

from . import ZigbeeRsHubConfigEntry
from .entity import HubEntity, ZigbeeRsEntity, enum_or_none, hub_device_info, setup_platform
from .hub import ZigbeeRsHub


def to_bool(value: Any) -> bool | None:
    """Hub booleans (`true`/`false`, `"on"`/`"off"`, numbers)."""
    if value is None:
        return None
    if isinstance(value, str):
        v = value.lower()
        if v in ("on", "true", "open", "1"):
            return True
        if v in ("off", "false", "closed", "0"):
            return False
        return None
    return bool(value)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Binary sensors of the hub's devices and the hub connection."""
    hub = entry.runtime_data
    async_add_entities([HubConnectedSensor(hub)])
    if hub.supports_network:
        async_add_entities([PermitJoinSensor(hub)])
    setup_platform(entry, "binary_sensor", async_add_entities, ZigbeeRsBinarySensor)


class ZigbeeRsBinarySensor(ZigbeeRsEntity, BinarySensorEntity):
    """A hub binary sensor."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_device_class = enum_or_none(BinarySensorDeviceClass, record.get("device_class"))

    @property
    def is_on(self) -> bool | None:
        """The hub's value."""
        return to_bool(self.hub_state)

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        """Extra attributes the hub sends."""
        return self.hub_attributes or None


class HubConnectedSensor(BinarySensorEntity):
    """Whether the WebSocket connection to the hub is up."""

    _attr_has_entity_name = True
    _attr_should_poll = False
    _attr_device_class = BinarySensorDeviceClass.CONNECTIVITY
    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _attr_translation_key = "hub_connected"

    def __init__(self, hub: ZigbeeRsHub) -> None:
        """Sensor of `hub`."""
        self.hub = hub
        self._attr_unique_id = f"{hub.entry.entry_id}_connected"
        self._attr_device_info = hub_device_info(hub)

    @property
    def is_on(self) -> bool:
        """Connected."""
        return self.hub.connected

    async def async_added_to_hass(self) -> None:
        """Follow the connection state."""
        self.async_on_remove(self.hub.add_hub_listener(self.async_write_ha_state))


class PermitJoinSensor(HubEntity, BinarySensorEntity):
    """Whether the network accepts joins (API 1.1 `permit_join` events)."""

    def __init__(self, hub: ZigbeeRsHub) -> None:
        """Sensor of `hub`."""
        super().__init__(hub, "permit_join_open")
        self._unsub_end: Callable[[], None] | None = None

    @property
    def is_on(self) -> bool:
        """Open until the hub's deadline."""
        until = self.hub.permit_join_until
        return until is not None and until > dt_util.utcnow()

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        """When joining closes."""
        until = self.hub.permit_join_until
        return {"until": until.isoformat() if self.is_on and until else None}

    @callback
    def _hub_update(self) -> None:
        if self._unsub_end is not None:
            self._unsub_end()
            self._unsub_end = None
        if self.is_on and self.hub.permit_join_until is not None:
            # Closes on time even if the hub's closing event is missed.
            self._unsub_end = async_track_point_in_utc_time(
                self.hass, self._ended, self.hub.permit_join_until
            )
        self.async_write_ha_state()

    async def async_added_to_hass(self) -> None:
        """Also arm the closing timer for a window that is already open."""
        await super().async_added_to_hass()
        if self.is_on and self.hub.permit_join_until is not None:
            self._unsub_end = async_track_point_in_utc_time(
                self.hass, self._ended, self.hub.permit_join_until
            )

    @callback
    def _ended(self, _now: Any) -> None:
        self._unsub_end = None
        self.async_write_ha_state()

    async def async_will_remove_from_hass(self) -> None:
        """Drop the timer."""
        if self._unsub_end is not None:
            self._unsub_end()
            self._unsub_end = None
