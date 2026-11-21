"""Diagnostics: the hub's model as the integration sees it, keys redacted."""

from __future__ import annotations

from typing import Any

from homeassistant.components.diagnostics import async_redact_data
from homeassistant.core import HomeAssistant
from homeassistant.helpers.device_registry import DeviceEntry

from . import ZigbeeRsHubConfigEntry
from .const import DOMAIN, TO_REDACT


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: ZigbeeRsHubConfigEntry
) -> dict[str, Any]:
    """Entry diagnostics."""
    hub = entry.runtime_data
    return async_redact_data(
        {
            "entry": {"data": dict(entry.data), "unique_id": entry.unique_id},
            "connected": hub.connected,
            "url": hub.url,
            "seq": hub.seq,
            "hello": hub.client.hello if hub.client else None,
            "info": hub.info,
            "devices": list(hub.devices.values()),
            "entities": list(hub.entities.values()),
            "states": list(hub.states.values()),
            "catalog": hub.catalog.as_dict(),
        },
        TO_REDACT,
    )


async def async_get_device_diagnostics(
    hass: HomeAssistant, entry: ZigbeeRsHubConfigEntry, device: DeviceEntry
) -> dict[str, Any]:
    """Device diagnostics: the record, its entities and `devices/diagnostics`."""
    hub = entry.runtime_data
    ident = next((i for d, i in device.identifiers if d == DOMAIN and i in hub.devices), None)
    if ident is None:
        return {"hub": {"connected": hub.connected, "info": hub.info}}
    return async_redact_data(
        {
            "device": hub.devices[ident],
            "entities": [e for e in hub.entities.values() if e.get("device") == ident],
            "states": [
                hub.states[u]
                for u, e in hub.entities.items()
                if e.get("device") == ident and u in hub.states
            ],
            "hub_diagnostics": await hub.async_device_diagnostics(ident),
        },
        TO_REDACT,
    )
