"""Definitions catalog: release download, chunked upload, resume, auto fetch."""

from __future__ import annotations

import asyncio
import base64
from collections.abc import AsyncGenerator, Callable
from datetime import timedelta
import json
from unittest.mock import patch

import pytest
from pytest_homeassistant_custom_component.common import (
    MockConfigEntry,
    async_fire_time_changed,
)

from custom_components.zigbee_rs_hub.const import DOMAIN
from custom_components.zigbee_rs_hub.diagnostics import async_get_config_entry_diagnostics
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from homeassistant.helpers import entity_registry as er
from homeassistant.util import dt as dt_util

from .fake_hub import (
    BANK,
    CATALOG_DATE,
    CATALOG_SERIAL,
    SENSOR_DEV,
    SHARD,
    UPLOAD_CHUNK_MAX,
    WEB,
    FakeCatalog,
    FakeHub,
    ReleaseServer,
    make_release,
)

VERSION = f"{CATALOG_DATE}.{CATALOG_SERIAL}"


@pytest.fixture
async def release() -> AsyncGenerator[ReleaseServer]:
    """The catalog mirror with a release."""
    server = ReleaseServer(make_release())
    await server.start()
    yield server
    await server.stop()


@pytest.fixture
def catalog(fake_hub: FakeHub, release: ReleaseServer) -> FakeCatalog:
    """The fake hub with a catalog store whose mirror is `release`."""
    fake_hub.catalog = FakeCatalog()
    fake_hub.catalog.config["base_url"] = release.base_url
    fake_hub.catalog.missing = [{"device": SENSOR_DEV, "shards": [SHARD]}]
    return fake_hub.catalog


@pytest.fixture(autouse=True)
def no_debounce() -> None:
    """Handle `definition_needed` at once."""
    with patch("custom_components.zigbee_rs_hub.const.CATALOG_FETCH_DEBOUNCE", 0):
        yield


def sensor_id(hass: HomeAssistant, entry: MockConfigEntry) -> str:
    entity_id = er.async_get(hass).async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_catalog_version"
    )
    assert entity_id is not None
    return entity_id


def uploads(fake_hub: FakeHub, target: str) -> list[dict]:
    return [r for r in fake_hub.calls("catalog/upload_chunk") if r["target"] == target]


def commits(fake_hub: FakeHub, target: str) -> list[dict]:
    return [r for r in fake_hub.calls("catalog/commit") if r["target"] == target]


async def wait_for(hass: HomeAssistant, cond: Callable[[], bool], timeout: float = 5.0) -> None:
    async with asyncio.timeout(timeout):
        while not cond():
            await asyncio.sleep(0.01)
            await hass.async_block_till_done()


async def test_full_update(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    setup_entry: MockConfigEntry,
) -> None:
    sensor = sensor_id(hass, setup_entry)
    assert hass.states.get(sensor).state == "none"
    assert hass.states.get(sensor).attributes["missing_definitions"] == 1
    assert "catalog" in fake_hub.calls("subscribe")[-1]["topics"]

    await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
    await hass.async_block_till_done()

    assert release.requests == ["manifest.cose", BANK, WEB, f"shard-{SHARD:02x}.cbor"]
    files = make_release()
    chunks = uploads(fake_hub, "bank")
    assert [c["offset"] for c in chunks] == list(range(0, len(files[BANK]), UPLOAD_CHUNK_MAX))
    assert all(len(base64.b64decode(c["data"])) == UPLOAD_CHUNK_MAX for c in chunks[:-1])
    assert b"".join(base64.b64decode(c["data"]) for c in chunks) == files[BANK]
    assert chunks[0]["size"] == len(files[BANK])
    assert chunks[0]["sha256"] == json.loads(files["manifest.cose"])["bank"]["sha256"]
    assert catalog.forced == [False]
    assert catalog.installed["generation"] == 5
    assert catalog.web_sha is not None
    assert commits(fake_hub, "shard") == [
        {
            "id": commits(fake_hub, "shard")[0]["id"],
            "type": "catalog/commit",
            "target": "shard",
            "shard": SHARD,
        }
    ]
    assert catalog.missing == []

    state = hass.states.get(sensor)
    assert state.state == VERSION
    assert state.attributes["generation"] == 5
    assert state.attributes["fetched_definitions"] == 1
    assert state.attributes["missing_definitions"] == 0
    assert state.attributes["last_error"] is None

    diag = await async_get_config_entry_diagnostics(hass, setup_entry)
    assert diag["catalog"]["supported"] is True
    assert diag["catalog"]["status"]["installed"]["generation"] == 5

    # Forced: the bank commit passes the anti-rollback flag.
    catalog.manifest = None
    catalog.installed = None
    await hass.services.async_call(
        DOMAIN,
        "update_catalog",
        {"config_entry_id": setup_entry.entry_id, "force": True},
        blocking=True,
    )
    assert catalog.forced == [False, True]


async def test_nothing_to_do(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    setup_entry: MockConfigEntry,
) -> None:
    catalog.install(make_release())
    catalog.missing = []
    await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
    assert release.requests == ["manifest.cose"]
    assert [c["target"] for c in fake_hub.calls("catalog/commit")] == ["manifest"]
    await hass.async_block_till_done()
    assert hass.states.get(sensor_id(hass, setup_entry)).state == VERSION


