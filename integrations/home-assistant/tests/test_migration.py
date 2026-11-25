"""Migration from ZHA: zigbee.db read, the import document, the services."""

from __future__ import annotations

import json
from pathlib import Path
import sqlite3
from typing import Any

import pytest
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.zigbee_rs_hub import migrate
from custom_components.zigbee_rs_hub.const import DOMAIN
from custom_components.zigbee_rs_hub.hub import find_device
from homeassistant.auth.models import User
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import Context, HomeAssistant
from homeassistant.exceptions import ServiceValidationError, Unauthorized
from homeassistant.helpers import area_registry as ar, device_registry as dr

from .fake_hub import SENSOR_DEV, FakeHub

COORD = "00:0b:57:ff:fe:aa:00:01"
LIGHT = "00:17:88:01:00:f1:e2:d3"
SENSOR = "00:15:8d:00:03:c4:d5:e6"

# zigpy's own backup (`NetworkBackup.as_dict()`), generated values.
ZIGPY_BACKUP = {
    "version": 1,
    "backup_time": "2026-11-20T10:00:00+00:00",
    "network_info": {
        "extended_pan_id": "a1:b2:c3:d4:e5:f6:07:18",
        "pan_id": "0F3A",
        "nwk_update_id": 4,
        "nwk_manager_id": "0000",
        "channel": 20,
        "channel_mask": [20],
        "security_level": 5,
        "network_key": {
            "key": "88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77",
            "tx_counter": 123456,
            "rx_counter": 0,
            "seq": 3,
            "partner_ieee": "ff:ff:ff:ff:ff:ff:ff:ff",
        },
        "tc_link_key": {
            "key": "5a:69:67:42:65:65:41:6c:6c:69:61:6e:63:65:30:39",
            "tx_counter": 4096,
            "rx_counter": 0,
            "seq": 0,
            "partner_ieee": COORD,
        },
        "key_table": [
            {
                "key": "01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef",
                "tx_counter": 12,
                "rx_counter": 34,
                "seq": 1,
                "partner_ieee": SENSOR,
            }
        ],
        "children": [SENSOR],
        "route_table": {},
        "tx_power": 8,
        "nwk_addresses": {SENSOR: "04B2", LIGHT: "7A10"},
        "stack_specific": {"ezsp": {"hashed_tclk": "33" * 16}},
        "metadata": {"ezsp": {"stack_version": 13}},
        "source": "bellows@0.47.0",
    },
    "node_info": {
        "nwk": "0000",
        "ieee": COORD,
        "logical_type": "coordinator",
        "model": "MGM24",
        "manufacturer": "Silicon Labs",
        "version": "7.4.5.0",
    },
}


