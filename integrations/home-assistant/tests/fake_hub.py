"""A fake zigbee-rs hub: the WebSocket JSON API on a local aiohttp server."""

from __future__ import annotations

import base64
import copy
import hashlib
import json
from typing import Any

from aiohttp import WSMsgType, web

TOKEN = "secret-token"
COORDINATOR_IEEE = "00124b0001020304"

SENSOR_DEV = "zb:00158d0001a2b3c4"
LIGHT_DEV = "zb:0017880100aabbcc"
REMOTE_DEV = "zb:00158d00aabbccdd"

TEMP_UID = f"{SENSOR_DEV}:1:temperature"
OCC_UID = f"{SENSOR_DEV}:1:occupancy"
LQI_UID = f"{SENSOR_DEV}:0:linkquality"
LIGHT_UID = f"{LIGHT_DEV}:11:light"
POB_UID = f"{LIGHT_DEV}:11:power_on_behavior"
ON_LEVEL_UID = f"{LIGHT_DEV}:11:on_level"
IDENTIFY_UID = f"{LIGHT_DEV}:11:identify"
SWITCH_UID = f"{LIGHT_DEV}:12:switch"
ACTION_UID = f"{REMOTE_DEV}:1:action"

CATALOG_DATE = 20261121
CATALOG_SERIAL = 3
BANK = "bank-4m.zdb"
WEB = "zdefc-web.zwb"
SHARD = 0x1A
# Decoded bytes per `catalog/upload_chunk` the hub accepts.
UPLOAD_CHUNK_MAX = 4 * 1024


def sha(data: bytes) -> str:
    """SHA-256 (hex)."""
    return hashlib.sha256(data).hexdigest()


def make_release(
    bank_size: int = 30_000, version: int = 7, generation: int = 5
) -> dict[str, bytes]:
    """Release files; the fake's `manifest.cose` is JSON instead of COSE."""
    bank = bytes(i % 251 for i in range(bank_size))
    web = b"web tools " * 100
    shard = b"shard 1a records"
    manifest = {
        "version": version,
        "catalog_date": CATALOG_DATE,
        "catalog_serial": CATALOG_SERIAL,
        "bank": {"name": BANK, "size": len(bank), "sha256": sha(bank), "generation": generation},
        "web": {"name": WEB, "size": len(web), "sha256": sha(web)},
        "shards": {f"{SHARD:02x}": sha(shard)},
    }
    return {
        "manifest.cose": json.dumps(manifest).encode(),
        BANK: bank,
        WEB: web,
        f"shard-{SHARD:02x}.cbor": shard,
    }


