"""Fans (Fan Control)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.fan import FanEntity, FanEntityFeature
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, setup_platform

_FEATURES = {
    "set_speed": FanEntityFeature.SET_SPEED,
    "preset_mode": FanEntityFeature.PRESET_MODE,
    "turn_on": FanEntityFeature.TURN_ON,
    "turn_off": FanEntityFeature.TURN_OFF,
    "oscillate": FanEntityFeature.OSCILLATE,
    "direction": FanEntityFeature.DIRECTION,
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Fans of the hub's devices."""
    setup_platform(entry, "fan", async_add_entities, ZigbeeRsFan)


class ZigbeeRsFan(ZigbeeRsEntity, FanEntity):
    """A hub fan."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_speed_count = int(capability(record, "speed_count", 3))
        self._attr_preset_modes = [str(p) for p in capability(record, "preset_modes", [])] or None
        features = FanEntityFeature(0)
        for name in capability(record, "supported_features", []):
            features |= _FEATURES.get(name, FanEntityFeature(0))
        if not self._attr_preset_modes:
            features &= ~FanEntityFeature.PRESET_MODE
        self._attr_supported_features = features

    @property
    def is_on(self) -> bool | None:
        """On."""
        state = self.hub_state
        return None if state is None else state == "on"

    @property
    def percentage(self) -> int | None:
        """Speed 0..100."""
        value = self.hub_attributes.get("percentage")
        return round(value) if isinstance(value, (int, float)) else None

    @property
    def preset_mode(self) -> str | None:
        """Preset."""
        value = self.hub_attributes.get("preset_mode")
        return None if value is None else str(value)

    async def async_turn_on(
        self, percentage: int | None = None, preset_mode: str | None = None, **kwargs: Any
    ) -> None:
        """`turn_on`."""
        params: dict[str, Any] = {}
        if percentage is not None:
            params["percentage"] = percentage
        if preset_mode is not None:
            params["preset_mode"] = preset_mode
        await self.command("turn_on", **params)

    async def async_turn_off(self, **kwargs: Any) -> None:
        """`turn_off`."""
        await self.command("turn_off")

    async def async_set_percentage(self, percentage: int) -> None:
        """`set_percentage`."""
        await self.command("set_percentage", percentage=percentage)

    async def async_set_preset_mode(self, preset_mode: str) -> None:
        """`set_preset_mode`."""
        await self.command("set_preset_mode", preset_mode=preset_mode)
