"""API 1.1: network entities, permit-join state, hub clock, 1.0 fallback."""

from __future__ import annotations

import asyncio
from datetime import timedelta

from pytest_homeassistant_custom_component.common import (
    MockConfigEntry,
    async_fire_time_changed,
)

from custom_components.zigbee_rs_hub.const import DOMAIN
from homeassistant.const import STATE_OFF, STATE_ON, STATE_UNKNOWN
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er
from homeassistant.util import dt as dt_util

from .fake_hub import SENSOR_DEV, FakeHub, entity, state
from .test_init import eid, wait_for


def hub_eid(hass: HomeAssistant, entry: MockConfigEntry, platform: str, key: str) -> str:
    return eid(hass, platform, f"{entry.entry_id}_{key}")


async def test_network_entities(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    assert "network" in fake_hub.calls("subscribe")[0]["topics"]
    net = hass.states.get(hub_eid(hass, setup_entry, "sensor", "network_state"))
    assert net.state == "up"
    assert net.attributes["pan_id"] == "0x1C06"
    assert net.attributes["extended_pan_id"] == "744dbdfffe641ece"
    assert hass.states.get(hub_eid(hass, setup_entry, "sensor", "channel")).state == "17"
    assert (
        hass.states.get(hub_eid(hass, setup_entry, "binary_sensor", "permit_join_open")).state
        == STATE_OFF
    )
    ends = hub_eid(hass, setup_entry, "sensor", "permit_join_until")
    assert hass.states.get(ends).state == STATE_UNKNOWN

    await fake_hub.push("network", {"topic": "network_state", "state": "down"})
    await wait_for(hass, lambda: hass.states.get(net.entity_id).state == "down")

    from homeassistant.helpers import device_registry as dr

    hub_dev = dr.async_get(hass).async_get(er.async_get(hass).async_get(net.entity_id).device_id)
    assert hub_dev.hw_version == "esp-thread-br-zigbee-gw (NCP 0.1.0)"


async def test_permit_join_state(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    setup_entry: MockConfigEntry,
) -> None:
    open_id = hub_eid(hass, setup_entry, "binary_sensor", "permit_join_open")
    ends_id = hub_eid(hass, setup_entry, "sensor", "permit_join_until")
    await fake_hub.push("network", {"topic": "permit_join", "remaining_s": 60})
    await wait_for(hass, lambda: hass.states.get(open_id).state == STATE_ON)
    until = dt_util.parse_datetime(hass.states.get(ends_id).state)
    assert timedelta(seconds=55) < until - dt_util.utcnow() <= timedelta(seconds=60)
    assert hass.states.get(open_id).attributes["until"] is not None

    # Closed by the hub.
    await fake_hub.push("network", {"topic": "permit_join", "remaining_s": 0})
    await wait_for(hass, lambda: hass.states.get(open_id).state == STATE_OFF)
    assert hass.states.get(ends_id).state == STATE_UNKNOWN

    # Closed on time even without the hub's event.
    await fake_hub.push("network", {"topic": "permit_join", "remaining_s": 1})
    await wait_for(hass, lambda: hass.states.get(open_id).state == STATE_ON)
    await asyncio.sleep(1.1)
    async_fire_time_changed(hass)
    await hass.async_block_till_done()
    assert hass.states.get(open_id).state == STATE_OFF


async def test_timestamps_on_an_unsynchronised_hub_clock(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    """`last_seen` 100 s before the hub's `time` is 100 s ago, not 1970."""
    uid = f"{SENSOR_DEV}:0:last_seen"
    fake_hub.entities[uid] = entity(uid, "sensor", device_class="timestamp", category="diagnostic")
    fake_hub.states[uid] = state(uid, fake_hub.network["time"] - 100)
    config_entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    st = hass.states.get(eid(hass, "sensor", uid))
    assert st.entity_id == "sensor.motion_sensor_last_seen"
    age = dt_util.utcnow() - dt_util.parse_datetime(st.state)
    assert timedelta(seconds=95) < age < timedelta(seconds=110)
    await hass.config_entries.async_unload(config_entry.entry_id)


async def test_api_1_0_hub(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    """No `network` topic and no network entities on API 1.0."""
    fake_hub.api_version = "1.0"
    config_entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    assert fake_hub.calls("subscribe")[0]["topics"] == ["devices", "entities", "events"]
    reg = er.async_get(hass)
    assert (
        reg.async_get_entity_id("sensor", DOMAIN, f"{config_entry.entry_id}_network_state") is None
    )
    assert reg.async_get_entity_id("binary_sensor", DOMAIN, f"{config_entry.entry_id}_connected")
    await hass.config_entries.async_unload(config_entry.entry_id)
