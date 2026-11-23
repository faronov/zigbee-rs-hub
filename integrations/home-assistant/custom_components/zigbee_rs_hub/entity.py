"""Base entity: one hub entity record (spec 4.2) → one HA entity."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import callback
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity import Entity
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .const import DOMAIN, PRIMARY_KEYS, TRANSLATED_KEYS
from .hub import ZigbeeRsHub

# Platforms whose device classes have names in HA's own translations.
_DEVICE_CLASS_NAMED = frozenset({"sensor", "binary_sensor", "number", "button"})


def setup_platform(
    entry: ConfigEntry,
    platform: str,
    async_add_entities: AddEntitiesCallback,
    factory: Callable[[ZigbeeRsHub, dict[str, Any]], Entity],
) -> None:
    """Create the platform's entities now and whenever the hub adds some."""
    hub: ZigbeeRsHub = entry.runtime_data

    @callback
    def add(records: list[dict[str, Any]]) -> None:
        async_add_entities([factory(hub, r) for r in records])

    hub.register_platform(platform, add)


def capability(record: dict[str, Any], name: str, default: Any = None) -> Any:
    """A field of the record's `capabilities`."""
    caps = record.get("capabilities") or {}
    value = caps.get(name)
    return default if value is None else value


def enum_or_none(enum: Any, value: Any) -> Any:
    """`enum(value)`, or `None` for values this HA version does not know."""
    if value is None:
        return None
    try:
        return enum(value)
    except ValueError:
        return None


class ZigbeeRsEntity(Entity):
    """An entity of a hub device; state pushed by the hub."""

    _attr_has_entity_name = True
    _attr_should_poll = False

    def __init__(self, hub: ZigbeeRsHub, record: dict[str, Any]) -> None:
        """Entity for `record`."""
        self.hub = hub
        self.uid: str = record["uid"]
        self.record = record
        self._attr_unique_id = self.uid
        self._attr_device_info = DeviceInfo(identifiers={(DOMAIN, record["device"])})
        self._attr_entity_registry_enabled_default = (
            record.get("enabled_default", True) is not False
        )
        self._attr_entity_category = enum_or_none(EntityCategory, record.get("category"))
        self._set_name(record)
        self._apply_record(record)
        self._apply_state(self.state_msg)

    def _set_name(self, record: dict[str, Any]) -> None:
        # Back to the class defaults first: a renamed record (a definition
        # resolved, a user name) may drop the name for a translation key.
        for attr in ("_attr_name", "_attr_translation_key"):
            self.__dict__.pop(attr, None)
        platform = record.get("platform", "")
        key = record.get("key", "")
        name = record.get("name")
        tkey = record.get("translation_key")
        if name:
            self._attr_name = name[:1].upper() + name[1:]
        elif key in PRIMARY_KEYS:
            self._attr_name = None
        elif (platform, tkey or key) in TRANSLATED_KEYS:
            # Also keys with a device class (`last_seen` is a timestamp).
            self._attr_translation_key = tkey or key
        elif record.get("device_class") and platform in _DEVICE_CLASS_NAMED:
            pass  # HA names it after the device class
        else:
            self._attr_name = (tkey or key).replace("_", " ").capitalize()

    @property
    def state_msg(self) -> dict[str, Any]:
        """The last `entity_state` message (empty before the first one)."""
        return self.hub.states.get(self.uid) or {}

    @property
    def hub_state(self) -> Any:
        """The `state` of the last state message."""
        return self.state_msg.get("state")

    @property
    def hub_attributes(self) -> dict[str, Any]:
        """The `attributes` of the last state message."""
        return self.state_msg.get("attributes") or {}

    @property
    def extra_state_attributes(self) -> dict[str, Any] | None:
        """`restored: true` while the value is the one the hub kept over its restart."""
        return {"restored": True} if self.hub_attributes.get("restored") else None

    @property
    def available(self) -> bool:
        """Connected, device available, entity not orphaned."""
        return self.hub.entity_available(self.uid)

    def _apply_record(self, record: dict[str, Any]) -> None:
        """Static data from the entity record (subclasses)."""

    def _apply_state(self, msg: dict[str, Any]) -> None:
        """State from a state message (subclasses)."""

    async def async_added_to_hass(self) -> None:
        """Follow the hub's updates."""
        self.async_on_remove(self.hub.add_listener(self.uid, self._handle_update))

    @callback
    def _handle_update(self) -> None:
        if self.uid not in self.hub.entities:
            # Removed on the hub (the registry entry goes too): unavailable.
            self.async_write_ha_state()
            return
        self.record = self.hub.entities[self.uid]
        self._set_name(self.record)
        self._apply_record(self.record)
        self._apply_state(self.state_msg)
        self.async_write_ha_state()

    async def command(self, command: str, **params: Any) -> Any:
        """`entities/command` for this entity."""
        return await self.hub.async_command(self.uid, command, params)


def hub_device_info(hub: ZigbeeRsHub) -> DeviceInfo:
    """Device info of the hub itself."""
    return DeviceInfo(identifiers={hub.hub_identifier})


class HubEntity(Entity):
    """An entity of the hub itself (connection, network, permit join)."""

    _attr_has_entity_name = True
    _attr_should_poll = False

    def __init__(self, hub: ZigbeeRsHub, key: str) -> None:
        """Entity `key` of `hub`; `key` is also the translation key."""
        self.hub = hub
        self._attr_translation_key = key
        self._attr_unique_id = f"{hub.entry.entry_id}_{key}"
        self._attr_device_info = hub_device_info(hub)

    @property
    def available(self) -> bool:
        """Only while connected."""
        return self.hub.connected

    async def async_added_to_hass(self) -> None:
        """Follow the hub's connection and network state."""
        self.async_on_remove(self.hub.add_hub_listener(self._hub_update))

    @callback
    def _hub_update(self) -> None:
        self.async_write_ha_state()
