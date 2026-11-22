"""Zigbee groups (device-support 14.9): group devices, their light and
scene entities, commands; group devices removed in HA delete the group."""

from __future__ import annotations

from collections.abc import AsyncGenerator

import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub import async_remove_config_entry_device
from custom_components.zigbee_rs_hub.const import DOMAIN
from custom_components.zigbee_rs_hub.hub import find_device
from homeassistant.const import ATTR_ENTITY_ID, STATE_ON
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr, entity_registry as er

from .fake_hub import FakeHub, entity, state

GROUP_DEV = "grp:0001"
GROUP_LIGHT = f"{GROUP_DEV}:0:light"
GROUP_SCENE = f"{GROUP_DEV}:0:scene_1"


def group_device() -> dict:
    """A group device record as the hub sends it."""
    return {
        "id": GROUP_DEV,
        "group": 1,
        "ieee": None,
        "nwk": None,
        "src_id": None,
        "manufacturer": "zigbee-rs",
        "model": "Zigbee group",
        "model_id": "0x0001",
        "name": "Kitchen",
        "via": "hub",
        "power": "mains",
        "available": True,
        "last_seen": None,
        "warnings": [],
        "interview": "complete",
        "disabled": False,
        "definition": [],
    }


@pytest.fixture
async def with_group(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> AsyncGenerator[MockConfigEntry]:
    """The fake network plus a group with a light and a scene."""
    fake_hub.devices[GROUP_DEV] = group_device()
    for e in [
        entity(
            GROUP_LIGHT,
            "light",
            capabilities={
                "supported_color_modes": ["color_temp"],
                "supported_features": ["transition"],
                "min_color_temp_kelvin": 2203,
                "max_color_temp_kelvin": 6536,
            },
        ),
        entity(GROUP_SCENE, "scene", name="Evening"),
    ]:
        fake_hub.entities[e["uid"]] = e
    fake_hub.states[GROUP_LIGHT] = state(
        GROUP_LIGHT, "on", brightness=200, color_temp_kelvin=2700, color_mode="color_temp"
    )
    fake_hub.states[GROUP_SCENE] = state(GROUP_SCENE, None)
    config_entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    yield config_entry
    await hass.config_entries.async_unload(config_entry.entry_id)
    await hass.async_block_till_done()


def eid(hass: HomeAssistant, platform: str, uid: str) -> str:
    entity_id = er.async_get(hass).async_get_entity_id(platform, DOMAIN, uid)
    assert entity_id is not None, uid
    return entity_id


async def test_group_device_light_and_scene(
    hass: HomeAssistant, fake_hub: FakeHub, with_group: MockConfigEntry
) -> None:
    dev = find_device(dr.async_get(hass), (DOMAIN, GROUP_DEV), with_group.entry_id)
    assert dev is not None
    assert (dev.name, dev.manufacturer, dev.model) == ("Kitchen", "zigbee-rs", "Zigbee group")
    assert dev.serial_number is None

    light = eid(hass, "light", GROUP_LIGHT)
    assert light == "light.kitchen"
    st = hass.states.get(light)
    assert st.state == STATE_ON
    assert st.attributes["brightness"] == 200

    scene = eid(hass, "scene", GROUP_SCENE)
    assert scene == "scene.kitchen_evening"
    await hass.services.async_call("scene", "turn_on", {ATTR_ENTITY_ID: scene}, blocking=True)
    await hass.services.async_call(
        "scene", "turn_on", {ATTR_ENTITY_ID: scene, "transition": 2}, blocking=True
    )
    calls = fake_hub.calls("entities/command")
    assert [(c["uid"], c["command"], c["params"]) for c in calls] == [
        (GROUP_SCENE, "activate", {}),
        (GROUP_SCENE, "activate", {"transition": 2}),
    ]

    await hass.services.async_call(
        "light", "turn_on", {ATTR_ENTITY_ID: light, "brightness": 100}, blocking=True
    )
    last = fake_hub.calls("entities/command")[-1]
    assert (last["uid"], last["command"], last["params"]) == (
        GROUP_LIGHT,
        "turn_on",
        {"brightness": 100},
    )


async def test_removing_the_group_device_deletes_the_group(
    hass: HomeAssistant, fake_hub: FakeHub, with_group: MockConfigEntry
) -> None:
    dev = find_device(dr.async_get(hass), (DOMAIN, GROUP_DEV), with_group.entry_id)
    assert await async_remove_config_entry_device(hass, with_group, dev)
    assert fake_hub.calls("devices/remove")[-1]["device"] == GROUP_DEV
