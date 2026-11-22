"""Switches (On/Off and boolean settings)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.switch import SwitchDeviceClass, SwitchEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .binary_sensor import to_bool
from .entity import (
    AutomationEntity,
    ZigbeeRsEntity,
    enum_or_none,
    setup_automations,
    setup_platform,
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Switches of the hub's devices; one per hub automation (enabled)."""
    setup_platform(entry, "switch", async_add_entities, ZigbeeRsSwitch)
    setup_automations(entry, "switch", async_add_entities, AutomationSwitch)


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


class AutomationSwitch(AutomationEntity, SwitchEntity):
    """Whether one of the hub's local automations is enabled."""

    _attr_translation_key = "automation"

    @property
    def is_on(self) -> bool | None:
        """Enabled on the hub."""
        enabled = self.rule.get("enabled")
        return enabled if isinstance(enabled, bool) else None

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        """The automation's ID, last run and why it may be suspended."""
        rule = self.rule
        last = self.hub.hub_time(rule.get("last_run"))
        return {
            "automation_id": self.rule_id,
            "last_run": last.isoformat() if last else None,
            "last_result": rule.get("last_result"),
            "runs": rule.get("runs"),
            "suspended": rule.get("suspended"),
        }

    async def async_turn_on(self, **kwargs: Any) -> None:
        """`automations/enable`."""
        await self.hub.async_automation("enable", self.rule_id)

    async def async_turn_off(self, **kwargs: Any) -> None:
        """`automations/disable`."""
        await self.hub.async_automation("disable", self.rule_id)