def make_db(path: Path, v: int) -> None:
    """zigpy's tables of schema `v` with generated devices (v17: typed values)."""
    db = sqlite3.connect(path)
    dev = "zigbee_devices" if v >= 17 else "devices"
    typed = ", data_type INTEGER" if v >= 17 else ""
    db.executescript(
        f"""
        CREATE TABLE {dev}_v{v} (ieee ieee, nwk INTEGER, status INTEGER, last_seen REAL);
        CREATE TABLE endpoints_v{v} (ieee ieee, endpoint_id INTEGER, profile_id INTEGER, device_type INTEGER, status INTEGER);
        CREATE TABLE clusters_v{v} (ieee ieee, endpoint_id INTEGER, cluster_type INTEGER, cluster_id INTEGER);
        CREATE TABLE attributes_cache_v{v} (ieee ieee, endpoint_id INTEGER, cluster_type INTEGER, cluster_id INTEGER,
            attr_id INTEGER, manufacturer_code INTEGER, status INTEGER{typed}, value BLOB, last_updated REAL);
        CREATE TABLE node_descriptors_v{v} (ieee ieee, logical_type INTEGER, complex_descriptor_available INTEGER,
            user_descriptor_available INTEGER, reserved INTEGER, aps_flags INTEGER, frequency_band INTEGER,
            mac_capability_flags INTEGER, manufacturer_code INTEGER, maximum_buffer_size INTEGER,
            maximum_incoming_transfer_size INTEGER, server_mask INTEGER, maximum_outgoing_transfer_size INTEGER,
            descriptor_capability_field INTEGER);
        CREATE TABLE groups_v{v} (group_id INTEGER, name TEXT);
        CREATE TABLE group_members_v{v} (group_id INTEGER, ieee ieee, endpoint_id INTEGER);
        CREATE TABLE network_backups_v{v} (id INTEGER PRIMARY KEY AUTOINCREMENT, backup_json TEXT);
        CREATE TABLE gp_devices_v{v} (ieee ieee, application_id INTEGER, src_id INTEGER, last_seen REAL);
        CREATE TABLE endpoints_v15 (ieee ieee);
        """
    )
    for ieee, nwk in ((COORD, 0), (LIGHT, 0x7A10), (SENSOR, 0x04B2)):
        db.execute(f"INSERT INTO {dev}_v{v} VALUES (?, ?, 2, 0)", (ieee, nwk))
    for ieee, ep, prof, dt, ins, outs in (
        (LIGHT, 11, 260, 269, [0, 3, 6, 8, 768], [25]),
        (SENSOR, 1, 260, 24321, [0, 3, 1026], [25]),
    ):
        db.execute(f"INSERT INTO endpoints_v{v} VALUES (?, ?, ?, ?, 1)", (ieee, ep, prof, dt))
        for t, lst in ((0, ins), (1, outs)):
            for c in lst:
                db.execute(f"INSERT INTO clusters_v{v} VALUES (?, ?, ?, ?)", (ieee, ep, t, c))
    for ieee, lt, mac in ((COORD, 0, 0x8E), (LIGHT, 1, 0x8E), (SENSOR, 2, 0x80)):
        db.execute(
            f"INSERT INTO node_descriptors_v{v} VALUES (?, ?, 0, 0, 0, 0, 8, ?, 4107, 82, 82, 11264, 82, 0)",
            (ieee, lt, mac),
        )

    def s(t: str) -> tuple[Any, tuple[int, bytes]]:
        return t, (0x42, bytes([len(t)]) + t.encode())

    def u(n: int, ty: int, size: int) -> tuple[Any, tuple[int, bytes]]:
        return n, (ty, n.to_bytes(size, "little"))

    for ieee, ep, c, a, (plain, (ty, raw)) in (
        (LIGHT, 11, 0, 4, s("Signify Netherlands B.V.")),
        (LIGHT, 11, 0, 5, s("LCT015")),
        (LIGHT, 11, 0, 7, u(1, 0x30, 1)),
        (LIGHT, 11, 0x0300, 0x400A, u(31, 0x19, 2)),
        (LIGHT, 11, 0x0300, 0x0003, u(1000, 0x21, 2)),
        (SENSOR, 1, 0, 5, s("lumi.weather")),
    ):
        if v >= 17:
            db.execute(
                f"INSERT INTO attributes_cache_v{v} VALUES (?, ?, 0, ?, ?, NULL, 0, ?, ?, 0)",
                (ieee, ep, c, a, ty, raw),
            )
        else:
            db.execute(
                f"INSERT INTO attributes_cache_v{v} VALUES (?, ?, 0, ?, ?, NULL, 0, ?, 0)",
                (ieee, ep, c, a, plain),
            )
    db.execute(f"INSERT INTO groups_v{v} VALUES (4097, 'Hall')")
    db.execute(f"INSERT INTO group_members_v{v} VALUES (4097, ?, 11), (4097, ?, 1)", (LIGHT, COORD))
    db.execute(
        f"INSERT INTO network_backups_v{v} (backup_json) VALUES ('{{}}'), (?)",
        (json.dumps(ZIGPY_BACKUP),),
    )
    db.execute(f"INSERT INTO gp_devices_v{v} VALUES ('00:00:00:00:01:78:80:ff', 0, 1, 0)")
    db.commit()
    db.close()


@pytest.mark.parametrize("v", [16, 17])
def test_zha_database_becomes_the_import_document(tmp_path: Path, v: int) -> None:
    path = tmp_path / "zigbee.db"
    make_db(path, v)
    zha = migrate.read_zha_db(str(path))
    assert zha["version"] == v
    doc = migrate.document(zha, None, {"0017880100f1e2d3": ("Hall light", "Hall")})
    assert doc["format"] == "zigbee-rs-hub/import"
    assert doc["network"]["extended_pan_id"] == "a1b2c3d4e5f60718"
    by = {d["ieee"]: d for d in doc["devices"]}
    assert by["000b57fffeaa0001"]["type"] == "coordinator"
    light = by["0017880100f1e2d3"]
    assert (light["name"], light["area"], light["type"], light["model"]) == (
        "Hall light",
        "Hall",
        "router",
        "LCT015",
    )
    assert light["power_source"] == 1
    assert light["endpoints"] == [
        {"id": 11, "profile": 260, "device_id": 269, "in": [0, 3, 6, 8, 768], "out": [25]}
    ]
    assert light["attributes"] == [{"ep": 11, "cluster": 768, "attr": 16394, "value": 31}]
    assert light["node_descriptor"]["mac_capabilities"] == 0x8E
    sensor = by["00158d0003c4d5e6"]
    assert (sensor["type"], sensor["model"], sensor["nwk"]) == (
        "end_device",
        "lumi.weather",
        0x04B2,
    )
    assert "name" not in sensor
    assert by["00000000017880ff"]["type"] == "green_power"
    assert doc["groups"] == [
        {
            "id": 4097,
            "name": "Hall",
            "members": [
                {"ieee": "0017880100f1e2d3", "ep": 11},
                {"ieee": "000b57fffeaa0001", "ep": 1},
            ],
        }
    ]


