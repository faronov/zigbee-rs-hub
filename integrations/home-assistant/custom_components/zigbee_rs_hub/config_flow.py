"""Config flow: host, port and API token of a zigbee-rs hub."""

from __future__ import annotations

from collections.abc import Mapping
import logging
from typing import Any

import voluptuous as vol

from homeassistant.config_entries import (
    ConfigEntry,
    ConfigFlow,
    ConfigFlowResult,
    OptionsFlow,
)
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN
from homeassistant.core import callback
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .api import CannotConnect, HubClient, HubError, InvalidAuth, hub_url
from .const import CONF_PATH, CONF_SYNC_TIME_ZONE, DEFAULT_PATH, DEFAULT_PORT, DOMAIN
from .hub import async_fetch_info

_LOGGER = logging.getLogger(__name__)


def _schema(defaults: Mapping[str, Any], with_token: bool = True) -> vol.Schema:
    fields: dict[Any, Any] = {
        vol.Required(CONF_HOST, default=defaults.get(CONF_HOST, "")): str,
        vol.Required(CONF_PORT, default=defaults.get(CONF_PORT, DEFAULT_PORT)): vol.All(
            vol.Coerce(int), vol.Range(min=1, max=65535)
        ),
    }
    if with_token:
        fields[vol.Required(CONF_TOKEN, default=defaults.get(CONF_TOKEN, ""))] = str
    return vol.Schema(fields)


class ZigbeeRsHubConfigFlow(ConfigFlow, domain=DOMAIN):
    """Manual set-up (the hub does not advertise itself over mDNS yet)."""

    VERSION = 1

    @staticmethod
    @callback
    def async_get_options_flow(config_entry: ConfigEntry) -> OptionsFlow:
        """Options of an entry."""
        return ZigbeeRsHubOptionsFlow()

    async def _probe(self, data: Mapping[str, Any]) -> dict[str, Any]:
        """Connect, authenticate and read the identity; raises `HubError`."""
        client = HubClient(
            async_get_clientsession(self.hass),
            hub_url(data[CONF_HOST], data[CONF_PORT], data.get(CONF_PATH, DEFAULT_PATH)),
            data[CONF_TOKEN],
        )
        await client.connect()
        try:
            return await async_fetch_info(client)
        finally:
            await client.close()

    async def _try(self, data: Mapping[str, Any], errors: dict[str, str]) -> dict[str, Any] | None:
        try:
            return await self._probe(data)
        except InvalidAuth:
            errors["base"] = "invalid_auth"
        except CannotConnect:
            errors["base"] = "cannot_connect"
        except HubError:
            _LOGGER.exception("Unexpected answer from the hub")
            errors["base"] = "unknown"
        return None

    @staticmethod
    def _unique_id(data: Mapping[str, Any], info: Mapping[str, Any]) -> str:
        return info.get("coordinator_ieee") or f"{data[CONF_HOST]}:{data[CONF_PORT]}"

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        """Host, port and token."""
        errors: dict[str, str] = {}
        if user_input is not None:
            user_input[CONF_HOST] = user_input[CONF_HOST].strip()
            info = await self._try(user_input, errors)
            if info is not None:
                await self.async_set_unique_id(self._unique_id(user_input, info))
                self._abort_if_unique_id_configured(
                    updates={CONF_HOST: user_input[CONF_HOST], CONF_PORT: user_input[CONF_PORT]}
                )
                return self.async_create_entry(
                    title=f"zigbee-rs hub ({user_input[CONF_HOST]})",
                    data={**user_input, CONF_PATH: DEFAULT_PATH},
                    options={CONF_SYNC_TIME_ZONE: True},
                )
        return self.async_show_form(
            step_id="user", data_schema=_schema(user_input or {}), errors=errors
        )

    async def async_step_reauth(self, entry_data: Mapping[str, Any]) -> ConfigFlowResult:
        """The hub refused the stored token."""
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        """Ask for a new token."""
        entry = self._get_reauth_entry()
        errors: dict[str, str] = {}
        if user_input is not None:
            data = {**entry.data, CONF_TOKEN: user_input[CONF_TOKEN]}
            if await self._try(data, errors) is not None:
                return self.async_update_reload_and_abort(entry, data=data)
        return self.async_show_form(
            step_id="reauth_confirm",
            data_schema=vol.Schema({vol.Required(CONF_TOKEN): str}),
            errors=errors,
            description_placeholders={"host": entry.data[CONF_HOST]},
        )

    async def async_step_reconfigure(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        """New address or token of the same hub."""
        entry = self._get_reconfigure_entry()
        errors: dict[str, str] = {}
        if user_input is not None:
            data = {**entry.data, **user_input, CONF_HOST: user_input[CONF_HOST].strip()}
            info = await self._try(data, errors)
            if info is not None:
                ieee = info.get("coordinator_ieee")
                if ieee and entry.unique_id and ":" not in entry.unique_id:
                    await self.async_set_unique_id(ieee)
                    self._abort_if_unique_id_mismatch(reason="wrong_hub")
                return self.async_update_reload_and_abort(entry, data=data)
        return self.async_show_form(
            step_id="reconfigure",
            data_schema=_schema(user_input or entry.data),
            errors=errors,
        )


class ZigbeeRsHubOptionsFlow(OptionsFlow):
    """Whether the hub takes Home Assistant's time zone."""

    async def async_step_init(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        """The options form."""
        if user_input is not None:
            return self.async_create_entry(data=user_input)
        current = self.config_entry.options.get(CONF_SYNC_TIME_ZONE, False)
        return self.async_show_form(
            step_id="init",
            data_schema=vol.Schema({vol.Required(CONF_SYNC_TIME_ZONE, default=current): bool}),
        )
