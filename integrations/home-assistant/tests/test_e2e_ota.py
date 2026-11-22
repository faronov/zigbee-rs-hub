"""End to end OTA: `hubctl --sim --sim-ota serve` (a simulated device with an
OTA client), an image uploaded with `hubctl ota-upload`, installed from HA.

Runs when `HUBCTL` names a hubctl binary (CI builds one); skipped otherwise.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncGenerator
import os
from pathlib import Path
import socket

import pytest

from custom_components.zigbee_rs_hub.const import DOMAIN
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN, STATE_OFF, STATE_ON
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er

HUBCTL = os.environ.get("HUBCTL")
SIM_DEVICE = "zb:6d6b000000004f01"

pytestmark = pytest.mark.skipif(not HUBCTL, reason="HUBCTL is not set")


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def _run(*args: str) -> None:
    assert HUBCTL
    proc = await asyncio.create_subprocess_exec(
        HUBCTL, *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
    )
    out, _ = await proc.communicate()
    assert proc.returncode == 0, out.decode()


@pytest.fixture
async def hub_port() -> AsyncGenerator[int]:
    assert HUBCTL
    port = _free_port()
    proc = await asyncio.create_subprocess_exec(
        HUBCTL,
        "--sim",
        "--sim-ota",
        "serve",
        "--listen",
        f"127.0.0.1:{port}",
        "--token",
        "e2e",
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    async with asyncio.timeout(30):
        while True:
            try:
                _, writer = await asyncio.open_connection("127.0.0.1", port)
            except OSError:
                await asyncio.sleep(0.1)
                continue
            writer.close()
            await writer.wait_closed()
            break
    yield port
    proc.terminate()
    await proc.wait()


async def _wait(hass: HomeAssistant, cond, timeout: float = 30.0) -> None:
    async with asyncio.timeout(timeout):
        while not cond():
            await asyncio.sleep(0.05)
            await hass.async_block_till_done()


async def test_ota_upload_and_install(hass: HomeAssistant, hub_port: int, tmp_path: Path) -> None:
    result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": "user"})
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_HOST: "127.0.0.1", CONF_PORT: hub_port, CONF_TOKEN: "e2e"}
    )
    assert result["type"] == "create_entry"
    await hass.async_block_till_done()

    # The simulated device joins and queries after a few seconds.
    reg = er.async_get(hass)
    await _wait(hass, lambda: reg.async_get_entity_id("update", DOMAIN, f"{SIM_DEVICE}:ota"))
    entity_id = reg.async_get_entity_id("update", DOMAIN, f"{SIM_DEVICE}:ota")
    await _wait(hass, lambda: hass.states.get(entity_id).attributes.get("installed_version"))
    assert hass.states.get(entity_id).attributes["installed_version"] == "0x00010000"
    assert hass.states.get(entity_id).state == STATE_OFF

    image = tmp_path / "fw.bin"
    image.write_bytes(bytes(range(256)) * 12)
    ota = tmp_path / "sim.ota"
    await _run(
        "ota-make",
        str(image),
        str(ota),
        "--manufacturer",
        "0x6D6B",
        "--image-type",
        "0x0102",
        "--version",
        "0x00010100",
        "--label",
        "sim 1.1",
    )
    await _run("ota-upload", f"ws://127.0.0.1:{hub_port}/api", "--token", "e2e", str(ota))
    await _wait(hass, lambda: hass.states.get(entity_id).state == STATE_ON)
    assert hass.states.get(entity_id).attributes["latest_version"] == "0x00010100"
    assert hass.states.get(entity_id).attributes["release_summary"] == "sim 1.1"

    await hass.services.async_call("update", "install", {"entity_id": entity_id}, blocking=True)
    await _wait(hass, lambda: hass.states.get(entity_id).state == STATE_OFF, timeout=60)
    st = hass.states.get(entity_id)
    assert st.attributes["installed_version"] == "0x00010100"
    assert st.attributes["in_progress"] is False
