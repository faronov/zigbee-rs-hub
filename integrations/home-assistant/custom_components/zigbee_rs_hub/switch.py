"""Switches (On/Off and boolean settings)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.switch import SwitchDeviceClass, SwitchEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .binary_sensor import to_bool
from .entity import ZigbeeRsEntity, enum_or_none, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Switches of the hub's devices."""
    setup_platform(entry, "switch", async_add_entities, ZigbeeRsSwitch)


class ZigbeeRsSwitch(ZigbeeRsEntity, SwitchEntity):
    """A hub switch."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_device_class = enum_or_none(SwitchDeviceClass, record.get("device_class"))

    @property
    def is_on(self) -> bool | None:
        """The hub's value."""
        return to_bool(self.hub_state)

    async def async_turn_on(self, **kwargs: Any) -> None:
        """`turn_on`."""
        await self.command("turn_on")

    async def async_turn_off(self, **kwargs: Any) -> None:
        """`turn_off`."""
        await self.command("turn_off")
