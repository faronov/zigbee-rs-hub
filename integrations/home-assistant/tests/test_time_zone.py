"""Time zone sync (option `sync_time_zone`): HA's zone and its TZif footer
sent with `hub/time/set` when the hub's differ."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import patch
import zoneinfo

from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub.const import CONF_SYNC_TIME_ZONE
from custom_components.zigbee_rs_hub.time_zone import posix_tz
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType

from .fake_hub import FakeHub

TALLINN = "EET-2EEST,M3.5.0/3,M10.5.0/4"


def test_posix_tz_is_the_tzif_footer() -> None:
    assert posix_tz("Europe/Tallinn") == TALLINN
    assert posix_tz("America/New_York") == "EST5EDT,M3.2.0,M11.1.0"
    assert posix_tz("Asia/Kolkata") == "IST-5:30"
    assert posix_tz("UTC") == "UTC0"
    for name in ("No/Such_Zone", "../../etc/passwd", "/etc/localtime", "Europe/../UTC", ""):
        assert posix_tz(name) is None, name


def test_posix_tz_needs_a_version_2_file_with_a_footer(tmp_path: Path) -> None:
    (tmp_path / "Test").mkdir()
    files = {
        "V2": b"TZif2" + bytes(15) + b"\x00\n\x01data\n<+03>-3\n",
        "V1": b"TZif\x00" + bytes(15) + b"data\n<+03>-3\n",
        "Empty": b"TZif3" + bytes(15) + b"data\n\n",
        "Truncated": b"TZif2" + bytes(15) + b"data\n<+03>-3",
        "Other": b"text\n<+03>-3\n",
    }
    for name, data in files.items():
        (tmp_path / "Test" / name).write_bytes(data)
    with patch.object(zoneinfo, "TZPATH", (str(tmp_path),)):
        assert posix_tz("Test/V2") == "<+03>-3"
        for name in ("V1", "Empty", "Truncated", "Other"):
            assert posix_tz(f"Test/{name}") is None, name


async def _setup(hass: HomeAssistant, entry: MockConfigEntry, sync: bool | None) -> None:
    entry.add_to_hass(hass)
    if sync is not None:
        hass.config_entries.async_update_entry(entry, options={CONF_SYNC_TIME_ZONE: sync})
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()


async def test_sync_sends_the_zone_and_follows_ha(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    await hass.config.async_set_time_zone("Europe/Tallinn")
    await _setup(hass, config_entry, True)
    assert [(r["tz"], r["tz_name"]) for r in fake_hub.calls("hub/time/set")] == [
        (TALLINN, "Europe/Tallinn")
    ]
    assert fake_hub.time == {"tz": TALLINN, "tz_name": "Europe/Tallinn"}
    # The same zone after a reconnect: nothing sent.
    await fake_hub.drop()
    for _ in range(50):
        await hass.async_block_till_done()
        if len(fake_hub.calls("hub/time")) == 2:
            break
        await asyncio.sleep(0.05)
    assert len(fake_hub.calls("hub/time")) == 2
    assert len(fake_hub.calls("hub/time/set")) == 1
    # HA's zone changes.
    await hass.config.async_update(time_zone="America/New_York")
    await hass.async_block_till_done()
    assert fake_hub.time == {"tz": "EST5EDT,M3.2.0,M11.1.0", "tz_name": "America/New_York"}
    await hass.config_entries.async_unload(config_entry.entry_id)


async def test_entries_without_the_option_leave_the_hub_alone(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    await hass.config.async_set_time_zone("Europe/Tallinn")
    await _setup(hass, config_entry, None)
    await hass.config.async_update(time_zone="America/New_York")
    await hass.async_block_till_done()
    assert fake_hub.calls("hub/time") == []
    assert fake_hub.time == {"tz": "UTC0", "tz_name": None}
    # Turned on in the options: synced at once.
    result = await hass.config_entries.options.async_init(config_entry.entry_id)
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "init"
    result = await hass.config_entries.options.async_configure(
        result["flow_id"], {CONF_SYNC_TIME_ZONE: True}
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    assert config_entry.options == {CONF_SYNC_TIME_ZONE: True}
    assert fake_hub.time == {"tz": "EST5EDT,M3.2.0,M11.1.0", "tz_name": "America/New_York"}
    await hass.config_entries.async_unload(config_entry.entry_id)


async def test_older_hubs(
    hass: HomeAssistant, fake_hub: FakeHub, config_entry: MockConfigEntry
) -> None:
    """Without `tz_name` the string alone counts; without `hub/time` nothing
    is sent and set-up goes on."""
    await hass.config.async_set_time_zone("Europe/Tallinn")
    fake_hub.time_names = False
    fake_hub.time = {"tz": TALLINN, "tz_name": None}
    await _setup(hass, config_entry, True)
    assert fake_hub.calls("hub/time") and fake_hub.calls("hub/time/set") == []
    await hass.config_entries.async_unload(config_entry.entry_id)
    await hass.async_block_till_done()
    fake_hub.time = None
    assert await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    assert fake_hub.calls("hub/time/set") == []
    assert config_entry.runtime_data.connected
    await hass.config_entries.async_unload(config_entry.entry_id)
