"""Numbers (writable attributes, Tuya DPs)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.number import NumberDeviceClass, NumberEntity, NumberMode
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Numbers of the hub's devices."""
    setup_platform(entry, "number", async_add_entities, ZigbeeRsNumber)


class ZigbeeRsNumber(ZigbeeRsEntity, NumberEntity):
    """A hub number."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_device_class = enum_or_none(NumberDeviceClass, record.get("device_class"))
        self._attr_native_unit_of_measurement = record.get("unit")
        self._attr_native_min_value = float(capability(record, "native_min_value", 0))
        self._attr_native_max_value = float(capability(record, "native_max_value", 100))
        self._attr_native_step = float(capability(record, "native_step", 1))
        self._attr_mode = enum_or_none(NumberMode, capability(record, "mode")) or NumberMode.AUTO

    @property
    def native_value(self) -> float | None:
        """The hub's value."""
        value = self.hub_state
        return value if isinstance(value, (int, float)) and not isinstance(value, bool) else None

    async def async_set_native_value(self, value: float) -> None:
        """`set_value`."""
        if value == int(value) and float(self._attr_native_step or 1).is_integer():
            value = int(value)
        await self.command("set_value", value=value)
