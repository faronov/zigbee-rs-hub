"""Climate (Thermostat cluster, DPs)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.climate import (
    ATTR_HVAC_MODE,
    ClimateEntity,
    ClimateEntityFeature,
    HVACAction,
    HVACMode,
)
from homeassistant.const import ATTR_TEMPERATURE, UnitOfTemperature
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, enum_or_none, setup_platform

_FEATURES = {
    "target_temperature": ClimateEntityFeature.TARGET_TEMPERATURE,
    "target_temperature_range": ClimateEntityFeature.TARGET_TEMPERATURE_RANGE,
    "preset_mode": ClimateEntityFeature.PRESET_MODE,
    "fan_mode": ClimateEntityFeature.FAN_MODE,
    "turn_on": ClimateEntityFeature.TURN_ON,
    "turn_off": ClimateEntityFeature.TURN_OFF,
}


def _num(value: Any) -> float | None:
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Climate entities of the hub's devices."""
    setup_platform(entry, "climate", async_add_entities, ZigbeeRsClimate)


class ZigbeeRsClimate(ZigbeeRsEntity, ClimateEntity):
    """A hub thermostat."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        modes = [enum_or_none(HVACMode, m) for m in capability(record, "hvac_modes", [])]
        self._attr_hvac_modes = [m for m in modes if m is not None] or [HVACMode.HEAT]
        self._attr_preset_modes = [str(p) for p in capability(record, "preset_modes", [])] or None
        self._attr_fan_modes = [str(p) for p in capability(record, "fan_modes", [])] or None
        self._attr_min_temp = float(capability(record, "min_temp", 7))
        self._attr_max_temp = float(capability(record, "max_temp", 35))
        self._attr_target_temperature_step = float(
            capability(record, "target_temperature_step", 0.5)
        )
        unit = capability(record, "temperature_unit", "°C")
        self._attr_temperature_unit = (
            UnitOfTemperature.FAHRENHEIT if unit == "°F" else UnitOfTemperature.CELSIUS
        )
        features = ClimateEntityFeature(0)
        for name in capability(record, "supported_features", []):
            features |= _FEATURES.get(name, ClimateEntityFeature(0))
        if not self._attr_preset_modes:
            features &= ~ClimateEntityFeature.PRESET_MODE
        if not self._attr_fan_modes:
            features &= ~ClimateEntityFeature.FAN_MODE
        self._attr_supported_features = features

    @property
    def hvac_mode(self) -> HVACMode | None:
        """Mode."""
        return enum_or_none(HVACMode, self.hub_state)

    @property
    def hvac_action(self) -> HVACAction | None:
        """What the device is doing."""
        return enum_or_none(HVACAction, self.hub_attributes.get("hvac_action"))

    @property
    def current_temperature(self) -> float | None:
        """Measured temperature."""
        return _num(self.hub_attributes.get("current_temperature"))

    @property
    def target_temperature(self) -> float | None:
        """Setpoint."""
        return _num(self.hub_attributes.get("target_temperature"))

    @property
    def preset_mode(self) -> str | None:
        """Preset (`preset` part)."""
        value = self.hub_attributes.get("preset", self.hub_attributes.get("preset_mode"))
        return None if value is None else str(value)

    @property
    def fan_mode(self) -> str | None:
        """Fan mode."""
        value = self.hub_attributes.get("fan_mode")
        return None if value is None else str(value)

    async def async_set_hvac_mode(self, hvac_mode: HVACMode) -> None:
        """`set_hvac_mode`."""
        await self.command("set_hvac_mode", hvac_mode=str(hvac_mode))

    async def async_set_temperature(self, **kwargs: Any) -> None:
        """`set_temperature`."""
        params: dict[str, Any] = {"temperature": kwargs[ATTR_TEMPERATURE]}
        if (mode := kwargs.get(ATTR_HVAC_MODE)) is not None:
            params["hvac_mode"] = str(mode)
        await self.command("set_temperature", **params)

    async def async_set_preset_mode(self, preset_mode: str) -> None:
        """`set_preset_mode`."""
        await self.command("set_preset_mode", preset_mode=preset_mode)

    async def async_set_fan_mode(self, fan_mode: str) -> None:
        """`set_fan_mode`."""
        await self.command("set_fan_mode", fan_mode=fan_mode)

    async def async_turn_on(self) -> None:
        """`turn_on`."""
        await self.command("turn_on")

    async def async_turn_off(self) -> None:
        """`turn_off`."""
        await self.command("turn_off")
