"""Binary sensors, plus the hub's own connection sensor."""

from __future__ import annotations

from typing import Any

from homeassistant.components.binary_sensor import (
    BinarySensorDeviceClass,
    BinarySensorEntity,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, enum_or_none, hub_device_info, setup_platform
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
    async_add_entities([HubConnectedSensor(entry.runtime_data)])
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
