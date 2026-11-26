"""End to end against the real API server: `hubctl --sim serve`.

Runs when `HUBCTL` names a hubctl binary (CI builds one); skipped otherwise.
`HUBCTL_BACKEND=ezsp` runs the hub on the EZSP backend's simulated NCP
(`hubctl --backend ezsp --sim`) instead of the ZRHP one.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncGenerator
import itertools
import json
import os
from pathlib import Path
import socket

import aiohttp
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
BACKEND = os.environ.get("HUBCTL_BACKEND", "zrhp")
# The simulated NCP's IEEE address and product name, per backend.
SIM_COORDINATOR_IEEE, SIM_NCP = {
    "zrhp": ("020100000000515a", "sim-ncp"),
    "ezsp": ("000b57fffe123456", "hub-ezsp sim"),
}[BACKEND]

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

    async def start(self, ready: bool = False) -> None:
        """Until the API accepts connections; `ready`: and the hub knows its
        coordinator (the first NCP session is up), which the config flow's
        unique ID needs. The EZSP backend's session comes up after the API
        listens."""
        assert HUBCTL
        self.proc = await asyncio.create_subprocess_exec(
            HUBCTL,
            "--backend",
            BACKEND,
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
                break
            if ready:
                await self._coordinator_known()

    async def _coordinator_known(self) -> None:
        async with (
            aiohttp.ClientSession() as session,
            session.ws_connect(f"http://127.0.0.1:{self.port}/api") as ws,
        ):
            await ws.send_json({"type": "auth", "token": "e2e"})
            for i in itertools.count(1):
                await ws.send_json({"id": i, "type": "hub/info"})
                while (msg := await ws.receive_json()).get("id") != i:
                    pass
                if msg["result"].get("coordinator_ieee"):
                    return
                await asyncio.sleep(0.05)

    async def stop(self) -> None:
        if self.proc is not None and self.proc.returncode is None:
            self.proc.terminate()
            await self.proc.wait()
        self.proc = None


@pytest.fixture
async def hubctl() -> AsyncGenerator[Hubctl]:
    server = Hubctl(_free_port())
    await server.start(ready=True)
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
    assert hub_dev.model == SIM_NCP
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


async def test_hubctl_import_from_zha(hass: HomeAssistant, hubctl: Hubctl, tmp_path: Path) -> None:
    """ZHA's zigbee.db (generated) previewed and imported on the real hub."""
    from .test_migration import COORD, ZIGPY_BACKUP, make_db

    path = tmp_path / "zigbee.db"
    make_db(path, 17)
    # The backup of a network whose coordinator is this radio (no IEEE write).
    backup = json.loads(
        json.dumps(ZIGPY_BACKUP).replace(
            COORD, ":".join(SIM_COORDINATOR_IEEE[i : i + 2] for i in range(0, 16, 2))
        )
    )
    (tmp_path / "backup.json").write_text(json.dumps(backup))
    result = await hass.config_entries.flow.async_init(DOMAIN, context={"source": "user"})
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_HOST: "127.0.0.1", CONF_PORT: hubctl.port, CONF_TOKEN: "e2e"},
    )
    entry: MockConfigEntry = result["result"]
    await hass.async_block_till_done()
    data = {"database": str(path), "backup": str(tmp_path / "backup.json")}
    preview = await hass.services.async_call(
        DOMAIN, "import_from_zha", data, blocking=True, return_response=True
    )
    assert preview["result"]["network"]["ext_pan_id"] == "a1b2c3d4e5f60718"
    assert preview["result"]["network"]["coordinator"] == "same"
    assert preview["result"]["summary"]["new"] == 2, preview
    rows = {r["ieee"]: r for r in preview["rows"]}
    assert rows["0017880100f1e2d3"]["status"] == "new"
    assert rows["00000000017880ff"]["reason"] == "green_power"
    applied = await hass.services.async_call(
        DOMAIN,
        "import_from_zha",
        {**data, "apply": True, "force": True, "confirm": True},
        blocking=True,
        return_response=True,
    )
    res = applied["result"]
    assert res["network"]["action"] == "restored", res
    assert res["devices"]["new"] == 2 and not res["coordinator_changed"]
    reg = dr.async_get(hass)
    await _wait(hass, lambda: find_device(reg, (DOMAIN, "zb:0017880100f1e2d3"), entry.entry_id))
    light = find_device(reg, (DOMAIN, "zb:0017880100f1e2d3"), entry.entry_id)
    assert light.model == "LCT015"
    await hass.config_entries.async_unload(entry.entry_id)
    await hass.async_block_till_done()
