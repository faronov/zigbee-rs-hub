"""Sensors (spec 4.3: number, string, enum label, timestamp)."""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorStateClass,
)
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Sensors of the hub's devices."""
    setup_platform(entry, "sensor", async_add_entities, ZigbeeRsSensor)


class ZigbeeRsSensor(ZigbeeRsEntity, SensorEntity):
    """A hub sensor."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        options = capability(record, "options")
        self._attr_device_class = enum_or_none(SensorDeviceClass, record.get("device_class"))
        if options and self._attr_device_class is None:
            self._attr_device_class = SensorDeviceClass.ENUM
        self._attr_options = [str(o) for o in options] if options else None
        self._attr_native_unit_of_measurement = record.get("unit")
        self._attr_state_class = enum_or_none(SensorStateClass, record.get("state_class"))
        precision = capability(record, "suggested_display_precision", record.get("precision"))
        self._attr_suggested_display_precision = precision if isinstance(precision, int) else None

    @property
    def native_value(self) -> Any:
        """The hub's value in the entity's unit."""
        value = self.hub_state
        if value is None:
            return None
        if self.device_class == SensorDeviceClass.TIMESTAMP:
            if isinstance(value, (int, float)):
                return datetime.fromtimestamp(value, UTC)
            return None
        if self.device_class == SensorDeviceClass.ENUM:
            value = str(value)
            return value if not self._attr_options or value in self._attr_options else None
        return value

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        """Extra attributes the hub sends."""
        return self.hub_attributes or None
