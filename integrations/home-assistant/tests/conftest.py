"""Fixtures: the fake hub and a config entry pointing at it."""

from __future__ import annotations

from collections.abc import AsyncGenerator
from unittest.mock import patch

import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub.const import CONF_PATH, DOMAIN
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN
from homeassistant.core import HomeAssistant

from .fake_hub import COORDINATOR_IEEE, TOKEN, FakeHub


@pytest.fixture(autouse=True)
def auto_enable_custom_integrations(enable_custom_integrations: None) -> None:
    """Load integrations from custom_components."""


@pytest.fixture(autouse=True)
def local_sockets(socket_enabled: None) -> None:
    """The fake hub listens on 127.0.0.1 (HA's plugin allows only that host)."""


@pytest.fixture(autouse=True)
def fast_reconnect() -> None:
    """Reconnect without the production back-off."""
    with (
        patch("custom_components.zigbee_rs_hub.const.RECONNECT_MIN", 0.05),
        patch("custom_components.zigbee_rs_hub.const.RECONNECT_MAX", 0.2),
    ):
        yield


@pytest.fixture
async def fake_hub() -> AsyncGenerator[FakeHub]:
    """A running fake hub."""
    hub = FakeHub()
    await hub.start()
    yield hub
    await hub.stop()


@pytest.fixture
def config_entry(fake_hub: FakeHub) -> MockConfigEntry:
    """An entry for the fake hub."""
    return MockConfigEntry(
        domain=DOMAIN,
        title="zigbee-rs hub (127.0.0.1)",
        unique_id=COORDINATOR_IEEE,
        data={
            CONF_HOST: "127.0.0.1",
            CONF_PORT: fake_hub.port,
            CONF_TOKEN: TOKEN,
            CONF_PATH: "/api",
        },
    )


@pytest.fixture
async def setup_entry(
    hass: HomeAssistant, config_entry: MockConfigEntry
) -> AsyncGenerator[MockConfigEntry]:
    """The entry, set up; unloaded at the end."""
    config_entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    yield config_entry
    await hass.config_entries.async_unload(config_entry.entry_id)
    await hass.async_block_till_done()
