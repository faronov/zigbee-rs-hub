"""Config flow: user step, errors, duplicates, reauth, reconfigure."""

from __future__ import annotations

import socket

from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub.const import CONF_PATH, DOMAIN
from homeassistant import config_entries
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType

from .fake_hub import COORDINATOR_IEEE, TOKEN, FakeHub


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def _start(hass: HomeAssistant) -> dict:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": config_entries.SOURCE_USER}
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "user"
    return result


async def test_user_flow_creates_entry(hass: HomeAssistant, fake_hub: FakeHub) -> None:
    result = await _start(hass)
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: " 127.0.0.1 ", CONF_PORT: fake_hub.port, CONF_TOKEN: TOKEN},
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    assert result["title"] == "zigbee-rs hub (127.0.0.1)"
    assert result["data"] == {
        CONF_HOST: "127.0.0.1",
        CONF_PORT: fake_hub.port,
        CONF_TOKEN: TOKEN,
        CONF_PATH: "/api",
    }
    assert result["result"].unique_id == COORDINATOR_IEEE
    await hass.async_block_till_done()
    await hass.config_entries.async_unload(result["result"].entry_id)


async def test_user_flow_without_hub_info(hass: HomeAssistant, fake_hub: FakeHub) -> None:
    """Hubs before `hub/info` are identified by their address."""
    fake_hub.hub_info = False
    result = await _start(hass)
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: "127.0.0.1", CONF_PORT: fake_hub.port, CONF_TOKEN: TOKEN},
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    assert result["result"].unique_id == f"127.0.0.1:{fake_hub.port}"
    await hass.async_block_till_done()
    await hass.config_entries.async_unload(result["result"].entry_id)


async def test_user_flow_errors_then_success(hass: HomeAssistant, fake_hub: FakeHub) -> None:
    result = await _start(hass)
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: "127.0.0.1", CONF_PORT: fake_hub.port, CONF_TOKEN: "wrong"},
    )
    assert result["type"] is FlowResultType.FORM
    assert result["errors"] == {"base": "invalid_auth"}

    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: "127.0.0.1", CONF_PORT: _free_port(), CONF_TOKEN: TOKEN},
    )
    assert result["type"] is FlowResultType.FORM
    assert result["errors"] == {"base": "cannot_connect"}

    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: "127.0.0.1", CONF_PORT: fake_hub.port, CONF_TOKEN: TOKEN},
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    await hass.config_entries.async_unload(result["result"].entry_id)


async def test_user_flow_already_configured_updates_address(
    hass: HomeAssistant, fake_hub: FakeHub
) -> None:
    entry = MockConfigEntry(
        domain=DOMAIN,
        unique_id=COORDINATOR_IEEE,
        data={CONF_HOST: "10.0.0.9", CONF_PORT: 80, CONF_TOKEN: TOKEN, CONF_PATH: "/api"},
    )
    entry.add_to_hass(hass)
    result = await _start(hass)
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: "127.0.0.1", CONF_PORT: fake_hub.port, CONF_TOKEN: TOKEN},
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "already_configured"
    assert entry.data[CONF_HOST] == "127.0.0.1"
    assert entry.data[CONF_PORT] == fake_hub.port


async def test_reauth(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    """A changed token on the hub starts reauth; the new token is stored."""
    fake_hub.token = "rotated"
    config_entry.add_to_hass(hass)
    assert not await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    assert config_entry.state is config_entries.ConfigEntryState.SETUP_ERROR
    flows = hass.config_entries.flow.async_progress_by_handler(DOMAIN)
    assert len(flows) == 1
    assert flows[0]["step_id"] == "reauth_confirm"

    result = await hass.config_entries.flow.async_configure(
        flows[0]["flow_id"], {CONF_TOKEN: "still-wrong"}
    )
    assert result["errors"] == {"base": "invalid_auth"}
    result = await hass.config_entries.flow.async_configure(
        flows[0]["flow_id"], {CONF_TOKEN: "rotated"}
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "reauth_successful"
    assert config_entry.data[CONF_TOKEN] == "rotated"
    await hass.async_block_till_done()
    assert config_entry.state is config_entries.ConfigEntryState.LOADED
    await hass.config_entries.async_unload(config_entry.entry_id)


async def test_reconfigure(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    other = FakeHub()
    await other.start()
    try:
        result = await setup_entry.start_reconfigure_flow(hass)
        assert result["step_id"] == "reconfigure"
        result = await hass.config_entries.flow.async_configure(
            result["flow_id"],
            {CONF_HOST: "127.0.0.1", CONF_PORT: other.port, CONF_TOKEN: TOKEN},
        )
        assert result["type"] is FlowResultType.ABORT
        assert result["reason"] == "reconfigure_successful"
        assert setup_entry.data[CONF_PORT] == other.port
        await hass.async_block_till_done()
        assert other.connections == 2  # probe + set-up
    finally:
        await hass.config_entries.async_unload(setup_entry.entry_id)
        await other.stop()
