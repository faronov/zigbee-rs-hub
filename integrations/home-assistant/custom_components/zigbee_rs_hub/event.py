"""Event entities (remotes, GPD commands, DP actions)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.event import EventDeviceClass, EventEntity
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Event entities of the hub's devices."""
    setup_platform(entry, "event", async_add_entities, ZigbeeRsEvent)


class ZigbeeRsEvent(ZigbeeRsEntity, EventEntity):
    """Fires the hub's `entity_event`s."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_device_class = enum_or_none(EventDeviceClass, record.get("device_class"))
        self._attr_event_types = [str(t) for t in capability(record, "event_types", [])]

    async def async_added_to_hass(self) -> None:
        """Also follow the entity's events."""
        await super().async_added_to_hass()
        self.async_on_remove(self.hub.add_event_listener(self.uid, self._handle_event))

    @callback
    def _handle_event(self, event: dict[str, Any]) -> None:
        event_type = str(event.get("event_type"))
        if event_type not in self.event_types:
            return
        attributes = event.get("attributes")
        self._trigger_event(event_type, attributes if isinstance(attributes, dict) else None)
        self.async_write_ha_state()