def test_zigpy_backup_becomes_the_ocb_zigpy_exports() -> None:
    o = migrate.zigpy_to_ocb(ZIGPY_BACKUP)
    assert (o["coordinator_ieee"], o["pan_id"], o["extended_pan_id"]) == (
        "000b57fffeaa0001",
        "0f3a",
        "a1b2c3d4e5f60718",
    )
    assert o["network_key"] == {
        "key": "8899aabbccddeeff0011223344556677",
        "sequence_number": 3,
        "frame_counter": 123456,
    }
    assert o["metadata"]["internal"]["network"]["tc_link_key"]["frame_counter"] == 4096
    assert o["stack_specific"]["ezsp"]["hashed_tclk"] == "33" * 16
    assert o["devices"][0] == {
        "ieee_address": "00158d0003c4d5e6",
        "nwk_address": "04b2",
        "is_child": True,
        "link_key": {"key": "0123456789abcdef0123456789abcdef", "tx_counter": 12, "rx_counter": 34},
    }
    assert migrate.zigpy_to_ocb(o) is o


def test_a_document_goes_in_small_requests() -> None:
    doc = {
        "format": "zigbee-rs-hub/import",
        "network": {
            "pan_id": "1a62",
            "devices": [{"ieee_address": f"{i:016x}"} for i in range(300)],
        },
        "devices": [{"ieee": f"{i:016x}", "name": "x" * 200} for i in range(100)],
        "groups": [
            {
                "id": i,
                "name": "Room",
                "members": [{"ieee": f"{m:016x}", "ep": 1} for m in range(10)],
            }
            for i in range(60)
        ],
    }
    reqs = migrate.requests(doc, {"force": True})
    assert reqs[0]["document"]["network"]["devices"] == []
    assert reqs[0]["force"] is True and "devices" not in reqs[0]["document"]
    assert "groups" not in reqs[0]["document"]
    assert sum(len(r.get("network_devices", [])) for r in reqs) == 300
    assert sum(len(r.get("devices", [])) for r in reqs) == 100
    assert sum(len(r.get("groups", [])) for r in reqs) == 60
    assert all(len(json.dumps(r)) < migrate.BATCH + 512 for r in reqs[1:])
    assert [r["more"] for r in reqs] == [True] * (len(reqs) - 1) + [False]
    assert doc["network"]["devices"], "the document itself is not changed"


def zha_devices(hass: HomeAssistant, loaded: bool) -> MockConfigEntry:
    """ZHA's config entry and its devices in the registry (the light named and in Hall)."""
    zha = MockConfigEntry(
        domain="zha",
        title="ZHA",
        state=ConfigEntryState.LOADED if loaded else ConfigEntryState.NOT_LOADED,
    )
    zha.add_to_hass(hass)
    reg = dr.async_get(hass)
    hall = ar.async_get(hass).async_create("Hall")
    light = reg.async_get_or_create(
        config_entry_id=zha.entry_id, identifiers={("zha", LIGHT)}, name="Signify LCT015"
    )
    reg.async_update_device(light.id, name_by_user="Hall light", area_id=hall.id)
    reg.async_get_or_create(
        config_entry_id=zha.entry_id, identifiers={("zha", SENSOR)}, name="LUMI lumi.weather"
    )
    return zha


