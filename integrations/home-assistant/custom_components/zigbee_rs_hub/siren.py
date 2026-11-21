"""Sirens (IAS WD, DPs)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.siren import (
    ATTR_DURATION,
    ATTR_TONE,
    ATTR_VOLUME_LEVEL,
    SirenEntity,
    SirenEntityFeature,
)
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .binary_sensor import to_bool
from .entity import ZigbeeRsEntity, capability, setup_platform

_FEATURES = {
    "turn_on": SirenEntityFeature.TURN_ON,
    "turn_off": SirenEntityFeature.TURN_OFF,
    "tones": SirenEntityFeature.TONES,
    "duration": SirenEntityFeature.DURATION,
    "volume_set": SirenEntityFeature.VOLUME_SET,
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Sirens of the hub's devices."""
    setup_platform(entry, "siren", async_add_entities, ZigbeeRsSiren)


class ZigbeeRsSiren(ZigbeeRsEntity, SirenEntity):
    """A hub siren."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        tones = capability(record, "available_tones")
        self._attr_available_tones = list(tones) if tones else None
        features = SirenEntityFeature(0)
        for name in capability(record, "supported_features", []):
            features |= _FEATURES.get(name, SirenEntityFeature(0))
        if not self._attr_available_tones:
            features &= ~SirenEntityFeature.TONES
        self._attr_supported_features = features

    @property
    def is_on(self) -> bool | None:
        """Sounding."""
        return to_bool(self.hub_state)

    async def async_turn_on(self, **kwargs: Any) -> None:
        """`turn_on` (tone, duration, volume_level)."""
        params = {
            k: kwargs[k] for k in (ATTR_TONE, ATTR_DURATION, ATTR_VOLUME_LEVEL) if k in kwargs
        }
        await self.command("turn_on", **params)

    async def async_turn_off(self, **kwargs: Any) -> None:
        """`turn_off`."""
        await self.command("turn_off")
