"""Connection, state store and HA registry sync for one hub."""

from __future__ import annotations

import asyncio
from collections.abc import Callable
import contextlib
from datetime import UTC, datetime, timedelta
import inspect
import logging
import time
from typing import Any

import aiohttp

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import device_registry as dr, entity_registry as er

from . import const
from .api import CannotConnect, HubApiError, HubClient, HubError, InvalidAuth, hub_url
from .catalog import CatalogUpdater
from .const import COMMAND_TIMEOUT, CONF_PATH, CONF_SYNC_TIME_ZONE, DEFAULT_PATH, DOMAIN, TOPICS
from .ota import OtaIndex, OtaStreams
from .time_zone import posix_tz, zone_name

_LOGGER = logging.getLogger(__name__)

SNAPSHOT_TIMEOUT = 15.0

type Listener = Callable[[], None]
type EventListener = Callable[[dict[str, Any]], None]
type PlatformAdder = Callable[[list[dict[str, Any]]], None]
type AutomationAdder = Callable[[list[str]], None]

_HAS_VIA_DEVICE_ID = (
    "via_device_id" in inspect.signature(dr.DeviceRegistry.async_get_or_create).parameters
)


def find_device(
    reg: dr.DeviceRegistry, identifier: tuple[str, str], entry_id: str
) -> dr.DeviceEntry | None:
    """The entry's device with `identifier` (any HA version)."""
    if hasattr(reg, "async_get_device_by_identifier"):
        return reg.async_get_device_by_identifier(identifier, entry_id)
    return reg.async_get_device(identifiers={identifier})


async def async_fetch_info(client: HubClient) -> dict[str, Any]:
    """`hub/info`, or what `auth_ok` tells on hubs without it."""
    try:
        info = await client.request("hub/info")
    except HubApiError as err:
        if err.code not in ("invalid_format", "unsupported"):
            raise
        info = None
    if not isinstance(info, dict):
        info = {}
    hello = client.hello
    info.setdefault("name", hello.get("hub"))
    info.setdefault("api_version", hello.get("api_version"))
    info.setdefault("features", hello.get("features", []))
    return info


def api_at_least(version: Any, major: int, minor: int) -> bool:
    """Whether an `api_version` (`major.minor`) is at least `major.minor`."""
    try:
        got = tuple(int(x) for x in str(version).split(".")[:2])
    except ValueError:
        return False
    return got >= (major, minor)


# Hub timestamps below this (2001-09-09) count seconds since the hub's boot:
# its clock is not synchronised yet (network/info `time`, API 1.1).
_EPOCH_FLOOR = 1e9


def automation_key(rule_id: str) -> str:
    """Listener key of an automation's entities (never an entity uid)."""
    return f"automation/{rule_id}"


def hub_version(name: str | None) -> str | None:
    """Firmware version from a hub name such as `zigbee-rs-hub 0.2.4`."""
    if not name:
        return None
    last = name.rsplit(" ", 1)[-1]
    return last if last[:1].isdigit() else None


