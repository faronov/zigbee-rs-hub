"""Selects (enum attributes, DPs)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.select import SelectEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Selects of the hub's devices."""
    setup_platform(entry, "select", async_add_entities, ZigbeeRsSelect)


class ZigbeeRsSelect(ZigbeeRsEntity, SelectEntity):
    """A hub select."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_options = [str(o) for o in capability(record, "options", [])]

    @property
    def current_option(self) -> str | None:
        """The hub's label."""
        value = self.hub_state
        return str(value) if value is not None and str(value) in self.options else None

    async def async_select_option(self, option: str) -> None:
        """`select_option`."""
        await self.command("select_option", option=option)