class FakeCatalog:
    """The hub's catalog store and its API (spec 9.3, 14.4), minimal."""

    def __init__(self) -> None:
        """No manifest, no bank, no web tools."""
        self.config = {
            "base_url": "http://127.0.0.1:1/latest",
            "auto_update": False,
            "auto_fetch": False,
        }
        self.manifest: dict[str, Any] | None = None
        self.installed: dict[str, Any] | None = None
        self.web_sha: str | None = None
        self.upload: dict[str, Any] | None = None
        self.missing: list[dict[str, Any]] = []
        self.local = 0
        self.forced: list[bool] = []
        # Close the connection after storing this many more chunks, unanswered.
        self.drop_after_chunks: int | None = None

    def install(self, release: dict[str, bytes]) -> None:
        """As if `release` had been installed completely."""
        self.manifest = json.loads(release["manifest.cose"])
        bank = self.manifest["bank"]
        self.installed = self._installed(bank)
        self.web_sha = self.manifest["web"]["sha256"]

    def _installed(self, bank: dict[str, Any]) -> dict[str, Any]:
        assert self.manifest is not None
        return {
            "generation": bank["generation"],
            "catalog_date": self.manifest["catalog_date"],
            "catalog_serial": self.manifest["catalog_serial"],
            "definitions": 12,
            "kid": "47bad820d263b77e",
            "size": bank["size"],
        }

    def wants(self) -> dict[str, Any] | None:
        """What the installed manifest offers that the hub lacks."""
        if self.manifest is None:
            return None
        bank = self.manifest["bank"]
        web = self.manifest["web"]

        def asset(a: dict[str, Any]) -> dict[str, Any]:
            return {k: a[k] for k in ("name", "size", "sha256")}

        newer = self.installed is None or bank["generation"] > self.installed["generation"]
        return {
            "bank": asset(bank) if newer else None,
            "web": asset(web) if web["sha256"] != self.web_sha else None,
        }

    def status(self) -> dict[str, Any]:
        """`catalog/status`."""
        m = self.manifest
        return {
            "installed": copy.deepcopy(self.installed),
            "active_slot": "a" if self.installed else None,
            "manifest": None
            if m is None
            else {
                "version": m["version"],
                "catalog_date": m["catalog_date"],
                "catalog_serial": m["catalog_serial"],
                "source": "zhc 25.0.0",
            },
            "wants": self.wants(),
            "missing": copy.deepcopy(self.missing),
            "upload": None
            if self.upload is None
            else {"target": self.upload["target"], "written": len(self.upload["data"])},
            "web_tools": {"installed": self.web_sha is not None},
            "local": {"definitions": self.local, "used": 0, "size": 65536},
            "slot_size": 262144,
            "config": dict(self.config),
            "warnings": [],
        }

    def chunk(self, req: dict[str, Any]) -> tuple[str | None, Any]:
        """`catalog/upload_chunk`: (error, result)."""
        data = base64.b64decode(req["data"])
        if len(data) > UPLOAD_CHUNK_MAX:
            return "invalid_value", f"chunks are at most {UPLOAD_CHUNK_MAX} bytes"
        if req["offset"] == 0:
            self.upload = {"target": req["target"], "data": bytearray(), "size": req.get("size")}
        up = self.upload
        if up is None or up["target"] != req["target"]:
            return "invalid_value", f"no {req['target']} upload in progress"
        if req["offset"] != len(up["data"]):
            return "invalid_value", f"expected offset {len(up['data'])}"
        up["data"] += data
        return None, {"written": len(up["data"])}

    def commit(self, req: dict[str, Any]) -> tuple[str | None, Any, dict[str, Any] | None]:
        """`catalog/commit`: (error, result, event)."""
        up = self.upload
        target = req["target"]
        if up is None or up["target"] != target:
            return "invalid_value", f"no {target} upload in progress", None
        self.upload = None
        data = bytes(up["data"])
        if target == "manifest":
            try:
                m = json.loads(data)
            except ValueError:
                return "verification_failed", "manifest: signature", None
            if self.manifest is not None and m["version"] < self.manifest["version"]:
                return "verification_failed", "older manifest", None
            self.manifest = m
            r = {k: m[k] for k in ("version", "catalog_date", "catalog_serial")}
            r["wants"] = self.wants()
            return None, r, {"topic": "catalog_manifest", **r}
        if self.manifest is None:
            return "verification_failed", "no manifest", None
        if target == "bank":
            bank = self.manifest["bank"]
            if sha(data) != bank["sha256"]:
                return "verification_failed", "bank: SHA-256", None
            self.forced.append(bool(req.get("force")))
            self.installed = self._installed(bank)
            r = {
                "sha256": sha(data),
                "generation": bank["generation"],
                "definitions": 12,
                "devices": [],
            }
            return None, r, {"topic": "catalog_updated", **r}
        if target == "web":
            if sha(data) != self.manifest["web"]["sha256"]:
                return "verification_failed", "web: SHA-256", None
            self.web_sha = sha(data)
            r = {"size": len(data), "files": 3}
            return None, r, {"topic": "web_tools_updated", **r}
        n = req.get("shard")
        if not isinstance(n, int) or self.manifest["shards"].get(f"{n:02x}") != sha(data):
            return "verification_failed", f"shard {n}: SHA-256", None
        devices = [m["device"] for m in self.missing if n in m["shards"]]
        self.missing = [m for m in self.missing if n not in m["shards"]]
        self.local += len(devices)
        r = {"definitions": [f"def-{d}" for d in devices], "skipped": [], "devices": devices}
        return None, r, {"topic": "definition_fetched", **r}


def device(dev_id: str, manufacturer: str, model: str, **extra: Any) -> dict[str, Any]:
    """A device record (spec 4.1)."""
    rec = {
        "id": dev_id,
        "ieee": dev_id[3:],
        "src_id": None,
        "manufacturer": manufacturer,
        "model": model,
        "model_id": model,
        "sw_version": "1.2.3",
        "hw_version": 1,
        "name": model,
        "via": "hub",
        "power": "battery",
        "available": True,
        "last_seen": 1795098123.4,
        "lqi": 200,
        "rssi": -60,
        "warnings": [],
        "interview": "complete",
        "disabled": False,
        "definition": [],
    }
    rec.update(extra)
    return rec


