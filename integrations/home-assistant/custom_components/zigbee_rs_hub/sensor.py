"""Sensors (spec 4.3: number, string, enum label, timestamp)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorStateClass,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.util import dt as dt_util

from . import ZigbeeRsHubConfigEntry
from .entity import HubEntity, ZigbeeRsEntity, capability, enum_or_none, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Sensors of the hub's devices."""
    hub = entry.runtime_data
    if hub.supports_network:
        async_add_entities([NetworkStateSensor(hub), ChannelSensor(hub), PermitJoinEndsSensor(hub)])
    if hub.catalog.supported:
        async_add_entities([CatalogSensor(hub)])
    setup_platform(entry, "sensor", async_add_entities, ZigbeeRsSensor)


class ZigbeeRsSensor(ZigbeeRsEntity, SensorEntity):
    """A hub sensor."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        options = capability(record, "options")
        self._attr_device_class = enum_or_none(SensorDeviceClass, record.get("device_class"))
        if options and self._attr_device_class is None:
            self._attr_device_class = SensorDeviceClass.ENUM
        self._attr_options = [str(o) for o in options] if options else None
        self._attr_native_unit_of_measurement = record.get("unit")
        self._attr_state_class = enum_or_none(SensorStateClass, record.get("state_class"))
        precision = capability(record, "suggested_display_precision", record.get("precision"))
        self._attr_suggested_display_precision = precision if isinstance(precision, int) else None

    @property
    def native_value(self) -> Any:
        """The hub's value in the entity's unit."""
        value = self.hub_state
        if value is None:
            return None
        if self.device_class == SensorDeviceClass.TIMESTAMP:
            return self.hub.hub_time(value)
        if self.device_class == SensorDeviceClass.ENUM:
            value = str(value)
            return value if not self._attr_options or value in self._attr_options else None
        return value

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        """Extra attributes the hub sends."""
        return self.hub_attributes or None


# network/info `state` and `network_state` events.
NETWORK_STATES = ["up", "formed_down", "none", "down", "forming", "leaving"]


class NetworkStateSensor(HubEntity, SensorEntity):
    """The Zigbee network's state on the NCP."""

    _attr_device_class = SensorDeviceClass.ENUM
    _attr_options = NETWORK_STATES
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, hub: Any) -> None:
        """Sensor of `hub`."""
        super().__init__(hub, "network_state")

    @property
    def native_value(self) -> str | None:
        """Network state from network/info and `network_state` events."""
        state = self.hub.network.get("state")
        return state if state in NETWORK_STATES else None

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        """Network facts from network/info."""
        net = self.hub.network
        pan = net.get("pan_id")
        return {
            "pan_id": f"0x{pan:04X}" if isinstance(pan, int) else None,
            "extended_pan_id": net.get("ext_pan_id"),
            "nwk_update_id": net.get("nwk_update_id"),
            "ownership": net.get("ownership"),
            "session": net.get("session"),
        }


class ChannelSensor(HubEntity, SensorEntity):
    """The Zigbee channel."""

    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, hub: Any) -> None:
        """Sensor of `hub`."""
        super().__init__(hub, "channel")

    @property
    def native_value(self) -> int | None:
        """Channel 11-26."""
        channel = self.hub.network.get("channel")
        return channel if isinstance(channel, int) else None


class PermitJoinEndsSensor(HubEntity, SensorEntity):
    """When joining closes (HA shows it as a countdown); unknown while closed."""

    _attr_device_class = SensorDeviceClass.TIMESTAMP

    def __init__(self, hub: Any) -> None:
        """Sensor of `hub`."""
        super().__init__(hub, "permit_join_until")

    @property
    def native_value(self) -> Any:
        """The deadline."""
        until = self.hub.permit_join_until
        return until if until is not None and until > dt_util.utcnow() else None


class CatalogSensor(HubEntity, SensorEntity):
    """Version (`catalog_date.serial`) of the hub's definitions bank, `none` without one."""

    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, hub: Any) -> None:
        """Sensor of `hub`."""
        super().__init__(hub, "catalog_version")

    async def async_added_to_hass(self) -> None:
        """Follow the connection and the catalog updater."""
        await super().async_added_to_hass()
        self.async_on_remove(self.hub.catalog.add_listener(self._hub_update))

    @property
    def available(self) -> bool:
        """Connected and the status is known."""
        return self.hub.connected and self.hub.catalog.status is not None

    @property
    def native_value(self) -> str | None:
        """`catalog_date.serial` of the installed bank."""
        if self.hub.catalog.status is None:
            return None
        return self.hub.catalog.installed_version or "none"

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        """Bank, manifest, fetched definitions, settings, last check."""
        cat = self.hub.catalog
        st = cat.status or {}
        inst = st.get("installed") if isinstance(st.get("installed"), dict) else {}
        man = st.get("manifest") if isinstance(st.get("manifest"), dict) else {}
        local = st.get("local") if isinstance(st.get("local"), dict) else {}
        missing = st.get("missing") if isinstance(st.get("missing"), list) else []
        return {
            "generation": inst.get("generation"),
            "definitions": inst.get("definitions"),
            "manifest_version": man.get("version"),
            "fetched_definitions": local.get("definitions"),
            "missing_definitions": len(missing),
            "auto_update": cat.auto_update,
            "auto_fetch": cat.auto_fetch,
            "last_check": cat.last_check,
            "last_error": cat.last_error,
        }
