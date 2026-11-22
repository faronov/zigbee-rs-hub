"""Set-up, entities, pushed state, commands, reconnect, services, removal."""

from __future__ import annotations

import asyncio
from collections.abc import Callable
from unittest.mock import patch

import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub import async_remove_config_entry_device
from custom_components.zigbee_rs_hub.const import DOMAIN
from custom_components.zigbee_rs_hub.diagnostics import (
    async_get_config_entry_diagnostics,
    async_get_device_diagnostics,
)
from custom_components.zigbee_rs_hub.hub import find_device
from homeassistant.config_entries import ConfigEntryState
from homeassistant.const import ATTR_ENTITY_ID, STATE_OFF, STATE_ON, STATE_UNAVAILABLE
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import device_registry as dr, entity_registry as er
from homeassistant.setup import async_setup_component

from .fake_hub import (
    ACTION_UID,
    COORDINATOR_IEEE,
    IDENTIFY_UID,
    LIGHT_DEV,
    LIGHT_UID,
    LQI_UID,
    OCC_UID,
    ON_LEVEL_UID,
    POB_UID,
    SENSOR_DEV,
    SWITCH_UID,
    TEMP_UID,
    FakeHub,
    entity,
    state,
)


def eid(hass: HomeAssistant, platform: str, unique_id: str) -> str:
    """Entity ID of a unique ID."""
    entity_id = er.async_get(hass).async_get_entity_id(platform, DOMAIN, unique_id)
    assert entity_id is not None, unique_id
    return entity_id


async def wait_for(hass: HomeAssistant, cond: Callable[[], bool], timeout: float = 5.0) -> None:
    """Let the event loop run until `cond` holds."""
    async with asyncio.timeout(timeout):
        while not cond():
            await asyncio.sleep(0.01)
            await hass.async_block_till_done()


async def test_devices_and_entities(hass: HomeAssistant, setup_entry: MockConfigEntry) -> None:
    assert setup_entry.state is ConfigEntryState.LOADED
    devices = dr.async_get(hass)
    hub_dev = find_device(devices, (DOMAIN, f"hub_{setup_entry.entry_id}"), setup_entry.entry_id)
    assert hub_dev is not None
    assert hub_dev.sw_version == "0.2.4"
    assert hub_dev.serial_number == COORDINATOR_IEEE
    assert hub_dev.model == "zigbee-rs NCP"

    light_dev = find_device(devices, (DOMAIN, LIGHT_DEV), setup_entry.entry_id)
    assert light_dev is not None
    assert light_dev.manufacturer == "ACME"
    assert light_dev.model == "Dimmer"
    assert light_dev.sw_version == "1.2.3"
    assert light_dev.hw_version == "1"
    assert light_dev.serial_number == "0017880100aabbcc"
    assert light_dev.via_device_id == hub_dev.id

    temp = hass.states.get(eid(hass, "sensor", TEMP_UID))
    assert temp.entity_id == "sensor.motion_sensor_temperature"
    assert temp.state == "21.4"
    assert temp.attributes["unit_of_measurement"] == "°C"
    assert temp.attributes["device_class"] == "temperature"
    assert temp.attributes["state_class"] == "measurement"

    assert hass.states.get(eid(hass, "binary_sensor", OCC_UID)).state == STATE_OFF
    light = hass.states.get(eid(hass, "light", LIGHT_UID))
    assert light.entity_id == "light.dimmer"
    assert light.state == STATE_ON
    assert light.attributes["brightness"] == 128
    assert light.attributes["supported_color_modes"] == ["brightness"]
    select = hass.states.get(eid(hass, "select", POB_UID))
    assert select.entity_id == "select.dimmer_power_on_behavior"
    assert select.state == "previous"
    assert select.attributes["options"] == ["off", "on", "toggle", "previous"]
    number = hass.states.get(eid(hass, "number", ON_LEVEL_UID))
    assert float(number.state) == 254
    assert number.attributes["max"] == 254
    assert hass.states.get(eid(hass, "button", IDENTIFY_UID)).entity_id == "button.dimmer_identify"
    assert (
        hass.states.get(eid(hass, "switch", SWITCH_UID)).entity_id == "switch.dimmer_switch_right"
    )
    action = hass.states.get(eid(hass, "event", ACTION_UID))
    assert action.attributes["event_types"] == ["on", "off", "brightness_up"]

    registry = er.async_get(hass)
    lqi = registry.async_get(eid(hass, "sensor", LQI_UID))
    assert lqi.disabled_by is er.RegistryEntryDisabler.INTEGRATION
    assert lqi.entity_category is er.EntityCategory.DIAGNOSTIC
    assert (
        registry.async_get(eid(hass, "select", POB_UID)).entity_category is er.EntityCategory.CONFIG
    )

    hub_conn = next(
        e
        for e in er.async_entries_for_config_entry(registry, setup_entry.entry_id)
        if e.unique_id == f"{setup_entry.entry_id}_connected"
    )
    assert hass.states.get(hub_conn.entity_id).state == STATE_ON


