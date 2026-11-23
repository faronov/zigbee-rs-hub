"""The `update` platform: device firmware through the hub's OTA server."""

from __future__ import annotations

import base64
import hashlib
import os
from typing import Any

import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from homeassistant.const import STATE_OFF, STATE_ON
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import entity_registry as er

from .fake_hub import LIGHT_DEV, SENSOR_DEV, FakeHub
from .test_init import wait_for

ENTITY = "update.motion_sensor_firmware"


def record(device: str, current: int | None, latest: int | None = None, job: Any = None) -> dict:
    """An `ota/devices/list` record."""
    return {
        "device": device,
        "ep": 1,
        "manufacturer_code": 0x115F,
        "image_type": 0x0102,
        "current_version": current,
        "hw_version": None,
        "policy": None,
        "effective_policy": "manual",
        "available": None
        if latest is None
        else {
            "image": 3,
            "file_version": latest,
            "size": 250000,
            "label": "Motion 1.1",
            "name": "m.ota",
        },
        "job": job,
        "last_query": 1795098123,
        "name": "Motion sensor",
        "sleepy": True,
        "available_device": True,
    }


async def _setup(hass: HomeAssistant, entry: MockConfigEntry) -> None:
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()


async def test_update_entity_install_and_progress(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    fake_hub.ota = {SENSOR_DEV: record(SENSOR_DEV, 0x01000000, 0x01000100)}
    await _setup(hass, config_entry)
    assert "ota" in next(r for r in fake_hub.requests if r["type"] == "subscribe")["topics"]
    st = hass.states.get(ENTITY)
    assert st is not None
    assert st.state == STATE_ON
    assert st.attributes["installed_version"] == "0x01000000"
    assert st.attributes["latest_version"] == "0x01000100"
    assert st.attributes["release_summary"] == "Motion 1.1"
    assert st.attributes["in_progress"] is False
    ent = er.async_get(hass).async_get(ENTITY)
    assert ent is not None and ent.unique_id == f"{SENSOR_DEV}:ota"

    await hass.services.async_call("update", "install", {"entity_id": ENTITY}, blocking=True)
    assert fake_hub.calls("ota/start") == [
        {"id": fake_hub.calls("ota/start")[0]["id"], "type": "ota/start", "device": SENSOR_DEV}
    ]

    job = {"state": "downloading", "progress": 40, "offset": 100000, "size": 250000, "manual": True}
    await fake_hub.push(
        "ota", {"topic": "ota_device", "device": record(SENSOR_DEV, 0x01000000, 0x01000100, job)}
    )
    await wait_for(hass, lambda: hass.states.get(ENTITY).attributes["in_progress"] is True)
    st = hass.states.get(ENTITY)
    assert st.attributes["update_percentage"] == 40

    done = {"state": "done", "progress": 100, "offset": 250000, "size": 250000, "manual": True}
    await fake_hub.push(
        "ota", {"topic": "ota_device", "device": record(SENSOR_DEV, 0x01000100, None, done)}
    )
    await wait_for(hass, lambda: hass.states.get(ENTITY).state == STATE_OFF)
    st = hass.states.get(ENTITY)
    assert st.attributes["installed_version"] == "0x01000100"
    assert st.attributes["in_progress"] is False

    # A device that shows up later gets its entity too.
    await fake_hub.push("ota", {"topic": "ota_device", "device": record(LIGHT_DEV, 7)})
    await wait_for(hass, lambda: hass.states.get("update.dimmer_firmware") is not None)
    assert hass.states.get("update.dimmer_firmware").state == STATE_OFF


async def test_install_from_the_ota_index(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    ota = b"\x1e\xf1\xee\x0b" + bytes(range(256)) * 20
    fake_hub.files["m.ota"] = b"VENDORHDR" + ota
    fake_hub.ota = {SENSOR_DEV: record(SENSOR_DEV, 0x01000000, 0x01000100)}
    fake_hub.ota_index = [
        {
            "fileName": "m.ota",
            "fileVersion": 0x01000200,
            "fileSize": len(ota) + 9,
            "url": f"http://127.0.0.1:{fake_hub.port}/files/m.ota",
            "imageType": 0x0102,
            "manufacturerCode": 0x115F,
            "sha512": hashlib.sha512(fake_hub.files["m.ota"]).hexdigest(),
            "minFileVersion": 0x01000000,
        },
        # Another device type, and one too old for the device.
        {"fileVersion": 0x09000000, "imageType": 0x0999, "manufacturerCode": 0x115F, "url": "x"},
        {
            "fileVersion": 0x01000300,
            "imageType": 0x0102,
            "manufacturerCode": 0x115F,
            "url": "y",
            "maxFileVersion": 1,
        },
    ]
    await _setup(hass, config_entry)
    # Fetched a minute after set-up, or now.
    await config_entry.runtime_data.async_refresh_ota_index()
    await wait_for(
        hass, lambda: hass.states.get(ENTITY).attributes["latest_version"] == "0x01000200"
    )
    st = hass.states.get(ENTITY)
    assert st.state == STATE_ON
    assert st.attributes["release_summary"] == "m.ota (OTA index)"

    await hass.services.async_call("update", "install", {"entity_id": ENTITY}, blocking=True)
    # The vendor prefix is cut, the file uploaded, then installed by image ID.
    assert bytes(fake_hub.uploads[1]) == ota
    begin = fake_hub.calls("ota/upload/begin")[0]
    assert begin["sha256"] == hashlib.sha256(ota).hexdigest()
    assert begin["min_file_version"] == 0x01000000
    assert "max_file_version" not in begin
    assert fake_hub.calls("ota/start")[0]["image"] == 41

    # A wrong SHA-512 is refused before anything reaches the hub.
    fake_hub.ota_index[0]["sha512"] = "00"
    hub = config_entry.runtime_data
    await hub.async_refresh_ota_index(force=True)
    await hass.async_block_till_done()
    with pytest.raises(HomeAssistantError, match="SHA-512"):
        await hass.services.async_call("update", "install", {"entity_id": ENTITY}, blocking=True)
    assert len(fake_hub.calls("ota/upload/begin")) == 1


async def test_no_update_entities_without_the_ota_server(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    await _setup(hass, config_entry)
    assert not hass.states.async_entity_ids("update")
    assert "ota" not in next(r for r in fake_hub.requests if r["type"] == "subscribe")["topics"]
    assert not fake_hub.calls("ota/devices/list")


def ota_file(version: int) -> bytes:
    """A minimal Zigbee OTA file for the sensor's image type."""
    head = bytearray(56)
    head[0:4] = b"\x1e\xf1\xee\x0b"
    head[4:6] = (0x0100).to_bytes(2, "little")
    head[6:8] = (56).to_bytes(2, "little")
    head[10:12] = (0x115F).to_bytes(2, "little")
    head[12:14] = (0x0102).to_bytes(2, "little")
    head[14:18] = version.to_bytes(4, "little")
    body = bytes(range(256)) * 30
    total = 56 + len(body)
    head[52:56] = total.to_bytes(4, "little")
    return bytes(head) + body


async def test_streaming_to_a_hub_without_a_store(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    ota = ota_file(0x01000200)
    fake_hub.ota_store = False
    fake_hub.files["m.ota"] = ota
    fake_hub.ota = {SENSOR_DEV: record(SENSOR_DEV, 0x01000000)}
    fake_hub.ota_index = [
        {
            "fileName": "m.ota",
            "fileVersion": 0x01000200,
            "url": f"http://127.0.0.1:{fake_hub.port}/files/m.ota",
            "imageType": 0x0102,
            "manufacturerCode": 0x115F,
            "sha512": hashlib.sha512(ota).hexdigest(),
        }
    ]
    await _setup(hass, config_entry)
    hub = config_entry.runtime_data
    assert not hub.ota_store
    await hub.async_refresh_ota_index()
    await wait_for(hass, lambda: hass.states.get(ENTITY).state == STATE_ON)
    await hass.services.async_call("update", "install", {"entity_id": ENTITY}, blocking=True)
    # Nothing uploaded: offered as a stream, started, the first block served.
    assert not fake_hub.calls("ota/upload/begin")
    offer = fake_hub.calls("ota/stream/offer")[0]
    assert base64.b64decode(offer["header"]) == ota[:256]
    assert offer["size"] == len(ota)
    assert fake_hub.calls("ota/start")[0]["image"] == 0x1_0000
    await wait_for(hass, lambda: (0x1_0000, 0) in fake_hub.streamed)
    assert fake_hub.streamed[(0x1_0000, 0)] == ota[:4096]
    cached = hass.config.path(".storage", "zigbee_rs_hub_ota", "115f-0102-01000200.ota")
    assert os.path.exists(cached)  # noqa: ASYNC240

    # The hub restarts mid-download: Home Assistant offers the file again
    # and restarts the upgrade.
    job = {"state": "downloading", "file_version": 0x01000200, "image": None, "progress": 30}
    fake_hub.ota = {SENSOR_DEV: record(SENSOR_DEV, 0x01000000, None, job)}
    await fake_hub.drop()
    await wait_for(hass, lambda: len(fake_hub.calls("ota/stream/offer")) == 2)
    await wait_for(hass, lambda: len(fake_hub.calls("ota/start")) == 2)
    assert fake_hub.calls("ota/start")[1]["force"] is True

    # Done: the stream is closed and the cached file dropped.
    done = {"state": "done", "file_version": 0x01000200, "image": 0x1_0000, "progress": 100}
    await fake_hub.push(
        "ota", {"topic": "ota_device", "device": record(SENSOR_DEV, 0x01000200, None, done)}
    )
    await wait_for(hass, lambda: bool(fake_hub.calls("ota/stream/close")))
    await wait_for(hass, lambda: not os.path.exists(cached))
