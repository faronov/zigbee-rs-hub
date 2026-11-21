"""Covers (Window Covering, DPs)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.cover import (
    ATTR_POSITION,
    ATTR_TILT_POSITION,
    CoverDeviceClass,
    CoverEntity,
    CoverEntityFeature,
)
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform

_FEATURES = {
    "open": CoverEntityFeature.OPEN,
    "close": CoverEntityFeature.CLOSE,
    "stop": CoverEntityFeature.STOP,
    "set_position": CoverEntityFeature.SET_POSITION,
    "open_tilt": CoverEntityFeature.OPEN_TILT,
    "close_tilt": CoverEntityFeature.CLOSE_TILT,
    "stop_tilt": CoverEntityFeature.STOP_TILT,
    "set_tilt_position": CoverEntityFeature.SET_TILT_POSITION,
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Covers of the hub's devices."""
    setup_platform(entry, "cover", async_add_entities, ZigbeeRsCover)


def _int(value: Any) -> int | None:
    return round(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


class ZigbeeRsCover(ZigbeeRsEntity, CoverEntity):
    """A hub cover."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        self._attr_device_class = enum_or_none(CoverDeviceClass, record.get("device_class"))
        features = CoverEntityFeature(0)
        for name in capability(record, "supported_features", []):
            features |= _FEATURES.get(name, CoverEntityFeature(0))
        self._attr_supported_features = features

    @property
    def current_cover_position(self) -> int | None:
        """0 closed .. 100 open."""
        return _int(self.hub_attributes.get("current_position"))

    @property
    def current_cover_tilt_position(self) -> int | None:
        """Tilt 0..100."""
        return _int(self.hub_attributes.get("current_tilt_position"))

    @property
    def is_closed(self) -> bool | None:
        """Closed."""
        state = self.hub_state
        if state in ("closed", "open"):
            return state == "closed"
        pos = self.current_cover_position
        return None if pos is None else pos == 0

    @property
    def is_opening(self) -> bool:
        """Opening."""
        return self.hub_state == "opening"

    @property
    def is_closing(self) -> bool:
        """Closing."""
        return self.hub_state == "closing"

    async def async_open_cover(self, **kwargs: Any) -> None:
        """`open_cover`."""
        await self.command("open_cover")

    async def async_close_cover(self, **kwargs: Any) -> None:
        """`close_cover`."""
        await self.command("close_cover")

    async def async_stop_cover(self, **kwargs: Any) -> None:
        """`stop_cover`."""
        await self.command("stop_cover")

    async def async_set_cover_position(self, **kwargs: Any) -> None:
        """`set_cover_position`."""
        await self.command("set_cover_position", position=kwargs[ATTR_POSITION])

    async def async_set_cover_tilt_position(self, **kwargs: Any) -> None:
        """`set_cover_tilt_position`."""
        await self.command("set_cover_tilt_position", tilt_position=kwargs[ATTR_TILT_POSITION])
