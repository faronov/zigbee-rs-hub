"""End to end against the real API server: `hubctl --sim serve`.

Runs when `HUBCTL` names a hubctl binary (CI builds one); skipped otherwise.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncGenerator
import os
from pathlib import Path
import socket

import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub.const import CONF_PATH, DOMAIN
from custom_components.zigbee_rs_hub.hub import find_device
from homeassistant.config_entries import ConfigEntryState
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN, STATE_OFF, STATE_ON
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr, entity_registry as er

from .fake_hub import ReleaseServer

HUBCTL = os.environ.get("HUBCTL")
# zdefc builds the test catalog release (next to hubctl by default).
ZDEFC = os.environ.get("ZDEFC") or (str(Path(HUBCTL).with_name("zdefc")) if HUBCTL else None)
REPO = Path(__file__).resolve().parents[3]
SIM_COORDINATOR_IEEE = "020100000000515a"

pytestmark = pytest.mark.skipif(not HUBCTL, reason="HUBCTL is not set")


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Hubctl:
    """`hubctl --sim serve` as a subprocess."""

    def __init__(self, port: int, *extra: str) -> None:
        self.port = port
        self.extra = extra
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
            *self.extra,
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


async def _run(*args: str) -> str:
    proc = await asyncio.create_subprocess_exec(
        *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
    )
    out, _ = await proc.communicate()
    assert proc.returncode == 0, out.decode()
    return out.decode()


@pytest.mark.skipif(not ZDEFC or not Path(ZDEFC).exists(), reason="no zdefc binary")
async def test_hubctl_catalog_update(hass: HomeAssistant, tmp_path: Path) -> None:
    """A signed release built by zdefc, served over HTTP, installed through HA."""
    assert ZDEFC
    key = tmp_path / "test.key"
    await _run(ZDEFC, "bank", "keygen", "-o", str(key))
    out = await _run(ZDEFC, "bank", "pubkey", "--key", str(key))
    pubkey = next(line.split()[-1] for line in out.splitlines() if line.startswith("public key"))
    rel = tmp_path / "release"
    await _run(
        ZDEFC,
        "bank",
        "build",
        "-o",
        str(rel),
        "--key",
        str(key),
        "--date",
        "20261121",
        "--serial",
        "3",
        "--name",
        "bank-4m.zdb",
        str(REPO / "definitions" / "examples"),
    )
    release = ReleaseServer({p.name: p.read_bytes() for p in rel.iterdir() if p.is_file()})
    await release.start()
    hub = Hubctl(_free_port(), "--catalog-key", pubkey)
    await hub.start()
    try:
        entry = MockConfigEntry(
            domain=DOMAIN,
            title="hubctl",
            data={
                CONF_HOST: "127.0.0.1",
                CONF_PORT: hub.port,
                CONF_TOKEN: "e2e",
                CONF_PATH: "/api",
            },
        )
        entry.add_to_hass(hass)
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        sensor = er.async_get(hass).async_get_entity_id(
            "sensor", DOMAIN, f"{entry.entry_id}_catalog_version"
        )
        assert sensor is not None
        assert hass.states.get(sensor).state == "none"

        await entry.runtime_data.async_request("catalog/config", base_url=release.base_url)
        await hass.services.async_call(
            DOMAIN, "update_catalog", {"config_entry_id": entry.entry_id}, blocking=True
        )
        await hass.async_block_till_done()
        assert release.requests[:2] == ["manifest.cose", "bank-4m.zdb"]
        assert hass.states.get(sensor).state == "20261121.3"
        status = await entry.runtime_data.async_request("catalog/status")
        assert status["installed"]["definitions"] > 0
        assert status["wants"] == {"bank": None, "web": None}

        # A second run: the same manifest, nothing to install.
        await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
        assert release.requests.count("bank-4m.zdb") == 1

        await hass.config_entries.async_unload(entry.entry_id)
        await hass.async_block_till_done()
    finally:
        await hub.stop()
        await release.stop()


PLUG1 = "zb:5a52530000000001:1:switch"
PLUG2 = "zb:5a52530000000002:1:switch"


async def test_hubctl_automations(hass: HomeAssistant) -> None:
    """A hub automation on simulated plugs, its switch and run button in HA."""
    hub = Hubctl(_free_port(), "--sim-devices")
    await hub.start()
    try:
        entry = MockConfigEntry(
            domain=DOMAIN,
            title="hubctl",
            data={
                CONF_HOST: "127.0.0.1",
                CONF_PORT: hub.port,
                CONF_TOKEN: "e2e",
                CONF_PATH: "/api",
            },
        )
        entry.add_to_hass(hass)
        assert await hass.config_entries.async_setup(entry.entry_id)
        await hass.async_block_till_done()
        reg = er.async_get(hass)
        await _wait(hass, lambda: reg.async_get_entity_id("switch", DOMAIN, PLUG2) is not None)
        plug1 = reg.async_get_entity_id("switch", DOMAIN, PLUG1)
        plug2 = reg.async_get_entity_id("switch", DOMAIN, PLUG2)
        await _wait(hass, lambda: hass.states.get(plug2).state == STATE_OFF)

        # Plug 2 follows plug 1, on the hub.
        await entry.runtime_data.async_request(
            "automations/create",
            rule={
                "id": "follow",
                "name": "Follow",
                "triggers": [{"type": "state", "uid": PLUG1, "op": "changed"}],
                "actions": [{"type": "command", "uid": PLUG2, "command": "toggle"}],
            },
        )
        uid = f"{entry.entry_id}_automation_follow"
        await _wait(hass, lambda: reg.async_get_entity_id("switch", DOMAIN, uid) is not None)
        auto = reg.async_get_entity_id("switch", DOMAIN, uid)
        await _wait(hass, lambda: (s := hass.states.get(auto)) is not None and s.state == STATE_ON)
        await hass.services.async_call("switch", "turn_on", {"entity_id": plug1}, blocking=True)
        await _wait(hass, lambda: hass.states.get(plug2).state == STATE_ON)
        await _wait(hass, lambda: hass.states.get(auto).attributes["last_result"] == "done")

        # Disabled from HA: plug 2 stays.
        await hass.services.async_call("switch", "turn_off", {"entity_id": auto}, blocking=True)
        listed = await entry.runtime_data.async_request("automations/get", automation="follow")
        assert listed["enabled"] is False
        await hass.services.async_call("switch", "turn_off", {"entity_id": plug1}, blocking=True)
        await _wait(hass, lambda: hass.states.get(plug1).state == STATE_OFF)
        await asyncio.sleep(0.5)
        assert hass.states.get(plug2).state == STATE_ON

        # The run button: a test run toggles plug 2 even while disabled.
        run = reg.async_get_entity_id("button", DOMAIN, f"{uid}_run")
        await hass.services.async_call("button", "press", {"entity_id": run}, blocking=True)
        await _wait(hass, lambda: hass.states.get(plug2).state == STATE_OFF)

        await entry.runtime_data.async_request("automations/delete", automation="follow")
        await _wait(hass, lambda: reg.async_get_entity_id("switch", DOMAIN, uid) is None)

        await hass.config_entries.async_unload(entry.entry_id)
        await hass.async_block_till_done()
    finally:
        await hub.stop()
