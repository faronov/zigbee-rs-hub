"""Locks (Door Lock, DPs)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.lock import LockEntity, LockEntityFeature
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, capability, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Locks of the hub's devices."""
    setup_platform(entry, "lock", async_add_entities, ZigbeeRsLock)


class ZigbeeRsLock(ZigbeeRsEntity, LockEntity):
    """A hub lock."""

    def _apply_record(self, record: dict[str, Any]) -> None:
        features = LockEntityFeature(0)
        if "open" in capability(record, "supported_features", []):
            features |= LockEntityFeature.OPEN
        self._attr_supported_features = features

    @property
    def is_locked(self) -> bool | None:
        """Locked."""
        state = self.hub_state
        return None if state is None else state == "locked"

    @property
    def is_locking(self) -> bool:
        """Locking."""
        return self.hub_state == "locking"

    @property
    def is_unlocking(self) -> bool:
        """Unlocking."""
        return self.hub_state == "unlocking"

    @property
    def is_jammed(self) -> bool:
        """Jammed."""
        return self.hub_state == "jammed"

    @property
    def is_open(self) -> bool:
        """Open."""
        return self.hub_state == "open"

    @property
    def is_opening(self) -> bool:
        """Opening."""
        return self.hub_state == "opening"

    async def async_lock(self, **kwargs: Any) -> None:
        """`lock`."""
        await self.command("lock")

    async def async_unlock(self, **kwargs: Any) -> None:
        """`unlock`."""
        await self.command("unlock")

    async def async_open(self, **kwargs: Any) -> None:
        """`open`."""
        await self.command("open")