async def test_base_url_override(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    setup_entry: MockConfigEntry,
) -> None:
    catalog.install(make_release())
    catalog.missing = []
    catalog.config["base_url"] = "http://127.0.0.1:1/latest"
    with pytest.raises(HomeAssistantError, match=r"manifest\.cose"):
        await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
    await hass.services.async_call(
        DOMAIN, "update_catalog", {"base_url": f"{release.base_url}/"}, blocking=True
    )
    assert release.requests == ["manifest.cose"]
    # The hub's setting is left alone.
    assert catalog.config["base_url"] == "http://127.0.0.1:1/latest"


async def test_resume_after_dropped_connection(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    setup_entry: MockConfigEntry,
) -> None:
    # Manifest chunk, two bank chunks; the second is stored, never answered.
    catalog.drop_after_chunks = 3
    await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
    assert fake_hub.connections == 2
    # The resumed upload continues after the stored chunk, not from 0.
    assert [c["offset"] for c in uploads(fake_hub, "bank")] == list(
        range(0, len(make_release()[BANK]), UPLOAD_CHUNK_MAX)
    )
    assert catalog.installed["generation"] == 5
    assert catalog.web_sha is not None


async def test_resume_from_an_earlier_run(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    setup_entry: MockConfigEntry,
) -> None:
    files = make_release()
    catalog.manifest = json.loads(files["manifest.cose"])
    catalog.upload = {"target": "bank", "data": bytearray(files[BANK][:12288]), "size": None}
    await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
    # The bank continues before the manifest upload would replace it.
    assert [c["offset"] for c in uploads(fake_hub, "bank")] == list(
        range(12288, len(files[BANK]), UPLOAD_CHUNK_MAX)
    )
    assert [c["target"] for c in fake_hub.calls("catalog/commit")][:2] == ["bank", "manifest"]
    assert catalog.installed["generation"] == 5


async def test_mirror_file_mismatch(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    setup_entry: MockConfigEntry,
) -> None:
    release.files[BANK] = b"not the bank"
    with pytest.raises(HomeAssistantError, match=r"bank-4m\.zdb"):
        await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
    assert uploads(fake_hub, "bank") == []
    await hass.async_block_till_done()
    assert "bank-4m.zdb" in hass.states.get(sensor_id(hass, setup_entry)).attributes["last_error"]


async def test_definition_needed(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    setup_entry: MockConfigEntry,
) -> None:
    catalog.install(make_release())
    catalog.config["auto_fetch"] = True
    event = {"topic": "definition_needed", "device": SENSOR_DEV, "shards": [SHARD]}
    await fake_hub.push("catalog", event)
    await wait_for(hass, lambda: bool(commits(fake_hub, "shard")))
    assert commits(fake_hub, "shard")[0]["shard"] == SHARD
    assert release.requests == [f"shard-{SHARD:02x}.cbor"]
    await wait_for(hass, lambda: catalog.missing == [])

    # One attempt per device per hour.
    await fake_hub.push("catalog", event)
    await asyncio.sleep(0.1)
    await hass.async_block_till_done()
    assert len(commits(fake_hub, "shard")) == 1

    # Off on the hub: nothing fetched.
    catalog.config["auto_fetch"] = False
    other = {"topic": "definition_needed", "device": "zb:0000000000000001", "shards": [SHARD]}
    await fake_hub.push("catalog", other)
    await asyncio.sleep(0.1)
    await hass.async_block_till_done()
    assert len(commits(fake_hub, "shard")) == 1


async def test_auto_update(
    hass: HomeAssistant,
    fake_hub: FakeHub,
    catalog: FakeCatalog,
    release: ReleaseServer,
    config_entry: MockConfigEntry,
) -> None:
    catalog.config["auto_update"] = True
    catalog.config["auto_fetch"] = True
    config_entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(config_entry.entry_id)
    await hass.async_block_till_done()
    assert release.requests == []

    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=121))
    await wait_for(hass, lambda: catalog.installed is not None and catalog.missing == [])
    assert catalog.web_sha is not None

    # Daily; off on the hub: no download.
    catalog.config["auto_update"] = False
    catalog.config["auto_fetch"] = False
    count = len(release.requests)
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(days=1, seconds=200))
    await asyncio.sleep(0.1)
    await hass.async_block_till_done()
    assert len(release.requests) == count

    assert await hass.config_entries.async_unload(config_entry.entry_id)
    await hass.async_block_till_done()
    catalog.config["auto_update"] = True
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(days=3))
    await hass.async_block_till_done()
    assert len(release.requests) == count


async def test_hub_without_catalog(
    hass: HomeAssistant, fake_hub: FakeHub, setup_entry: MockConfigEntry
) -> None:
    assert (
        er.async_get(hass).async_get_entity_id(
            "sensor", DOMAIN, f"{setup_entry.entry_id}_catalog_version"
        )
        is None
    )
    assert all("catalog" not in s["topics"] for s in fake_hub.calls("subscribe"))
    with pytest.raises(ServiceValidationError):
        await hass.services.async_call(DOMAIN, "update_catalog", {}, blocking=True)