async def test_state_push_and_availability(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    temp_id = eid(hass, "sensor", TEMP_UID)
    await fake_hub.push_state(TEMP_UID, 23.75)
    await wait_for(hass, lambda: hass.states.get(temp_id).state == "23.75")

    await fake_hub.push_state(OCC_UID, True)
    occ_id = eid(hass, "binary_sensor", OCC_UID)
    await wait_for(hass, lambda: hass.states.get(occ_id).state == STATE_ON)

    # The device goes unavailable: all its entities follow.
    dev = dict(fake_hub.devices[SENSOR_DEV], available=False)
    await fake_hub.push("devices", {"topic": "device_updated", "device": dev})
    await wait_for(hass, lambda: hass.states.get(temp_id).state == STATE_UNAVAILABLE)
    assert hass.states.get(occ_id).state == STATE_UNAVAILABLE
    assert hass.states.get(eid(hass, "light", LIGHT_UID)).state == STATE_ON

    dev["available"] = True
    dev["name"] = "Hallway motion"
    await fake_hub.push("devices", {"topic": "device_updated", "device": dev})
    await wait_for(hass, lambda: hass.states.get(temp_id).state == "23.75")
    hub_device = find_device(dr.async_get(hass), (DOMAIN, SENSOR_DEV), setup_entry.entry_id)
    assert hub_device.name == "Hallway motion"

    # A per-entity unavailable state.
    msg = state(TEMP_UID, None)
    msg["available"] = False
    await fake_hub.push("entities", {"topic": "entity_state", **msg})
    await wait_for(hass, lambda: hass.states.get(temp_id).state == STATE_UNAVAILABLE)

    # The device comes back without the hub resending the states.
    await fake_hub.push("devices", {"topic": "device_updated", "device": dev})
    await wait_for(hass, lambda: hass.states.get(temp_id).state == "unknown")


async def test_entity_added_updated_removed(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    uid = f"{SENSOR_DEV}:1:humidity"
    rec = entity(uid, "sensor", device_class="humidity", unit="%", state_class="measurement")
    await fake_hub.push("entities", {"topic": "entity_added", "entity": rec})
    await fake_hub.push_state(uid, 55)
    registry = er.async_get(hass)
    await wait_for(hass, lambda: registry.async_get_entity_id("sensor", DOMAIN, uid) is not None)
    hum_id = eid(hass, "sensor", uid)
    await wait_for(hass, lambda: (s := hass.states.get(hum_id)) is not None and s.state == "55")

    # Orphaned by a new definition: unavailable.
    await fake_hub.push("entities", {"topic": "entity_updated", "entity": dict(rec, orphaned=True)})
    await wait_for(hass, lambda: hass.states.get(hum_id).state == STATE_UNAVAILABLE)

    await fake_hub.push("entities", {"topic": "entity_removed", "uid": uid})
    await wait_for(hass, lambda: registry.async_get_entity_id("sensor", DOMAIN, uid) is None)
    await wait_for(hass, lambda: hass.states.get(hum_id) is None)


async def test_entity_renamed_by_a_new_resolution(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    uid = f"{SENSOR_DEV}:1:high_temperature"
    rec = entity(uid, "number", name="high temperature 1", unit="°C")
    await fake_hub.push("entities", {"topic": "entity_added", "entity": rec})
    await fake_hub.push_state(uid, 30)
    registry = er.async_get(hass)
    await wait_for(hass, lambda: registry.async_get_entity_id("number", DOMAIN, uid) is not None)
    ent_id = eid(hass, "number", uid)
    await wait_for(hass, lambda: hass.states.get(ent_id) is not None)
    assert hass.states.get(ent_id).attributes["friendly_name"].endswith("High temperature 1")

    # Re-resolved without the endpoint suffix: the name follows.
    await fake_hub.push(
        "entities", {"topic": "entity_updated", "entity": dict(rec, name="high temperature")}
    )
    await wait_for(
        hass,
        lambda: hass.states.get(ent_id).attributes["friendly_name"].endswith("High temperature"),
    )


async def test_snapshot_drops_entities_removed_while_away(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    registry = er.async_get(hass)
    temp_id = eid(hass, "sensor", TEMP_UID)
    lqi_id = eid(hass, "sensor", LQI_UID)
    port = fake_hub.port
    await fake_hub.stop()
    # A catalog definition replaced the temperature entity while HA was away;
    # the link quality entity became an orphan (kept).
    del fake_hub.entities[TEMP_UID]
    fake_hub.states.pop(TEMP_UID, None)
    fake_hub.entities[LQI_UID] = dict(fake_hub.entities[LQI_UID], orphaned=True)
    await fake_hub.start(port)
    await wait_for(hass, lambda: registry.async_get_entity_id("sensor", DOMAIN, TEMP_UID) is None)
    await wait_for(hass, lambda: hass.states.get(temp_id) is None)
    assert registry.async_get(lqi_id) is not None
    # Hub-level entities are not the hub's device entities.
    assert any(
        e.unique_id.endswith("_connected")
        for e in er.async_entries_for_config_entry(registry, setup_entry.entry_id)
    )


async def test_device_added_at_runtime(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    from .fake_hub import device

    dev_id = "zb:0011223344556677"
    await fake_hub.push(
        "devices", {"topic": "device_added", "device": device(dev_id, "Sonoff", "SNZB-02")}
    )
    uid = f"{dev_id}:1:temperature"
    await fake_hub.push(
        "entities",
        {
            "topic": "entity_added",
            "entity": entity(uid, "sensor", device_class="temperature", unit="°C"),
        },
    )
    await fake_hub.push_state(uid, 19.5)
    await wait_for(hass, lambda: hass.states.get("sensor.snzb_02_temperature") is not None)
    await wait_for(hass, lambda: hass.states.get("sensor.snzb_02_temperature").state == "19.5")


async def test_event_entity(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    action_id = eid(hass, "event", ACTION_UID)
    await fake_hub.push(
        "events",
        {
            "topic": "entity_event",
            "uid": ACTION_UID,
            "event_type": "brightness_up",
            "attributes": {"duration": 2},
            "time": 1795098200.0,
        },
    )
    await wait_for(
        hass, lambda: hass.states.get(action_id).attributes.get("event_type") == "brightness_up"
    )
    assert hass.states.get(action_id).attributes["duration"] == 2


@pytest.mark.parametrize(
    ("domain", "service", "uid", "data", "command", "params"),
    [
        (
            "light",
            "turn_on",
            LIGHT_UID,
            {"brightness": 77, "transition": 2},
            "turn_on",
            {"brightness": 77, "transition": 2},
        ),
        ("light", "turn_off", LIGHT_UID, {}, "turn_off", {}),
        ("switch", "turn_off", SWITCH_UID, {}, "turn_off", {}),
        ("switch", "turn_on", SWITCH_UID, {}, "turn_on", {}),
        ("number", "set_value", ON_LEVEL_UID, {"value": 100}, "set_value", {"value": 100}),
        ("select", "select_option", POB_UID, {"option": "off"}, "select_option", {"option": "off"}),
        ("button", "press", IDENTIFY_UID, {}, "press", {}),
    ],
)
async def test_commands(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    setup_entry: MockConfigEntry,
    domain: str,
    service: str,
    uid: str,
    data: dict,
    command: str,
    params: dict,
) -> None:
    await hass.services.async_call(
        domain, service, {ATTR_ENTITY_ID: eid(hass, domain, uid), **data}, blocking=True
    )
    calls = fake_hub.calls("entities/command")
    assert len(calls) == 1
    assert calls[0]["uid"] == uid
    assert calls[0]["command"] == command
    assert calls[0]["params"] == params


async def test_command_error(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    fake_hub.command_errors[SWITCH_UID] = {
        "code": "zcl_status",
        "message": "device answered FAILURE",
    }
    with pytest.raises(HomeAssistantError, match="FAILURE"):
        await hass.services.async_call(
            "switch", "turn_on", {ATTR_ENTITY_ID: eid(hass, "switch", SWITCH_UID)}, blocking=True
        )


async def test_reconnect_resyncs(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    temp_id = eid(hass, "sensor", TEMP_UID)
    conns = fake_hub.connections
    port = fake_hub.port
    await fake_hub.stop()
    await wait_for(hass, lambda: hass.states.get(temp_id).state == STATE_UNAVAILABLE)
    assert hass.states.get(eid(hass, "light", LIGHT_UID)).state == STATE_UNAVAILABLE

    # Changed while the hub was away: the snapshot on reconnect brings it.
    fake_hub.states[TEMP_UID] = state(TEMP_UID, 18.0)
    await asyncio.sleep(0.3)  # a few failed attempts
    await fake_hub.start(port)
    await wait_for(hass, lambda: hass.states.get(temp_id).state == "18.0")
    assert fake_hub.connections == conns + 1
    assert hass.states.get(eid(hass, "light", LIGHT_UID)).state == STATE_ON
    assert len(fake_hub.calls("subscribe")) == 2

    # And pushes work on the new connection.
    await fake_hub.push_state(TEMP_UID, 18.5)
    await wait_for(hass, lambda: hass.states.get(temp_id).state == "18.5")


async def test_setup_retries_when_hub_is_down(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    await fake_hub.stop()
    config_entry.add_to_hass(hass)
    assert not await hass.config_entries.async_setup(config_entry.entry_id)
    assert config_entry.state is ConfigEntryState.SETUP_RETRY


async def test_permit_join_service_and_button(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    await hass.services.async_call(
        DOMAIN, "permit_join", {"duration": 60, "include_gp": True}, blocking=True
    )
    assert fake_hub.calls("network/permit_join")[-1] == {
        "id": fake_hub.calls("network/permit_join")[-1]["id"],
        "type": "network/permit_join",
        "duration_s": 60,
        "include_gp": True,
    }
    light_dev = find_device(dr.async_get(hass), (DOMAIN, LIGHT_DEV), setup_entry.entry_id)
    await hass.services.async_call(
        DOMAIN, "permit_join", {"device_id": light_dev.id}, blocking=True
    )
    last = fake_hub.calls("network/permit_join")[-1]
    assert last["duration_s"] == 254
    assert last["target"] == LIGHT_DEV

    button = next(
        e
        for e in er.async_entries_for_config_entry(er.async_get(hass), setup_entry.entry_id)
        if e.unique_id == f"{setup_entry.entry_id}_permit_join"
    )
    await hass.services.async_call(
        "button", "press", {ATTR_ENTITY_ID: button.entity_id}, blocking=True
    )
    assert len(fake_hub.calls("network/permit_join")) == 3

    await hass.services.async_call(
        DOMAIN, "reconfigure_device", {"device_id": light_dev.id}, blocking=True
    )
    assert fake_hub.calls("devices/reconfigure")[-1]["device"] == LIGHT_DEV
    hub_dev = find_device(
        dr.async_get(hass), (DOMAIN, f"hub_{setup_entry.entry_id}"), setup_entry.entry_id
    )
    with pytest.raises(HomeAssistantError):
        await hass.services.async_call(
            DOMAIN, "reinterview_device", {"device_id": hub_dev.id}, blocking=True
        )


async def test_remove_device(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    assert await async_setup_component(hass, "config", {})
    devices = dr.async_get(hass)
    sensor_dev = find_device(devices, (DOMAIN, SENSOR_DEV), setup_entry.entry_id)
    hub_dev = find_device(devices, (DOMAIN, f"hub_{setup_entry.entry_id}"), setup_entry.entry_id)
    assert not await async_remove_config_entry_device(hass, setup_entry, hub_dev)
    assert await async_remove_config_entry_device(hass, setup_entry, sensor_dev)
    assert fake_hub.calls("devices/remove")[-1]["device"] == SENSOR_DEV
    # HA removes the device entry itself after the callback; the hub's
    # device_removed event must not have removed it first.
    await hass.async_block_till_done()
    assert find_device(devices, (DOMAIN, SENSOR_DEV), setup_entry.entry_id) is not None
    devices.async_remove_device(sensor_dev.id)
    assert find_device(devices, (DOMAIN, SENSOR_DEV), setup_entry.entry_id) is None

    # Removal on the hub side (e.g. from its web UI) removes the HA device.
    await fake_hub.push("devices", {"topic": "device_removed", "id": LIGHT_DEV})
    await wait_for(
        hass, lambda: find_device(devices, (DOMAIN, LIGHT_DEV), setup_entry.entry_id) is None
    )
    assert er.async_get(hass).async_get_entity_id("light", DOMAIN, LIGHT_UID) is None


async def test_diagnostics_redact(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    diag = await async_get_config_entry_diagnostics(hass, setup_entry)
    assert diag["entry"]["data"]["token"] == "**REDACTED**"
    assert diag["connected"] is True
    assert diag["info"]["coordinator_ieee"] == COORDINATOR_IEEE
    assert len(diag["devices"]) == 3
    assert "secret-token" not in str(diag)

    light_dev = find_device(dr.async_get(hass), (DOMAIN, LIGHT_DEV), setup_entry.entry_id)
    ddiag = await async_get_device_diagnostics(hass, setup_entry, light_dev)
    assert ddiag["device"]["id"] == LIGHT_DEV
    assert len(ddiag["entities"]) == 5
    assert ddiag["hub_diagnostics"]["gp_data"]["key"] == "**REDACTED**"


async def test_unique_id_moves_to_coordinator_ieee(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    """Entries made before `hub/info` adopt the IEEE once the hub tells it."""
    entry = MockConfigEntry(domain=DOMAIN, unique_id="127.0.0.1:1", data=dict(config_entry.data))
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    assert entry.unique_id == COORDINATOR_IEEE
    await hass.config_entries.async_unload(entry.entry_id)


async def test_keepalive_drops_a_hung_hub(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    """A hub that stops answering is reconnected by the keepalive."""
    fake_hub.hang_after_connect = True
    conns = fake_hub.connections
    with (
        patch("custom_components.zigbee_rs_hub.api.KEEPALIVE", 0.05),
        patch("custom_components.zigbee_rs_hub.api.REQUEST_TIMEOUT", 0.1),
    ):
        # The patched interval applies to the next connection's loop.
        await fake_hub.drop()
        await wait_for(hass, lambda: fake_hub.connections >= conns + 2)
    temp_id = eid(hass, "sensor", TEMP_UID)
    fake_hub.hang_after_connect = False
    await wait_for(hass, lambda: hass.states.get(temp_id).state == "21.4")


async def test_composite_platforms(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    from .fake_hub import device

    dev_id = "zb:00aa00aa00aa00aa"
    await fake_hub.push(
        "devices", {"topic": "device_added", "device": device(dev_id, "X", "Combo")}
    )
    records = {
        "thermostat": entity(
            f"{dev_id}:1:thermostat",
            "climate",
            capabilities={
                "hvac_modes": ["off", "heat"],
                "min_temp": 5,
                "max_temp": 30,
                "target_temperature_step": 0.5,
                "preset_modes": ["eco", "comfort"],
                "supported_features": ["target_temperature", "preset_mode", "turn_off", "turn_on"],
                "temperature_unit": "°C",
            },
        ),
        "cover": entity(
            f"{dev_id}:2:cover",
            "cover",
            device_class="blind",
            capabilities={"supported_features": ["open", "close", "stop", "set_position"]},
        ),
        "lock": entity(f"{dev_id}:3:lock", "lock", capabilities={"supported_features": []}),
        "fan": entity(
            f"{dev_id}:4:fan",
            "fan",
            capabilities={
                "speed_count": 3,
                "preset_modes": [],
                "supported_features": ["set_speed", "preset_mode", "turn_on", "turn_off"],
            },
        ),
        "siren": entity(
            f"{dev_id}:5:siren",
            "siren",
            capabilities={"supported_features": ["turn_on", "turn_off"]},
        ),
        "valve": entity(
            f"{dev_id}:6:valve",
            "valve",
            capabilities={"reports_position": False, "supported_features": ["open", "close"]},
        ),
        "schedule": entity(
            f"{dev_id}:1:schedule",
            "text",
            capabilities={"native_min": 0, "native_max": 64, "mode": "text"},
        ),
        "color": entity(
            f"{dev_id}:7:light",
            "light",
            name="light rgb",
            capabilities={
                "supported_color_modes": ["color_temp", "xy"],
                "supported_features": ["transition"],
            },
        ),
    }
    states = {
        "thermostat": state(
            records["thermostat"]["uid"],
            "heat",
            current_temperature=20.5,
            target_temperature=21,
            hvac_action="heating",
            preset="eco",
        ),
        "cover": state(records["cover"]["uid"], "open", current_position=40),
        "lock": state(records["lock"]["uid"], "locked"),
        "fan": state(records["fan"]["uid"], "on", percentage=66),
        "siren": state(records["siren"]["uid"], "off"),
        "valve": state(records["valve"]["uid"], "closed"),
        "schedule": state(records["schedule"]["uid"], "06:00/21 22:00/17"),
        "color": state(
            records["color"]["uid"], "on", brightness=200, xy_color=[0.3, 0.3], color_mode=1
        ),
    }
    for key, rec in records.items():
        fake_hub.states[rec["uid"]] = states[key]
        await fake_hub.push("entities", {"topic": "entity_added", "entity": rec})
        await fake_hub.push("entities", {"topic": "entity_state", **states[key]})
    registry = er.async_get(hass)
    await wait_for(
        hass,
        lambda: all(
            registry.async_get_entity_id(r["platform"], DOMAIN, r["uid"]) for r in records.values()
        ),
    )
    await hass.async_block_till_done()

    def st(key: str):
        rec = records[key]
        return hass.states.get(eid(hass, rec["platform"], rec["uid"]))

    climate = st("thermostat")
    assert climate.entity_id == "climate.combo"
    assert climate.state == "heat"
    assert climate.attributes["current_temperature"] == 20.5
    assert climate.attributes["temperature"] == 21
    assert climate.attributes["hvac_action"] == "heating"
    assert climate.attributes["preset_mode"] == "eco"
    cover = st("cover")
    assert cover.state == "open"
    assert cover.attributes["current_position"] == 40
    assert st("lock").state == "locked"
    assert st("fan").state == "on"
    assert st("fan").attributes["percentage"] == 66
    assert st("siren").state == "off"
    assert st("valve").state == "closed"
    assert st("schedule").state == "06:00/21 22:00/17"
    color = st("color")
    assert color.attributes["color_mode"] == "xy"
    assert color.attributes["xy_color"] == (0.3, 0.3)

    calls = [
        (
            "climate",
            "set_temperature",
            "thermostat",
            {"temperature": 22.5},
            "set_temperature",
            {"temperature": 22.5},
        ),
        (
            "climate",
            "set_preset_mode",
            "thermostat",
            {"preset_mode": "comfort"},
            "set_preset_mode",
            {"preset_mode": "comfort"},
        ),
        (
            "climate",
            "set_hvac_mode",
            "thermostat",
            {"hvac_mode": "off"},
            "set_hvac_mode",
            {"hvac_mode": "off"},
        ),
        (
            "cover",
            "set_cover_position",
            "cover",
            {"position": 70},
            "set_cover_position",
            {"position": 70},
        ),
        ("cover", "stop_cover", "cover", {}, "stop_cover", {}),
        ("lock", "unlock", "lock", {}, "unlock", {}),
        ("fan", "set_percentage", "fan", {"percentage": 33}, "set_percentage", {"percentage": 33}),
        ("siren", "turn_on", "siren", {}, "turn_on", {}),
        ("valve", "open_valve", "valve", {}, "open_valve", {}),
        (
            "text",
            "set_value",
            "schedule",
            {"value": "07:00/20"},
            "set_value",
            {"value": "07:00/20"},
        ),
        (
            "light",
            "turn_on",
            "color",
            {"color_temp_kelvin": 3000},
            "turn_on",
            {"color_temp_kelvin": 3000},
        ),
        (
            "light",
            "turn_on",
            "color",
            {"xy_color": [0.4, 0.4]},
            "turn_on",
            {"xy_color": [0.4, 0.4]},
        ),
    ]
    for domain, service, key, data, command, params in calls:
        await hass.services.async_call(
            domain, service, {ATTR_ENTITY_ID: st(key).entity_id, **data}, blocking=True
        )
        last = fake_hub.calls("entities/command")[-1]
        assert (last["uid"], last["command"], last["params"]) == (
            records[key]["uid"],
            command,
            params,
        ), service
