"""End to end against the real API server: `hubctl --sim serve`.

Runs when `HUBCTL` names a hubctl binary (CI builds one); skipped otherwise.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncGenerator
import os
import socket

import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub.const import CONF_PATH, DOMAIN
from custom_components.zigbee_rs_hub.hub import find_device
from homeassistant.config_entries import ConfigEntryState
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN, STATE_OFF, STATE_ON
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr, entity_registry as er

HUBCTL = os.environ.get("HUBCTL")
SIM_COORDINATOR_IEEE = "020100000000515a"

pytestmark = pytest.mark.skipif(not HUBCTL, reason="HUBCTL is not set")


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Hubctl:
    """`hubctl --sim serve` as a subprocess."""

    def __init__(self, port: int) -> None:
        self.port = port
        self.proc: asyncio.subprocess.Process | None = None

    async def start(self) -> None:
        assert HUBCTL
        self.proc = await asyncio.create_subprocess_exec(
            HUBCTL,
            "--sim",
            "serve",
            "--listen",
            f"127.0.0.1:{self.port}",
            "--token",
            "e2e",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        async with asyncio.timeout(30):
            while True:
                try:
                    _, writer = await asyncio.open_connection("127.0.0.1", self.port)
                except OSError:
                    await asyncio.sleep(0.1)
                    continue
                writer.close()
                await writer.wait_closed()
                return

    async def stop(self) -> None:
        if self.proc is not None and self.proc.returncode is None:
            self.proc.terminate()
            await self.proc.wait()
        self.proc = None


@pytest.fixture
async def hubctl() -> AsyncGenerator[Hubctl]:
    server = Hubctl(_free_port())
    await server.start()
    yield server
    await server.stop()


async def _wait(hass: HomeAssistant, cond, timeout: float = 20.0) -> None:
    async with asyncio.timeout(timeout):
        while not cond():
            await asyncio.sleep(0.05)
            await hass.async_block_till_done()


async def test_hubctl_sim(hass: HomeAssistant, hubctl: Hubctl) -> None:
    result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": "user"})
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: "127.0.0.1", CONF_PORT: hubctl.port, CONF_TOKEN: "e2e"},
    )
    assert result["type"] == "create_entry"
    entry: MockConfigEntry = result["result"]
    assert entry.unique_id == SIM_COORDINATOR_IEEE
    assert entry.data[CONF_PATH] == "/api"
    await hass.async_block_till_done()
    assert entry.state is ConfigEntryState.LOADED

    hub_dev = find_device(dr.async_get(hass), (DOMAIN, f"hub_{entry.entry_id}"), entry.entry_id)
    assert hub_dev.model == "sim-ncp"
    assert hub_dev.serial_number == SIM_COORDINATOR_IEEE
    connected = er.async_get(hass).async_get_entity_id(
        "binary_sensor", DOMAIN, f"{entry.entry_id}_connected"
    )
    assert hass.states.get(connected).state == STATE_ON

    await hass.services.async_call(DOMAIN, "permit_join", {"duration": 30}, blocking=True)
    # API 1.1: the network topic reports the open window.
    reg = er.async_get(hass)
    permit = reg.async_get_entity_id("binary_sensor", DOMAIN, f"{entry.entry_id}_permit_join_open")
    assert permit is not None
    await _wait(hass, lambda: hass.states.get(permit).state == STATE_ON)
    assert reg.async_get_entity_id("sensor", DOMAIN, f"{entry.entry_id}_network_state")

    # The hub process restarts: unavailable, then reconnected.
    await hubctl.stop()
    await _wait(hass, lambda: hass.states.get(connected).state == STATE_OFF)
    await hubctl.start()
    await _wait(hass, lambda: hass.states.get(connected).state == STATE_ON)
    await hass.services.async_call(DOMAIN, "permit_join", {"duration": 0}, blocking=True)
    await _wait(hass, lambda: hass.states.get(permit).state == STATE_OFF)

    await hass.config_entries.async_unload(entry.entry_id)
    await hass.async_block_till_done()
