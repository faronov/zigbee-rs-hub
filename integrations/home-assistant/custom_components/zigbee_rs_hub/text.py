"""Texts (string attributes, schedules)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.text import TextEntity, TextMode
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Texts of the hub's devices."""
    setup_platform(entry, "text", async_add_entities, ZigbeeRsText)


class ZigbeeRsText(ZigbeeRsEntity, TextEntity):
    """A hub text."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_native_min = int(capability(record, "native_min", 0))
        self._attr_native_max = int(capability(record, "native_max", 255))
        self._attr_pattern = capability(record, "pattern")
        self._attr_mode = enum_or_none(TextMode, capability(record, "mode")) or TextMode.TEXT

    @property
    def native_value(self) -> str | None:
        """The hub's value."""
        value = self.hub_state
        return None if value is None else str(value)

    async def async_set_value(self, value: str) -> None:
        """`set_value`."""
        await self.command("set_value", value=value)