class ZigbeeRsHub:
    """One hub: a persistent WebSocket client with reconnect and the hub's model."""

    def __init__(
        self, hass: HomeAssistant, entry: ConfigEntry, session: aiohttp.ClientSession
    ) -> None:
        """Hub for a config entry."""
        self.hass = hass
        self.entry = entry
        self._session = session
        self.url = hub_url(
            entry.data[CONF_HOST],
            entry.data[CONF_PORT],
            entry.data.get(CONF_PATH, DEFAULT_PATH),
        )
        self.client: HubClient | None = None
        self.connected = False
        self.info: dict[str, Any] = {}
        self.devices: dict[str, dict[str, Any]] = {}
        self.entities: dict[str, dict[str, Any]] = {}
        self.states: dict[str, dict[str, Any]] = {}
        self.seq = 0
        # network/info and the `network` topic (API 1.1); empty before.
        self.network: dict[str, Any] = {}
        self.permit_join_until: datetime | None = None
        self._clock_offset: float | None = None
        self._snapshot = asyncio.Event()
        self._listeners: dict[str, list[Listener]] = {}
        self._event_listeners: dict[str, list[EventListener]] = {}
        self._hub_listeners: list[Listener] = []
        self._adders: dict[str, PlatformAdder] = {}
        self._created: set[str] = set()
        # Devices HA's own "Delete device" removes from the registry.
        self._ha_removed: set[str] = set()
        self._stopping = False
        self._hub_device_id: str | None = None
        # Local automations of the hub (API 1.3), by rule ID.
        self.automations: dict[str, dict[str, Any]] = {}
        self._automation_adders: dict[str, AutomationAdder] = {}
        self._automations_created: set[tuple[str, str]] = set()
        self.catalog = CatalogUpdater(hass, self, session)
        # OTA Upgrade server (hub/info `ota`): ota/devices/list records.
        self.ota: dict[str, dict[str, Any]] = {}
        self.ota_settings: dict[str, Any] = {}
        self.ota_index = OtaIndex(session)
        self.ota_streams = OtaStreams(self, hass.config.path(".storage", f"{DOMAIN}_ota"))
        self._ota_adder: Callable[[list[str]], None] | None = None
        self._ota_created: set[str] = set()

    # Identity

    @property
    def hub_identifier(self) -> tuple[str, str]:
        """Device registry identifier of the hub itself."""
        return (DOMAIN, f"hub_{self.entry.entry_id}")

    @property
    def supports_network(self) -> bool:
        """API 1.1: network/info and the `network` topic."""
        return api_at_least(self.info.get("api_version"), 1, 1)

    @property
    def supports_automations(self) -> bool:
        """Local automations on the hub (API 1.3: `automations` in hub/info)."""
        return isinstance(self.info.get("automations"), dict)

    @property
    def supports_ota(self) -> bool:
        """The hub is an OTA Upgrade server (`ota` in hub/info)."""
        return isinstance(self.info.get("ota"), dict)

    @property
    def ota_store(self) -> bool:
        """The hub stores uploaded images (8 MB); else images are streamed."""
        return self.supports_ota and self.info["ota"].get("store") is not False

    @property
    def coordinator_ieee(self) -> str | None:
        """Coordinator IEEE (hub/info), if the hub tells it."""
        ieee = self.info.get("coordinator_ieee")
        return ieee if isinstance(ieee, str) and ieee else None

    @property
    def stopping(self) -> bool:
        """The entry is unloading: no reconnect follows."""
        return self._stopping

    # Connection

    async def async_connect(self) -> None:
        """Connect, authenticate, load the identity and the snapshot."""
        client = HubClient(self._session, self.url, self.entry.data[CONF_TOKEN], self._on_closed)
        client.on_message = self._on_message
        await client.connect()
        try:
            self.info = await async_fetch_info(client)
            self._snapshot = asyncio.Event()
            self.client = client
            topics = list(TOPICS)
            if self.supports_network:
                topics.append("network")
            if self.supports_automations:
                topics.append("automations")
            if self.supports_ota:
                topics.append("ota")
            await client.subscribe(topics, self._on_event)
            async with asyncio.timeout(SNAPSHOT_TIMEOUT):
                waits = {
                    asyncio.ensure_future(self._snapshot.wait()),
                    asyncio.ensure_future(client.closed.wait()),
                }
                try:
                    await asyncio.wait(waits, return_when=asyncio.FIRST_COMPLETED)
                finally:
                    for w in waits:
                        w.cancel()
            if not self._snapshot.is_set():
                raise CannotConnect("connection closed before the snapshot")
            if self.supports_automations:
                await self._load_automations(client)
            await self.catalog.async_connected(client)
        except TimeoutError as err:
            await client.close()
            self.client = None
            raise CannotConnect("no snapshot from the hub") from err
        except BaseException:
            await client.close()
            self.client = None
            raise
        self.connected = True
        if self.supports_ota:
            await self._async_load_ota()
            self.entry.async_create_background_task(
                self.hass, self.ota_streams.resume(), f"{DOMAIN} OTA streams"
            )
        self._update_hub_device()
        await self.async_sync_time_zone()
        _LOGGER.debug(
            "Connected to %s: %d devices, %d entities",
            self.url,
            len(self.devices),
            len(self.entities),
        )
        self._notify_all()

    @callback
    def _on_closed(self) -> None:
        # Failures while connecting are handled by the caller.
        if self._stopping or not self.connected:
            return
        _LOGGER.info("Connection to the zigbee-rs hub at %s lost", self.url)
        self.connected = False
        self._notify_all()
        self.entry.async_create_background_task(self.hass, self._reconnect(), f"{DOMAIN} reconnect")

    async def _reconnect(self) -> None:
        delay = const.RECONNECT_MIN
        while not self._stopping:
            await asyncio.sleep(delay)
            if self._stopping:
                return
            try:
                await self.async_connect()
            except InvalidAuth:
                _LOGGER.warning("The zigbee-rs hub refused the token")
                self.entry.async_start_reauth(self.hass)
                return
            except HubError as err:
                _LOGGER.debug("Reconnect to %s failed: %s", self.url, err)
                delay = min(delay * 2, const.RECONNECT_MAX)
            else:
                _LOGGER.info("Reconnected to the zigbee-rs hub at %s", self.url)
                return

    async def async_stop(self) -> None:
        """Close the connection for good."""
        self._stopping = True
        self.connected = False
        if self.client is not None:
            await self.client.close()

    # Events

    @callback
    def _on_event(self, event: dict[str, Any], seq: int) -> None:
        self.seq = max(self.seq, seq)
        topic = event.get("topic")
        if topic == "snapshot":
            self._apply_snapshot(event)
            self._snapshot.set()
        elif topic in ("device_added", "device_updated"):
            dev = event.get("device")
            if isinstance(dev, dict) and dev.get("id"):
                self._ha_removed.discard(dev["id"])
                self.devices[dev["id"]] = dev
                self._sync_device(dev)
                # State messages carry the device's availability too; the hub
                # need not resend them when only the device record changed.
                available = dev.get("available") is not False
                for uid, rec in self.entities.items():
                    if rec.get("device") == dev["id"] and uid in self.states:
                        self.states[uid] = {
                            **self.states[uid],
                            "available": available and not rec.get("orphaned"),
                        }
                self._notify_device(dev["id"])
                self._notify(f"{dev['id']}:ota")
        elif topic == "device_removed":
            self._remove_device(str(event.get("id")))
        elif topic in ("entity_added", "entity_updated"):
            ent = event.get("entity")
            if isinstance(ent, dict) and ent.get("uid"):
                self.entities[ent["uid"]] = ent
                self._create_entities([ent])
                self._notify(ent["uid"])
        elif topic == "entity_removed":
            self._remove_entity(str(event.get("uid")))
        elif topic == "entity_state":
            uid = event.get("uid")
            if isinstance(uid, str):
                self.states[uid] = event
                self._notify(uid)
        elif topic == "permit_join":
            self._set_permit_join(event.get("remaining_s"))
            self._notify_hub()
        elif topic == "network_state":
            self.network["state"] = event.get("state")
            self._notify_hub()
        elif topic == "ota_device":
            rec = event.get("device")
            if isinstance(rec, dict) and rec.get("device"):
                self.ota[rec["device"]] = rec
                if (rec.get("job") or {}).get("image") in self.ota_streams.sessions:
                    self.entry.async_create_background_task(
                        self.hass, self.ota_streams.finished(rec), f"{DOMAIN} OTA stream end"
                    )
                self._create_updates()
                self._notify(f"{rec['device']}:ota")
        elif topic == "entity_event":
            uid = event.get("uid")
            for cb in list(self._event_listeners.get(uid, [])):
                cb(event)
        elif topic in ("automation_added", "automation_updated"):
            rule = event.get("automation")
            if isinstance(rule, dict) and isinstance(rule.get("id"), str):
                self.automations[rule["id"]] = rule
                self._create_automation_entities([rule["id"]])
                self._notify(automation_key(rule["id"]))
        elif topic == "automation_removed":
            self._remove_automation(str(event.get("automation")))
        elif topic == "automation_run":
            rule = self.automations.get(str(event.get("automation")))
            if rule is not None:
                rule["last_result"] = event.get("result")
                rule["last_run"] = event.get("time")
                self._notify(automation_key(rule["id"]))

    @callback
    def _apply_snapshot(self, snap: dict[str, Any]) -> None:
        devices = {
            d["id"]: d for d in snap.get("devices") or [] if isinstance(d, dict) and d.get("id")
        }
        entities = {
            e["uid"]: e for e in snap.get("entities") or [] if isinstance(e, dict) and e.get("uid")
        }
        states = {
            s["uid"]: s for s in snap.get("states") or [] if isinstance(s, dict) and s.get("uid")
        }
        self.devices = devices
        # The hub keeps orphans (fingerprint changed) in the snapshot with
        # `orphaned: true`; an entity of a listed device that is missing was
        # removed (a definition changed) while HA was not listening.
        reg = er.async_get(self.hass)
        for ent in er.async_entries_for_config_entry(reg, self.entry.entry_id):
            uid = ent.unique_id
            if uid.count(":") >= 3 and uid.rsplit(":", 2)[0] in devices and uid not in entities:
                self._created.discard(uid)
                self.states.pop(uid, None)
                reg.async_remove(ent.entity_id)
        self.entities = entities
        self.states = states
        if isinstance(net := snap.get("network"), dict):
            self.network = net
            self._set_permit_join(net.get("permit_join_s"))
            if isinstance(t := net.get("time"), (int, float)):
                self._clock_offset = time.time() - t
        self._update_hub_device()
        for dev in devices.values():
            self._sync_device(dev)
        self._create_entities(list(entities.values()))

    # Automations

    async def _load_automations(self, client: HubClient) -> None:
        try:
            result = await client.request("automations/list")
        except HubError as err:
            _LOGGER.debug("automations/list failed: %s", err)
            return
        rules = result.get("rules") if isinstance(result, dict) else None
        if not isinstance(rules, list):
            return
        fresh = {r["id"]: r for r in rules if isinstance(r, dict) and isinstance(r.get("id"), str)}
        for rule_id in set(self.automations) - set(fresh):
            self._remove_automation(rule_id)
        self.automations = fresh
        self._create_automation_entities(list(fresh))
        for rule_id in fresh:
            self._notify(automation_key(rule_id))

    @callback
    def register_automation_platform(self, platform: str, adder: AutomationAdder) -> None:
        """`platform` creates one entity per automation through `adder`."""
        self._automation_adders[platform] = adder
        self._create_automation_entities(list(self.automations))

    @callback
    def _create_automation_entities(self, rule_ids: list[str]) -> None:
        for platform, adder in self._automation_adders.items():
            new = [r for r in rule_ids if (platform, r) not in self._automations_created]
            self._automations_created.update((platform, r) for r in new)
            if new:
                adder(new)

    @callback
    def _remove_automation(self, rule_id: str) -> None:
        self.automations.pop(rule_id, None)
        self._notify(automation_key(rule_id))
        reg = er.async_get(self.hass)
        for platform, suffix in (("switch", ""), ("button", "_run")):
            self._automations_created.discard((platform, rule_id))
            unique_id = f"{self.entry.entry_id}_automation_{rule_id}{suffix}"
            if entity_id := reg.async_get_entity_id(platform, DOMAIN, unique_id):
                reg.async_remove(entity_id)

    async def async_automation(self, request: str, rule_id: str) -> None:
        """`automations/<request>` (enable, disable, run) for one rule."""
        result = await self.async_request(f"automations/{request}", automation=rule_id)
        if request != "run" and isinstance(result, dict) and result.get("id") == rule_id:
            self.automations[rule_id] = result
            self._notify(automation_key(rule_id))

    # Device registry

    @callback
    def _update_hub_device(self) -> None:
        name = self.info.get("name") or "zigbee-rs hub"
        ncp = self.network.get("ncp") if isinstance(self.network.get("ncp"), dict) else {}
        hw = " ".join(
            x for x in (ncp.get("board"), f"(NCP {ncp['fw']})" if ncp.get("fw") else None) if x
        )
        self._hub_device_id = (
            dr.async_get(self.hass)
            .async_get_or_create(
                config_entry_id=self.entry.entry_id,
                identifiers={self.hub_identifier},
                manufacturer="zigbee-rs",
                model=self.info.get("ncp_product") or "zigbee-rs-hub",
                name=self.entry.title,
                sw_version=hub_version(name),
                hw_version=hw or None,
                serial_number=self.coordinator_ieee,
                configuration_url=self.url.replace("ws://", "http://", 1).rsplit("/", 1)[0] + "/",
            )
            .id
        )

    @callback
    def _sync_device(self, dev: dict[str, Any]) -> None:
        def text(v: Any) -> str | None:
            return None if v is None else str(v)

        via: dict[str, Any] = (
            {"via_device_id": self._hub_device_id}
            if _HAS_VIA_DEVICE_ID
            else {"via_device": self.hub_identifier}
        )
        dr.async_get(self.hass).async_get_or_create(
            config_entry_id=self.entry.entry_id,
            identifiers={(DOMAIN, dev["id"])},
            name=text(dev.get("name")) or dev["id"],
            manufacturer=text(dev.get("manufacturer")),
            model=text(dev.get("model")),
            model_id=text(dev.get("model_id")),
            sw_version=text(dev.get("sw_version")),
            hw_version=text(dev.get("hw_version")),
            serial_number=text(dev.get("ieee") or dev.get("src_id")),
            **via,
        )

    @callback
    def _remove_device(self, device_id: str) -> None:
        self.devices.pop(device_id, None)
        self.ota.pop(device_id, None)
        self._ota_created.discard(device_id)
        for uid in [u for u, e in self.entities.items() if e.get("device") == device_id]:
            self._remove_entity(uid)
        if device_id in self._ha_removed:
            return
        reg = dr.async_get(self.hass)
        if entry := find_device(reg, (DOMAIN, device_id), self.entry.entry_id):
            reg.async_remove_device(entry.id)

    @callback
    def _remove_entity(self, uid: str) -> None:
        rec = self.entities.pop(uid, None)
        self.states.pop(uid, None)
        self._created.discard(uid)
        self._notify(uid)
        if rec is None:
            return
        reg = er.async_get(self.hass)
        platform = rec.get("platform")
        if platform and (entity_id := reg.async_get_entity_id(platform, DOMAIN, uid)):
            reg.async_remove(entity_id)

    # Entities

    @callback
    def register_platform(self, platform: str, adder: PlatformAdder) -> None:
        """Platform `platform` creates entities through `adder`."""
        self._adders[platform] = adder
        self._create_entities(list(self.entities.values()))

    @callback
    def _create_entities(self, records: list[dict[str, Any]]) -> None:
        by_platform: dict[str, list[dict[str, Any]]] = {}
        for rec in records:
            uid = rec["uid"]
            platform = rec.get("platform")
            if uid in self._created or platform not in self._adders:
                continue
            if rec.get("device") not in self.devices:
                continue
            self._created.add(uid)
            by_platform.setdefault(platform, []).append(rec)
        for platform, recs in by_platform.items():
            self._adders[platform](recs)

    # OTA

    @callback
    def _on_message(self, msg: dict[str, Any]) -> None:
        if msg.get("type") == "ota/block_needed":
            self.ota_streams.block_needed(msg)

    async def _async_load_ota(self) -> None:
        """The OTA records of all devices (`ota/devices/list`)."""
        if self.client is None:
            return
        try:
            recs = await self.client.request("ota/devices/list")
            status = await self.client.request("ota/status")
        except HubError as err:
            _LOGGER.debug("ota/devices/list failed: %s", err)
            return
        if isinstance(status, dict) and isinstance(status.get("settings"), dict):
            self.ota_settings = status["settings"]
        self.ota = {
            r["device"]: r
            for r in recs or []
            if isinstance(r, dict) and isinstance(r.get("device"), str)
        }
        self._create_updates()

    async def async_refresh_ota_index(self, force: bool = False) -> None:
        """Fetch the hub's OTA image index (at most daily unless `force`)."""
        if await self.ota_index.refresh(self.ota_settings.get("index_url"), force):
            for device_id in list(self.ota):
                self._notify(f"{device_id}:ota")

    @callback
    def register_update_platform(self, adder: Callable[[list[str]], None]) -> None:
        """The update platform creates one entity per OTA device through `adder`."""
        self._ota_adder = adder
        self._create_updates()

    @callback
    def _create_updates(self) -> None:
        if self._ota_adder is None:
            return
        new = [d for d in self.ota if d not in self._ota_created and d in self.devices]
        if new:
            self._ota_created.update(new)
            self._ota_adder(new)

    @callback
    def add_listener(self, uid: str, cb: Listener) -> Callable[[], None]:
        """`cb` runs when the entity's record, state or availability changes."""
        self._listeners.setdefault(uid, []).append(cb)
        return lambda: self._listeners.get(uid, []).remove(cb)

    @callback
    def add_event_listener(self, uid: str, cb: EventListener) -> Callable[[], None]:
        """`cb` gets the `entity_event`s of an event entity."""
        self._event_listeners.setdefault(uid, []).append(cb)
        return lambda: self._event_listeners.get(uid, []).remove(cb)

    @callback
    def add_hub_listener(self, cb: Listener) -> Callable[[], None]:
        """`cb` runs when the connection state changes."""
        self._hub_listeners.append(cb)
        return lambda: self._hub_listeners.remove(cb)

    @callback
    def _notify(self, uid: str) -> None:
        for cb in list(self._listeners.get(uid, [])):
            cb()

    @callback
    def _notify_device(self, device_id: str) -> None:
        for uid, rec in self.entities.items():
            if rec.get("device") == device_id:
                self._notify(uid)

    @callback
    def _notify_hub(self) -> None:
        for cb in list(self._hub_listeners):
            cb()

    @callback
    def _notify_all(self) -> None:
        for uid in list(self._listeners):
            self._notify(uid)
        self._notify_hub()

    @callback
    def _set_permit_join(self, remaining: Any) -> None:
        if isinstance(remaining, (int, float)) and remaining > 0:
            self.network["permit_join_s"] = remaining
            self.permit_join_until = datetime.now(UTC) + timedelta(seconds=remaining)
        else:
            self.network["permit_join_s"] = 0
            self.permit_join_until = None

    def hub_time(self, value: Any) -> datetime | None:
        """A hub timestamp (s) as a datetime, also before the hub's clock is set."""
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            return None
        if value < _EPOCH_FLOOR:
            if self._clock_offset is None:
                return None
            value += self._clock_offset
        return datetime.fromtimestamp(value, UTC)

    def entity_available(self, uid: str) -> bool:
        """Connected, the device is available and the entity is not orphaned."""
        rec = self.entities.get(uid)
        if not self.connected or rec is None or rec.get("orphaned"):
            return False
        dev = self.devices.get(rec.get("device", ""))
        if dev is None or dev.get("available") is False:
            return False
        state = self.states.get(uid)
        return state is None or state.get("available", True) is not False

    # Requests

    async def async_request(
        self, req_type: str, timeout: float | None = None, **params: Any
    ) -> Any:
        """A request; hub errors become `HomeAssistantError`."""
        if self.client is None or not self.connected:
            raise HomeAssistantError(f"zigbee-rs hub at {self.url} is not connected")
        try:
            if timeout is None:
                return await self.client.request(req_type, **params)
            return await self.client.request(req_type, timeout=timeout, **params)
        except HubApiError as err:
            raise HomeAssistantError(f"{req_type} failed: {err}") from err
        except HubError as err:
            raise HomeAssistantError(f"{req_type} failed: {err}") from err

    async def async_command(
        self, uid: str, command: str, params: dict[str, Any] | None = None
    ) -> Any:
        """`entities/command` with HA's service and parameter names."""
        return await self.async_request(
            "entities/command",
            timeout=COMMAND_TIMEOUT,
            uid=uid,
            command=command,
            params=params or {},
        )

    async def async_sync_time_zone(self) -> None:
        """Give the hub Home Assistant's time zone (option `sync_time_zone`).

        Its IANA name and the POSIX TZ string of its TZif file, when the
        hub's differ. Hubs without `hub/time` are left alone.
        """
        if not self.entry.options.get(CONF_SYNC_TIME_ZONE) or self.client is None:
            return
        name = self.hass.config.time_zone
        if not zone_name(name):
            return
        tz = await self.hass.async_add_executor_job(posix_tz, name)
        if tz is None:
            _LOGGER.warning("No POSIX TZ string for the time zone %s: the hub keeps its own", name)
            return
        try:
            now = await self.client.request("hub/time")
            if not isinstance(now, dict):
                return
            # Hubs before `tz_name` compare by the string only.
            if now.get("tz") == tz and now.get("tz_name", name) == name:
                return
            await self.client.request("hub/time/set", tz=tz, tz_name=name)
        except HubApiError as err:
            if err.code not in ("invalid_format", "unsupported"):
                _LOGGER.warning("Could not set the hub's time zone to %s (%s): %s", name, tz, err)
            return
        except HubError as err:
            _LOGGER.debug("Time zone sync with %s failed: %s", self.url, err)
            return
        _LOGGER.info("The hub's time zone is now %s (%s)", name, tz)

    async def async_permit_join(
        self, duration: int, include_gp: bool = False, target: str | None = None
    ) -> None:
        """Open (or close, duration 0) the network for joining."""
        params: dict[str, Any] = {"duration_s": duration, "include_gp": include_gp}
        if target:
            params["target"] = target
        await self.async_request("network/permit_join", **params)
        if not self.supports_network:
            return
        # Hubs need not send a `permit_join` event for a window the host
        # opened: assume the requested one (whole network only), then take
        # the hub's own count from network/info. Later events still win.
        if not target:
            self._set_permit_join(duration)
            self._notify_hub()
        await self.async_refresh_network()

    async def async_refresh_network(self) -> None:
        """Reconcile the network facts with network/info (API 1.1)."""
        if self.client is None or not self.connected:
            return
        try:
            net = await self.client.request("network/info")
        except HubError as err:
            _LOGGER.debug("network/info failed: %s", err)
            return
        if not isinstance(net, dict):
            return
        self.network.update(net)
        if "permit_join_s" in net:
            self._set_permit_join(net.get("permit_join_s"))
        self._notify_hub()

    async def async_remove_device(self, device_id: str) -> None:
        """Remove a device from the network and the hub (HA removes its entry)."""
        self._ha_removed.add(device_id)
        try:
            await self.async_request("devices/remove", timeout=COMMAND_TIMEOUT, device=device_id)
        except HomeAssistantError:
            self._ha_removed.discard(device_id)
            raise
        self._remove_device(device_id)

    async def async_device_diagnostics(self, device_id: str) -> Any:
        """`devices/diagnostics`, `None` when unavailable."""
        with contextlib.suppress(HomeAssistantError):
            return await self.async_request("devices/diagnostics", device=device_id)
        return None