def entity(uid: str, platform: str, **extra: Any) -> dict[str, Any]:
    """An entity record (spec 4.2)."""
    dev, ep, key = uid.rsplit(":", 2)
    rec = {
        "uid": uid,
        "device": dev,
        "endpoint": int(ep),
        "key": key,
        "platform": platform,
        "device_class": None,
        "unit": None,
        "state_class": None,
        "category": None,
        "enabled_default": True,
        "name": None,
        "translation_key": None,
        "precision": None,
        "capabilities": {},
        "source": "generic",
        "orphaned": False,
    }
    rec.update(extra)
    return rec


def state(uid: str, value: Any, **attributes: Any) -> dict[str, Any]:
    """A state message (spec 4.5)."""
    return {
        "uid": uid,
        "state": value,
        "attributes": attributes,
        "available": True,
        "last_changed": 1795098123.4,
        "last_reported": 1795098123.4,
    }


def default_model() -> tuple[dict, dict, dict]:
    """Devices, entities and states of the fake network."""
    devices = {
        SENSOR_DEV: device(SENSOR_DEV, "LUMI", "Motion sensor"),
        LIGHT_DEV: device(LIGHT_DEV, "ACME", "Dimmer", power="mains"),
        REMOTE_DEV: device(REMOTE_DEV, "IKEA", "Remote"),
    }
    entities = {
        e["uid"]: e
        for e in [
            entity(
                TEMP_UID,
                "sensor",
                device_class="temperature",
                unit="°C",
                state_class="measurement",
                precision=1,
                capabilities={"suggested_display_precision": 1},
            ),
            entity(OCC_UID, "binary_sensor", device_class="occupancy"),
            entity(
                LQI_UID,
                "sensor",
                category="diagnostic",
                enabled_default=False,
                translation_key="linkquality",
                state_class="measurement",
            ),
            entity(
                LIGHT_UID,
                "light",
                translation_key="light",
                capabilities={
                    "supported_color_modes": ["brightness"],
                    "supported_features": ["transition"],
                },
            ),
            entity(
                POB_UID,
                "select",
                category="config",
                translation_key="power_on_behavior",
                capabilities={"options": ["off", "on", "toggle", "previous"]},
            ),
            entity(
                ON_LEVEL_UID,
                "number",
                category="config",
                translation_key="on_level",
                capabilities={
                    "native_min_value": 1,
                    "native_max_value": 254,
                    "native_step": 1,
                    "mode": "slider",
                },
            ),
            entity(IDENTIFY_UID, "button", device_class="identify", category="diagnostic"),
            entity(SWITCH_UID, "switch", name="switch right", translation_key=None),
            entity(
                ACTION_UID,
                "event",
                device_class="button",
                translation_key="action",
                capabilities={"event_types": ["on", "off", "brightness_up"]},
            ),
        ]
    }
    states = {
        s["uid"]: s
        for s in [
            state(TEMP_UID, 21.4),
            state(OCC_UID, False),
            state(LQI_UID, 200),
            state(LIGHT_UID, "on", brightness=128, color_mode="brightness"),
            state(POB_UID, "previous"),
            state(ON_LEVEL_UID, 254),
            state(IDENTIFY_UID, None),
            state(SWITCH_UID, True),
            state(ACTION_UID, None),
        ]
    }
    return devices, entities, states


