"""The zigbee-rs hub integration: the hub's WebSocket JSON API in Home Assistant."""

from __future__ import annotations

import logging

import voluptuous as vol

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import CONF_HOST, CONF_PORT, EVENT_CORE_CONFIG_UPDATE
from homeassistant.core import Event, HomeAssistant, ServiceCall, callback
from homeassistant.exceptions import (
    ConfigEntryAuthFailed,
    ConfigEntryNotReady,
    HomeAssistantError,
    ServiceValidationError,
)
from homeassistant.helpers import config_validation as cv, device_registry as dr
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType

from .api import HubError, InvalidAuth
from .const import (
    ATTR_BASE_URL,
    ATTR_CONFIG_ENTRY_ID,
    ATTR_DURATION,
    ATTR_FORCE,
    ATTR_INCLUDE_GP,
    DOMAIN,
    PERMIT_JOIN_DEFAULT,
    PLATFORMS,
    SERVICE_PERMIT_JOIN,
    SERVICE_RECONFIGURE,
    SERVICE_REINTERVIEW,
    SERVICE_UPDATE_CATALOG,
)
from .hub import ZigbeeRsHub

_LOGGER = logging.getLogger(__name__)

type ZigbeeRsHubConfigEntry = ConfigEntry[ZigbeeRsHub]

CONFIG_SCHEMA = cv.config_entry_only_config_schema(DOMAIN)

ATTR_DEVICE_ID = "device_id"