async def test_import_from_zha_previews_then_applies_with_zha_stopped(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry, tmp_path: Path
) -> None:
    path = tmp_path / "zigbee.db"
    make_db(path, 17)
    zha = zha_devices(hass, loaded=True)
    preview = await hass.services.async_call(
        DOMAIN, "import_from_zha", {"database": str(path)}, blocking=True, return_response=True
    )
    assert preview["result"]["network"]["action"] == "restore"
    assert preview["result"]["restore_check"] == "ok"
    calls = fake_hub.calls("network/import/preview")
    doc = calls[0]["document"]
    assert doc["source"]["kind"] == "zha"
    assert doc["network"]["extended_pan_id"] == "a1b2c3d4e5f60718"
    assert doc["network"]["devices"] == [], "the network part's devices follow in batches"
    sent = [d for c in calls for d in c.get("devices", [])]
    light = next(d for d in sent if d["ieee"] == "0017880100f1e2d3")
    assert (light["name"], light["area"]) == ("Hall light", "Hall")
    assert sum(len(c.get("network_devices", [])) for c in calls) == 2
    assert {r["ieee"]: r["status"] for r in preview["rows"]}["000b57fffeaa0001"] == "skip"
    assert not fake_hub.calls("network/import")

    with pytest.raises(ServiceValidationError, match="ZHA is running"):
        await hass.services.async_call(
            DOMAIN, "import_from_zha", {"database": str(path), "apply": True}, blocking=True
        )
    assert not fake_hub.calls("network/import")

    hass.config_entries.async_update_entry(zha)
    zha.mock_state(hass, ConfigEntryState.NOT_LOADED)
    applied = await hass.services.async_call(
        DOMAIN,
        "import_from_zha",
        {"database": str(path), "apply": True, "force": True, "reconfigure": "never"},
        blocking=True,
        return_response=True,
    )
    first = fake_hub.calls("network/import")[0]
    assert (first["force"], first["reconfigure"]) == (True, "never")
    assert "confirm" not in first
    assert applied["result"]["devices"]["new"] == 2
    assert "backup" not in applied["result"], "keys stay out of the service response"
    assert applied["areas"] == 1
    reg = dr.async_get(hass)
    ours = find_device(reg, (DOMAIN, "zb:0017880100f1e2d3"), setup_entry.entry_id)
    assert ours is not None and ours.name == "Hall light"
    assert ours.area_id == ar.async_get(hass).async_get_area_by_name("Hall").id


async def test_import_from_zha_refuses_what_it_cannot_read(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry, tmp_path: Path
) -> None:
    with pytest.raises(ServiceValidationError, match="no ZHA database"):
        await hass.services.async_call(
            DOMAIN, "import_from_zha", {"database": str(tmp_path / "missing.db")}, blocking=True
        )
    empty = tmp_path / "empty.db"
    sqlite3.connect(empty).close()
    with pytest.raises(Exception, match="not a zigpy database"):
        await hass.services.async_call(
            DOMAIN, "import_from_zha", {"database": str(empty)}, blocking=True
        )
    assert not fake_hub.calls("network/import/preview")


@pytest.mark.parametrize("service", ["import_from_zha", "copy_names_areas"])
async def test_migration_services_are_for_administrators(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    setup_entry: MockConfigEntry,
    hass_read_only_user: User,
    tmp_path: Path,
    service: str,
) -> None:
    path = tmp_path / "zigbee.db"
    make_db(path, 17)
    data = {"database": str(path), "apply": True} if service == "import_from_zha" else {}
    with pytest.raises(Unauthorized):
        await hass.services.async_call(
            DOMAIN, service, data, blocking=True, context=Context(user_id=hass_read_only_user.id)
        )
    assert not fake_hub.calls("network/import/preview")
    assert not fake_hub.calls("network/import")


async def test_copy_names_areas_from_zigbee2mqtt_devices(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    mqtt = MockConfigEntry(domain="mqtt", title="MQTT")
    mqtt.add_to_hass(hass)
    reg = dr.async_get(hass)
    kitchen = ar.async_get(hass).async_create("Kitchen")
    z2m = reg.async_get_or_create(
        config_entry_id=mqtt.entry_id,
        identifiers={("mqtt", "zigbee2mqtt_0x" + SENSOR_DEV[3:])},
        name="kitchen_motion",
    )
    reg.async_update_device(z2m.id, name_by_user="Kitchen motion", area_id=kitchen.id)
    r = await hass.services.async_call(
        DOMAIN, "copy_names_areas", {}, blocking=True, return_response=True
    )
    assert r == {"names": 1, "areas": 1}
    ours = find_device(reg, (DOMAIN, SENSOR_DEV), setup_entry.entry_id)
    assert (ours.name_by_user, ours.area_id) == ("Kitchen motion", kitchen.id)
    # Kept unless overwrite.
    reg.async_update_device(ours.id, name_by_user="Mine")
    r = await hass.services.async_call(
        DOMAIN, "copy_names_areas", {}, blocking=True, return_response=True
    )
    assert r == {"names": 0, "areas": 0}
    assert find_device(reg, (DOMAIN, SENSOR_DEV), setup_entry.entry_id).name_by_user == "Mine"
