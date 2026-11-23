"""Local automations of the hub (API 1.3): a switch (enabled) and a run button each."""

from __future__ import annotations

from typing import Any

from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub.const import DOMAIN
from homeassistant.const import STATE_OFF, STATE_ON
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er

from .fake_hub import FakeHub
from .test_init import eid, wait_for


def rule(rule_id: str, name: str, enabled: bool = True, **extra: Any) -> dict[str, Any]:
    return {
        "v": 1,
        "id": rule_id,
        "name": name,
        "enabled": enabled,
        "mode": "single",
        "triggers": [],
        "conditions": [],
        "actions": [{"type": "notify", "message": "x"}],
        "running": False,
        "runs": 0,
        "last_run": None,
        "last_result": None,
        **extra,
    }


def ids(entry: MockConfigEntry, rule_id: str) -> tuple[str, str]:
    return (
        f"{entry.entry_id}_automation_{rule_id}",
        f"{entry.entry_id}_automation_{rule_id}_run",
    )


async def setup(hass: HomeAssistant, fake_hub: FakeHub, entry: MockConfigEntry) -> None:
    fake_hub.api_version = "1.3"
    fake_hub.automations = {
        "porch": rule("porch", "Porch light"),
        "night": rule("night", "Night", enabled=False, suspended="time not synchronised"),
    }
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()


async def test_automation_entities(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    await setup(hass, fake_hub, config_entry)
    assert "automations" in fake_hub.calls("subscribe")[0]["topics"]
    sw_uid, run_uid = ids(config_entry, "porch")
    porch = hass.states.get(eid(hass, "switch", sw_uid))
    assert porch.state == STATE_ON
    assert porch.name.endswith("Automation Porch light")
    assert porch.attributes["automation_id"] == "porch"
    night = hass.states.get(eid(hass, "switch", ids(config_entry, "night")[0]))
    assert night.state == STATE_OFF
    assert night.attributes["suspended"] == "time not synchronised"
    button = eid(hass, "button", run_uid)
    assert hass.states.get(button).name.endswith("Run Porch light")
    # On the hub's own device.
    reg = er.async_get(hass)
    hub_device = reg.async_get(
        eid(hass, "button", f"{config_entry.entry_id}_permit_join")
    ).device_id
    assert reg.async_get(porch.entity_id).device_id == hub_device

    # Disable from HA; the hub's event and result agree.
    await hass.services.async_call(
        "switch", "turn_off", {"entity_id": porch.entity_id}, blocking=True
    )
    assert fake_hub.calls("automations/disable")[0]["automation"] == "porch"
    await wait_for(hass, lambda: hass.states.get(porch.entity_id).state == STATE_OFF)
    await hass.services.async_call(
        "switch", "turn_on", {"entity_id": porch.entity_id}, blocking=True
    )
    await wait_for(hass, lambda: hass.states.get(porch.entity_id).state == STATE_ON)

    # Run now: the run event updates the switch's attributes.
    await hass.services.async_call("button", "press", {"entity_id": button}, blocking=True)
    assert fake_hub.calls("automations/run")[0]["automation"] == "porch"
    await wait_for(
        hass, lambda: hass.states.get(porch.entity_id).attributes["last_result"] == "done"
    )
    assert hass.states.get(porch.entity_id).attributes["last_run"] is not None

    await hass.config_entries.async_unload(config_entry.entry_id)
    await hass.async_block_till_done()


async def test_added_renamed_removed_and_resynced(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    await setup(hass, fake_hub, config_entry)
    reg = er.async_get(hass)
    new = rule("r3", "Stairs")
    fake_hub.automations["r3"] = new
    await fake_hub.push("automations", {"topic": "automation_added", "automation": new})
    sw_uid = ids(config_entry, "r3")[0]
    await wait_for(hass, lambda: reg.async_get_entity_id("switch", DOMAIN, sw_uid) is not None)
    sw = eid(hass, "switch", sw_uid)
    await wait_for(hass, lambda: hass.states.get(sw) is not None)
    assert hass.states.get(sw).state == STATE_ON

    renamed = {**new, "name": "Stairs light", "enabled": False}
    await fake_hub.push("automations", {"topic": "automation_updated", "automation": renamed})
    await wait_for(hass, lambda: hass.states.get(sw).state == STATE_OFF)
    assert hass.states.get(sw).name.endswith("Automation Stairs light")

    await fake_hub.push("automations", {"topic": "automation_removed", "automation": "r3"})
    await wait_for(hass, lambda: reg.async_get_entity_id("switch", DOMAIN, sw_uid) is None)
    assert reg.async_get_entity_id("button", DOMAIN, ids(config_entry, "r3")[1]) is None

    # Removed while HA was away: gone after the reconnect's list.
    del fake_hub.automations["porch"]
    await fake_hub.drop()
    await wait_for(
        hass,
        lambda: reg.async_get_entity_id("switch", DOMAIN, ids(config_entry, "porch")[0]) is None,
    )
    night = eid(hass, "switch", ids(config_entry, "night")[0])
    await wait_for(hass, lambda: hass.states.get(night).state == STATE_OFF)

    await hass.config_entries.async_unload(config_entry.entry_id)
    await hass.async_block_till_done()


async def test_hub_without_automations(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    assert "automations" not in fake_hub.calls("subscribe")[0]["topics"]
    assert not fake_hub.calls("automations/list")
    entries = er.async_entries_for_config_entry(er.async_get(hass), setup_entry.entry_id)
    assert not [e for e in entries if "_automation_" in e.unique_id]
