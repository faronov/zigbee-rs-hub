"""Lights (On/Off + Level + Color Control, or definition parts)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.light import (
    ATTR_BRIGHTNESS,
    ATTR_COLOR_TEMP_KELVIN,
    ATTR_EFFECT,
    ATTR_HS_COLOR,
    ATTR_TRANSITION,
    ATTR_XY_COLOR,
    ColorMode,
    LightEntity,
    LightEntityFeature,
)
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .binary_sensor import to_bool
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform

# ZCL Color Control ColorMode attribute values.
_ZCL_COLOR_MODE = {0: ColorMode.HS, 1: ColorMode.XY, 2: ColorMode.COLOR_TEMP}
_FEATURES = {"transition": LightEntityFeature.TRANSITION, "effect": LightEntityFeature.EFFECT}


def _pair(value: Any) -> tuple[float, float] | None:
    if isinstance(value, (list, tuple)) and len(value) == 2:
        try:
            return (float(value[0]), float(value[1]))
        except (TypeError, ValueError):
            return None
    return None


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Lights of the hub's devices."""
    setup_platform(entry, "light", async_add_entities, ZigbeeRsLight)


class ZigbeeRsLight(ZigbeeRsEntity, LightEntity):
    """A hub light."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        modes = {
            m
            for m in (
                enum_or_none(ColorMode, x) for x in capability(record, "supported_color_modes", [])
            )
            if m is not None
        }
        self._attr_supported_color_modes = modes or {ColorMode.ONOFF}
        features = LightEntityFeature(0)
        for name in capability(record, "supported_features", []):
            features |= _FEATURES.get(name, LightEntityFeature(0))
        effects = capability(record, "effect_list")
        self._attr_effect_list = [str(e) for e in effects] if effects else None
        if not self._attr_effect_list:
            features &= ~LightEntityFeature.EFFECT
        self._attr_supported_features = features
        self._attr_min_color_temp_kelvin = int(capability(record, "min_color_temp_kelvin", 2000))
        self._attr_max_color_temp_kelvin = int(capability(record, "max_color_temp_kelvin", 6500))

    @property
    def is_on(self) -> bool | None:
        """On/off."""
        return to_bool(self.hub_state)

    @property
    def brightness(self) -> int | None:
        """0..255."""
        value = self.hub_attributes.get("brightness")
        return round(value) if isinstance(value, (int, float)) else None

    @property
    def color_temp_kelvin(self) -> int | None:
        """Colour temperature."""
        value = self.hub_attributes.get("color_temp_kelvin")
        return round(value) if isinstance(value, (int, float)) else None

    @property
    def xy_color(self) -> tuple[float, float] | None:
        """CIE xy."""
        return _pair(self.hub_attributes.get("xy_color"))

    @property
    def hs_color(self) -> tuple[float, float] | None:
        """Hue/saturation."""
        return _pair(self.hub_attributes.get("hs_color"))

    @property
    def effect(self) -> str | None:
        """Current effect."""
        value = self.hub_attributes.get("effect")
        return None if value is None else str(value)

    @property
    def color_mode(self) -> ColorMode:
        """The mode the hub reports, else one the state supports."""
        modes = self.supported_color_modes or {ColorMode.ONOFF}
        raw = self.hub_attributes.get("color_mode")
        mode = _ZCL_COLOR_MODE.get(raw) if isinstance(raw, int) else enum_or_none(ColorMode, raw)
        if mode in modes:
            return mode
        if len(modes) == 1:
            return next(iter(modes))
        for candidate, present in (
            (ColorMode.COLOR_TEMP, self.color_temp_kelvin),
            (ColorMode.XY, self.xy_color),
            (ColorMode.HS, self.hs_color),
        ):
            if candidate in modes and present is not None:
                return candidate
        return sorted(modes)[0]

    async def async_turn_on(self, **kwargs: Any) -> None:
        """`turn_on` with HA's parameter names."""
        params: dict[str, Any] = {}
        for attr in (ATTR_BRIGHTNESS, ATTR_TRANSITION, ATTR_COLOR_TEMP_KELVIN, ATTR_EFFECT):
            if attr in kwargs:
                params[attr] = kwargs[attr]
        for attr in (ATTR_XY_COLOR, ATTR_HS_COLOR):
            if attr in kwargs:
                params[attr] = list(kwargs[attr])
        await self.command("turn_on", **params)

    async def async_turn_off(self, **kwargs: Any) -> None:
        """`turn_off`."""
        params = {ATTR_TRANSITION: kwargs[ATTR_TRANSITION]} if ATTR_TRANSITION in kwargs else {}
        await self.command("turn_off", **params)
