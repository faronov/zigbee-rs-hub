"""Buttons of the hub's devices, plus the hub's own permit-join button."""

from __future__ import annotations

from typing import Any

from homeassistant.components.button import ButtonDeviceClass, ButtonEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .const import PERMIT_JOIN_DEFAULT
from .entity import (
    AutomationEntity,
    ZigbeeRsEntity,
    enum_or_none,
    hub_device_info,
    setup_automations,
    setup_platform,
)
from .hub import ZigbeeRsHub


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Buttons; one per hub automation (run it now)."""
    async_add_entities([PermitJoinButton(entry.runtime_data)])
    setup_platform(entry, "button", async_add_entities, ZigbeeRsButton)
    setup_automations(entry, "button", async_add_entities, AutomationRunButton)


class ZigbeeRsButton(ZigbeeRsEntity, ButtonEntity):
    """A hub button (commands, one-shot writes)."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_device_class = enum_or_none(ButtonDeviceClass, record.get("device_class"))

    async def async_press(self) -> None:
        """`press`."""
        await self.command("press")


class PermitJoinButton(ButtonEntity):
    """Opens the network for joining (Zigbee and Green Power)."""

    _attr_has_entity_name = True
    _attr_should_poll = False
    _attr_translation_key = "permit_join"

    def __init__(self, hub: ZigbeeRsHub) -> None:
        """Button of `hub`."""
        self.hub = hub
        self._attr_unique_id = f"{hub.entry.entry_id}_permit_join"
        self._attr_device_info = hub_device_info(hub)

    @property
    def available(self) -> bool:
        """Only while connected."""
        return self.hub.connected

    async def async_added_to_hass(self) -> None:
        """Follow the connection state."""
        self.async_on_remove(self.hub.add_hub_listener(self.async_write_ha_state))

    async def async_press(self) -> None:
        """Permit joining for the default duration."""
        await self.hub.async_permit_join(PERMIT_JOIN_DEFAULT, include_gp=True)


class AutomationRunButton(AutomationEntity, ButtonEntity):
    """Runs one of the hub's local automations now (its actions, a test run)."""

    _attr_translation_key = "automation_run"

    def __init__(self, hub: ZigbeeRsHub, rule_id: str) -> None:
        """Run button of automation `rule_id`."""
        super().__init__(hub, rule_id, "_run")

    async def async_press(self) -> None:
        """`automations/run`."""
        await self.hub.async_automation("run", self.rule_id)