PERMIT_JOIN_SCHEMA = vol.Schema(
    {
        vol.Optional(ATTR_CONFIG_ENTRY_ID): cv.string,
        vol.Optional(ATTR_DURATION, default=PERMIT_JOIN_DEFAULT): vol.All(
            vol.Coerce(int), vol.Range(min=0, max=254)
        ),
        vol.Optional(ATTR_INCLUDE_GP, default=False): cv.boolean,
        vol.Optional(ATTR_DEVICE_ID): cv.string,
    }
)
DEVICE_SCHEMA = vol.Schema({vol.Required(ATTR_DEVICE_ID): cv.string})
UPDATE_CATALOG_SCHEMA = vol.Schema(
    {
        vol.Optional(ATTR_CONFIG_ENTRY_ID): cv.string,
        vol.Optional(ATTR_FORCE, default=False): cv.boolean,
        vol.Optional(ATTR_BASE_URL): cv.url,
    }
)


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Register the services."""

    def loaded_hubs() -> list[ZigbeeRsHub]:
        return [e.runtime_data for e in hass.config_entries.async_loaded_entries(DOMAIN)]

    def hub_of_device(device_id: str) -> tuple[ZigbeeRsHub, str]:
        dev = dr.async_get(hass).async_get(device_id)
        if dev is not None:
            for hub in loaded_hubs():
                if hub.entry.entry_id not in dev.config_entries:
                    continue
                for domain, ident in dev.identifiers:
                    if domain == DOMAIN and ident in hub.devices:
                        return hub, ident
        raise ServiceValidationError(
            translation_domain=DOMAIN,
            translation_key="unknown_device",
            translation_placeholders={"device_id": device_id},
        )

    async def permit_join(call: ServiceCall) -> None:
        target = None
        if device_id := call.data.get(ATTR_DEVICE_ID):
            hub, target = hub_of_device(device_id)
            hubs = [hub]
        else:
            hubs = loaded_hubs()
            if entry_id := call.data.get(ATTR_CONFIG_ENTRY_ID):
                hubs = [h for h in hubs if h.entry.entry_id == entry_id]
        if not hubs:
            raise ServiceValidationError(translation_domain=DOMAIN, translation_key="no_hub")
        for hub in hubs:
            await hub.async_permit_join(
                call.data[ATTR_DURATION], call.data[ATTR_INCLUDE_GP], target
            )

    async def device_request(call: ServiceCall, req_type: str) -> None:
        hub, ident = hub_of_device(call.data[ATTR_DEVICE_ID])
        await hub.async_request(req_type, timeout=60.0, device=ident)

    async def reinterview(call: ServiceCall) -> None:
        await device_request(call, "devices/reinterview")

    async def reconfigure(call: ServiceCall) -> None:
        await device_request(call, "devices/reconfigure")

    async def update_catalog(call: ServiceCall) -> None:
        hubs = loaded_hubs()
        if entry_id := call.data.get(ATTR_CONFIG_ENTRY_ID):
            hubs = [h for h in hubs if h.entry.entry_id == entry_id]
        if not hubs:
            raise ServiceValidationError(translation_domain=DOMAIN, translation_key="no_hub")
        hubs = [h for h in hubs if h.catalog.supported]
        if not hubs:
            raise ServiceValidationError(translation_domain=DOMAIN, translation_key="no_catalog")
        errors = []
        for hub in hubs:
            try:
                await hub.catalog.async_update(
                    force=call.data[ATTR_FORCE], base_url=call.data.get(ATTR_BASE_URL)
                )
            except HomeAssistantError as err:
                errors.append(f"{hub.entry.title}: {err}")
        if errors:
            raise HomeAssistantError("; ".join(errors))

    hass.services.async_register(DOMAIN, SERVICE_PERMIT_JOIN, permit_join, PERMIT_JOIN_SCHEMA)
    hass.services.async_register(
        DOMAIN, SERVICE_UPDATE_CATALOG, update_catalog, UPDATE_CATALOG_SCHEMA
    )
    hass.services.async_register(DOMAIN, SERVICE_REINTERVIEW, reinterview, DEVICE_SCHEMA)
    hass.services.async_register(DOMAIN, SERVICE_RECONFIGURE, reconfigure, DEVICE_SCHEMA)
    return True


async def async_setup_entry(hass: HomeAssistant, entry: ZigbeeRsHubConfigEntry) -> bool:
    """Connect to the hub and set up the platforms."""
    hub = ZigbeeRsHub(hass, entry, async_get_clientsession(hass))
    try:
        await hub.async_connect()
    except InvalidAuth as err:
        raise ConfigEntryAuthFailed(str(err)) from err
    except HubError as err:
        await hub.async_stop()
        raise ConfigEntryNotReady(
            f"zigbee-rs hub at {entry.data[CONF_HOST]}:{entry.data[CONF_PORT]}: {err}"
        ) from err
    entry.runtime_data = hub
    _adopt_coordinator_ieee(hass, entry, hub)
    entry.async_on_unload(hub.async_stop)
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    for cancel in hub.catalog.async_start():
        entry.async_on_unload(cancel)

    @callback
    def time_zone_changed(_event: Event) -> None:
        entry.async_create_task(hass, hub.async_sync_time_zone(), f"{DOMAIN} time zone")

    async def options_updated(_hass: HomeAssistant, _entry: ConfigEntry) -> None:
        await hub.async_sync_time_zone()

    entry.async_on_unload(hass.bus.async_listen(EVENT_CORE_CONFIG_UPDATE, time_zone_changed))
    entry.async_on_unload(entry.add_update_listener(options_updated))
    return True


@callback
def _adopt_coordinator_ieee(
    hass: HomeAssistant, entry: ZigbeeRsHubConfigEntry, hub: ZigbeeRsHub
) -> None:
    """Entries made against a hub without `hub/info` move to the IEEE later."""
    ieee = hub.coordinator_ieee
    if ieee is None or entry.unique_id == ieee:
        return
    if any(
        e.unique_id == ieee
        for e in hass.config_entries.async_entries(DOMAIN)
        if e.entry_id != entry.entry_id
    ):
        _LOGGER.warning("Another entry already uses the hub %s", ieee)
        return
    hass.config_entries.async_update_entry(entry, unique_id=ieee)


async def async_unload_entry(hass: HomeAssistant, entry: ZigbeeRsHubConfigEntry) -> bool:
    """Unload the platforms and close the connection."""
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)


async def async_remove_config_entry_device(
    hass: HomeAssistant, entry: ZigbeeRsHubConfigEntry, device_entry: dr.DeviceEntry
) -> bool:
    """HA's "Delete device": remove it from the Zigbee network and the hub."""
    hub = entry.runtime_data
    if hub.hub_identifier in device_entry.identifiers:
        return False
    for domain, ident in device_entry.identifiers:
        if domain != DOMAIN:
            continue
        if ident not in hub.devices:
            # Already gone from the hub: only the HA entry is left.
            return True
        try:
            await hub.async_remove_device(ident)
        except HomeAssistantError as err:
            _LOGGER.error("Could not remove %s: %s", ident, err)
            return False
        return True
    return True