class FakeHub:
    """The hub side of the API, scriptable from tests."""

    def __init__(self) -> None:
        """An empty-handed fake with the default network."""
        self.token = TOKEN
        self.name = "zigbee-rs-hub 0.2.4"
        self.hub_info = True
        self.api_version = "1.1"
        self.permit_join_cap = 254
        self.network = {
            "state": "up",
            "session": "ready",
            "ownership": "owned",
            "permit_join_s": 0,
            "pan_id": 7174,
            "ext_pan_id": "744dbdfffe641ece",
            "channel": 17,
            "nwk_update_id": 2,
            "tx_power": 127,
            "ncp": {"fw": "0.1.0", "board": "esp-thread-br-zigbee-gw", "product": "zigbee-rs-ncp"},
            # The hub's clock: seconds since boot (not synchronised).
            "time": 4098.5,
        }
        # OTA server records by device (`ota/*`, `ota` topic); None: a hub
        # without the OTA server.
        self.ota: dict[str, dict[str, Any]] | None = None
        # The OTA index the hub names (served here at /index.json) and its
        # files (/files/<name>); uploads received: upload id → bytes.
        self.ota_index: list[dict[str, Any]] | None = None
        self.files: dict[str, bytes] = {}
        self.uploads: dict[int, bytearray] = {}
        self.images: list[dict[str, Any]] = []
        # A hub without an image store (4 MB): streams only.
        self.ota_store = True
        self.streamed: dict[tuple[int, int], bytes] = {}
        # Answer only the first hub/info of a connection (a hung hub).
        self.hang_after_connect = False
        # `None`: a hub without the catalog API.
        self.catalog: FakeCatalog | None = None
        # `None`: a hub without local automations (before API 1.3).
        self.automations: dict[str, dict[str, Any]] | None = None
        self.devices, self.entities, self.states = default_model()
        self.requests: list[dict[str, Any]] = []
        self.command_errors: dict[str, dict[str, str]] = {}
        self.sockets: list[tuple[web.WebSocketResponse, dict[int, set[str]]]] = []
        self.connections = 0
        self.seq = 0
        self.port = 0
        self._runner: web.AppRunner | None = None
        self._site: web.TCPSite | None = None

    async def start(self, port: int = 0) -> None:
        """Listen on 127.0.0.1."""
        app = web.Application()
        app.router.add_get("/api", self._handler)
        app.router.add_get("/index.json", self._index)
        app.router.add_get("/files/{name}", self._file)
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        self._site = web.TCPSite(self._runner, "127.0.0.1", port or self.port)
        await self._site.start()
        server = self._site._server
        assert server is not None
        self.port = server.sockets[0].getsockname()[1]

    async def _index(self, _request: web.Request) -> web.Response:
        return web.json_response(self.ota_index or [])

    async def _file(self, request: web.Request) -> web.Response:
        return web.Response(body=self.files[request.match_info["name"]])

    async def stop(self) -> None:
        """Close all connections and stop listening."""
        await self.drop()
        if self._runner is not None:
            await self._runner.cleanup()
            self._runner = None

    async def drop(self) -> None:
        """Close all connections (the hub rebooted or Wi-Fi dropped)."""
        for ws, _ in list(self.sockets):
            await ws.close()
        self.sockets.clear()

    def calls(self, req_type: str) -> list[dict[str, Any]]:
        """Received requests of one type."""
        return [r for r in self.requests if r.get("type") == req_type]

    async def push(self, topic_group: str, event: dict[str, Any]) -> None:
        """Send an event to every subscription of `topic_group`."""
        self.seq += 1
        for ws, subs in list(self.sockets):
            for sub_id, topics in subs.items():
                if topic_group in topics:
                    await ws.send_str(
                        json.dumps({"type": "event", "id": sub_id, "seq": self.seq, "event": event})
                    )

    async def push_state(self, uid: str, value: Any, **attributes: Any) -> None:
        """An `entity_state` delta."""
        msg = state(uid, value, **attributes)
        self.states[uid] = msg
        await self.push("entities", {"topic": "entity_state", **msg})

    def snapshot(self) -> dict[str, Any]:
        """The subscription snapshot."""
        return {
            "topic": "snapshot",
            "devices": copy.deepcopy(list(self.devices.values())),
            "entities": copy.deepcopy(list(self.entities.values())),
            "states": copy.deepcopy(list(self.states.values())),
        }

    async def _handler(self, request: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        self.connections += 1
        await ws.send_json(
            {"type": "auth_required", "hub": self.name, "api_version": self.api_version}
        )
        msg = await ws.receive()
        auth = json.loads(msg.data) if msg.type == WSMsgType.TEXT else {}
        if auth.get("token") != self.token:
            await ws.send_json({"type": "auth_invalid", "message": "unauthorized"})
            await ws.close()
            return ws
        await ws.send_json(
            {
                "type": "auth_ok",
                "hub": self.name,
                "api_version": self.api_version,
                "zdef_versions": [1],
                "features": ["expr-v1", "gp"],
            }
        )
        subs: dict[int, set[str]] = {}
        entry = (ws, subs)
        self.sockets.append(entry)
        try:
            async for msg in ws:
                if msg.type != WSMsgType.TEXT:
                    continue
                req = json.loads(msg.data)
                self.requests.append(req)
                await self._answer(ws, subs, req)
        finally:
            if entry in self.sockets:
                self.sockets.remove(entry)
        return ws

    async def _answer(
        self, ws: web.WebSocketResponse, subs: dict[int, set[str]], req: dict[str, Any]
    ) -> None:
        rid = req.get("id")
        kind = req.get("type")

        async def ok(result: Any = None) -> None:
            await ws.send_json({"type": "result", "id": rid, "success": True, "result": result})

        async def err(code: str, message: str) -> None:
            await ws.send_json(
                {
                    "type": "result",
                    "id": rid,
                    "success": False,
                    "error": {"code": code, "message": message},
                }
            )

        if kind == "hub/info" and self.hang_after_connect and subs:
            return
        if kind == "hub/info" and self.hub_info:
            extra = (
                {"ota": {"store": self.ota_store, "stream": True, "chunk_max": 4096}}
                if self.ota is not None
                else {}
            )
            await ok(
                extra
                | {
                    "name": self.name,
                    "api_version": self.api_version,
                    "features": ["expr-v1", "gp"],
                    "seq": self.seq,
                    "devices": len(self.devices),
                    "entities": len(self.entities),
                    "coordinator_ieee": COORDINATOR_IEEE,
                    "ncp_board": "esp32h2",
                    "ncp_product": "zigbee-rs NCP",
                    **(
                        {"automations": {"format": 1, "max_rules": 64}}
                        if self.automations is not None
                        else {}
                    ),
                }
            )
        elif kind == "subscribe":
            topics = set(req.get("topics", []))
            known = {"devices", "entities", "events", "gp"}
            if self.catalog is not None:
                known.add("catalog")
            if self.api_version >= "1.1":
                known.add("network")
            if self.automations is not None:
                known.add("automations")
            if self.ota is not None:
                known.add("ota")
            if topics - known:
                await err("invalid_value", f"unknown topics {sorted(topics - known)}")
                return
            subs[rid] = topics
            await ok({"seq": self.seq})
            snap = self.snapshot() if topics != {"catalog"} else {"topic": "snapshot"}
            if "network" in topics:
                snap["network"] = copy.deepcopy(self.network)
            await ws.send_json({"type": "event", "id": rid, "seq": self.seq, "event": snap})
        elif kind == "entities/command":
            if error := self.command_errors.get(req["uid"]):
                await err(error["code"], error["message"])
            else:
                await ok({"status": "ok"})
        elif kind == "network/permit_join":
            if not req.get("target"):
                # The hub caps the window; no `permit_join` event (as the
                # real NCP today).
                self.network["permit_join_s"] = min(req["duration_s"], self.permit_join_cap)
            await ok(None)
        elif kind == "network/info" and self.api_version >= "1.1":
            await ok(copy.deepcopy(self.network))
        elif kind == "devices/remove":
            dev = req.get("device")
            if dev not in self.devices:
                await err("not_found", f"no device {dev}")
                return
            self.devices.pop(dev)
            for uid in [u for u, e in self.entities.items() if e["device"] == dev]:
                self.entities.pop(uid)
                self.states.pop(uid, None)
            await ok(None)
            await self.push("devices", {"topic": "device_removed", "id": dev})
        elif kind == "devices/diagnostics":
            await ok({"frames": [], "gp_data": {"key": "00112233445566778899aabbccddeeff"}})
        elif kind in ("devices/reinterview", "devices/reconfigure"):
            await ok({"started": True})
        elif kind.startswith("automations/") and self.automations is not None:
            await self._answer_automations(req, ok, err)
        elif kind.startswith(("catalog/", "definitions/")) and self.catalog is not None:
            await self._answer_catalog(ws, req, ok, err)
        elif kind == "ota/devices/list" and self.ota is not None:
            await ok(copy.deepcopy(list(self.ota.values())))
        elif kind == "ota/start" and self.ota is not None:
            await ok({"sleepy": False})
            if req.get("image", 0) >= 0x1_0000:
                await ws.send_json(
                    {"type": "ota/block_needed", "session": req["image"], "offset": 0, "len": 4096}
                )
        elif kind == "ota/stream/offer" and self.ota is not None:
            head = base64.b64decode(req["header"])
            await ok(
                {
                    "session": 0x1_0000,
                    "manufacturer_code": int.from_bytes(head[10:12], "little"),
                    "image_type": int.from_bytes(head[12:14], "little"),
                    "file_version": int.from_bytes(head[14:18], "little"),
                    "devices": [],
                }
            )
        elif kind == "ota/block_data" and self.ota is not None:
            self.streamed[(req["session"], req["offset"])] = base64.b64decode(req["data"])
            await ok(None)
        elif kind == "ota/stream/close" and self.ota is not None:
            await ok(None)
        elif kind == "ota/status" and self.ota is not None:
            url = f"http://127.0.0.1:{self.port}/index.json" if self.ota_index is not None else ""
            await ok({"settings": {"policy": "manual", "index_url": url}, "chunk_max": 4096})
        elif kind == "ota/upload/begin" and self.ota is not None:
            upload = len(self.uploads) + 1
            self.uploads[upload] = bytearray()
            await ok({"upload": upload, "offset": 0, "chunk_max": 4096})
        elif kind == "ota/upload/chunk" and self.ota is not None:
            buf = self.uploads[req["upload"]]
            assert req["offset"] == len(buf)
            buf.extend(base64.b64decode(req["data"]))
            await ok({"written": len(buf)})
        elif kind == "ota/upload/commit" and self.ota is not None:
            image = {"image": 40 + req["upload"], "size": len(self.uploads[req["upload"]])}
            self.images.append(image)
            await ok(image)
        else:
            await err("invalid_format", f"unknown variant `{kind}`")

    async def _answer_automations(self, req: dict[str, Any], ok, err) -> None:
        rules = self.automations
        assert rules is not None
        kind = req["type"]
        if kind == "automations/list":
            await ok({"time_synced": True, "rules": copy.deepcopy(list(rules.values()))})
            return
        rule = rules.get(req.get("automation", ""))
        if rule is None:
            await err("not_found", f"no automation {req.get('automation')}")
        elif kind in ("automations/enable", "automations/disable"):
            rule["enabled"] = kind == "automations/enable"
            await ok(copy.deepcopy(rule))
            await self.push("automations", {"topic": "automation_updated", "automation": rule})
        elif kind == "automations/run":
            rule["runs"] = rule.get("runs", 0) + 1
            await ok({"run": rule["runs"], "trace": {"result": "done"}})
            await self.push(
                "automations",
                {
                    "topic": "automation_run",
                    "automation": rule["id"],
                    "run": rule["runs"],
                    "result": "done",
                    "time": 4100.0,
                },
            )
        else:
            await err("invalid_format", f"unknown variant `{kind}`")

    async def _answer_catalog(
        self, ws: web.WebSocketResponse, req: dict[str, Any], ok, err
    ) -> None:
        cat = self.catalog
        assert cat is not None
        kind = req["type"]
        if kind == "catalog/status":
            await ok(cat.status())
        elif kind == "catalog/config":
            cat.config.update(
                {k: req[k] for k in ("base_url", "auto_update", "auto_fetch") if k in req}
            )
            await ok(dict(cat.config))
        elif kind == "catalog/upload_chunk":
            code, result = cat.chunk(req)
            if code is not None:
                await err(code, result)
                return
            if cat.drop_after_chunks is not None:
                cat.drop_after_chunks -= 1
                if cat.drop_after_chunks <= 0:
                    # Stored, but the answer never arrives.
                    cat.drop_after_chunks = None
                    await ws.close()
                    return
            await ok(result)
        elif kind == "catalog/commit":
            code, result, event = cat.commit(req)
            if code is not None:
                await err(code, result)
                return
            await ok(result)
            if event is not None:
                await self.push("catalog", event)
        elif kind == "catalog/abort":
            cat.upload = None
            await ok(None)
        elif kind == "definitions/fetch":
            shards = next(
                (m["shards"] for m in cat.missing if m["device"] == req.get("device")), []
            )
            await ok({"device": req.get("device"), "shards": shards})
        else:
            await err("invalid_format", f"unknown variant `{kind}`")


class ReleaseServer:
    """The catalog mirror: release files over HTTP on 127.0.0.1."""

    def __init__(self, files: dict[str, bytes]) -> None:
        """Serves `files` under `/latest/`."""
        self.files = files
        self.requests: list[str] = []
        self.port = 0
        self._runner: web.AppRunner | None = None

    @property
    def base_url(self) -> str:
        """The mirror's base URL."""
        return f"http://127.0.0.1:{self.port}/latest"

    async def start(self) -> None:
        """Listen."""
        app = web.Application()
        app.router.add_get("/latest/{name}", self._handler)
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "127.0.0.1", 0)
        await site.start()
        server = site._server
        assert server is not None
        self.port = server.sockets[0].getsockname()[1]

    async def stop(self) -> None:
        """Stop listening."""
        if self._runner is not None:
            await self._runner.cleanup()
            self._runner = None

    async def _handler(self, request: web.Request) -> web.Response:
        name = request.match_info["name"]
        self.requests.append(name)
        if name not in self.files:
            raise web.HTTPNotFound
        return web.Response(body=self.files[name], content_type="application/octet-stream")
