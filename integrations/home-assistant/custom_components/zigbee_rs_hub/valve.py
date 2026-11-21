"""Valves (DPs, On/Off on valves)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.valve import (
    ValveDeviceClass,
    ValveEntity,
    ValveEntityFeature,
)
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform

_FEATURES = {
    "open": ValveEntityFeature.OPEN,
    "close": ValveEntityFeature.CLOSE,
    "set_position": ValveEntityFeature.SET_POSITION,
    "stop": ValveEntityFeature.STOP,
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Valves of the hub's devices."""
    setup_platform(entry, "valve", async_add_entities, ZigbeeRsValve)


class ZigbeeRsValve(ZigbeeRsEntity, ValveEntity):
    """A hub valve."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_device_class = enum_or_none(ValveDeviceClass, record.get("device_class"))
        self._attr_reports_position = bool(capability(record, "reports_position", False))
        features = ValveEntityFeature(0)
        for name in capability(record, "supported_features", []):
            features |= _FEATURES.get(name, ValveEntityFeature(0))
        self._attr_supported_features = features

    @property
    def current_valve_position(self) -> int | None:
        """0 closed .. 100 open."""
        value = self.hub_attributes.get("current_position")
        return round(value) if isinstance(value, (int, float)) else None

    @property
    def is_closed(self) -> bool | None:
        """Closed."""
        state = self.hub_state
        if state in ("closed", "open"):
            return state == "closed"
        if isinstance(state, bool):
            return not state
        pos = self.current_valve_position
        return None if pos is None else pos == 0

    @property
    def is_opening(self) -> bool:
        """Opening."""
        return self.hub_state == "opening"

    @property
    def is_closing(self) -> bool:
        """Closing."""
        return self.hub_state == "closing"

    async def async_open_valve(self) -> None:
        """`open_valve`."""
        await self.command("open_valve")

    async def async_close_valve(self) -> None:
        """`close_valve`."""
        await self.command("close_valve")

    async def async_set_valve_position(self, position: int) -> None:
        """`set_valve_position`."""
        await self.command("set_valve_position", position=position)
