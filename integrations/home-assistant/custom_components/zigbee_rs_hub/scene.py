"""Scenes of the hub's Zigbee groups (device-support 14.9)."""

from __future__ import annotations

from typing import Any

from homeassistant.components.scene import Scene
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from . import ZigbeeRsHubConfigEntry
from .entity import ZigbeeRsEntity, setup_platform


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ZigbeeRsHubConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Scenes of the hub's groups."""
    setup_platform(entry, "scene", async_add_entities, ZigbeeRsScene)


class ZigbeeRsScene(ZigbeeRsEntity, Scene):
    """A group scene: ZCL Recall Scene to the group (one groupcast)."""

    async def async_activate(self, **kwargs: Any) -> None:
        """`activate` (with HA's optional `transition`, seconds)."""
        params = {"transition": kwargs["transition"]} if "transition" in kwargs else {}
        await self.command("activate", **params)
