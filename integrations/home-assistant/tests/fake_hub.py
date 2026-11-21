"""A fake zigbee-rs hub: the WebSocket JSON API on a local aiohttp server."""

from __future__ import annotations

import copy
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
        # Answer only the first hub/info of a connection (a hung hub).
        self.hang_after_connect = False
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
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        self._site = web.TCPSite(self._runner, "127.0.0.1", port or self.port)
        await self._site.start()
        server = self._site._server
        assert server is not None
        self.port = server.sockets[0].getsockname()[1]

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
        await ws.send_json({"type": "auth_required", "hub": self.name, "api_version": "1.0"})
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
                "api_version": "1.0",
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
            await ok(
                {
                    "name": self.name,
                    "api_version": "1.0",
                    "features": ["expr-v1", "gp"],
                    "seq": self.seq,
                    "devices": len(self.devices),
                    "entities": len(self.entities),
                    "coordinator_ieee": COORDINATOR_IEEE,
                    "ncp_board": "esp32h2",
                    "ncp_product": "zigbee-rs NCP",
                }
            )
        elif kind == "subscribe":
            subs[rid] = set(req.get("topics", []))
            await ok({"seq": self.seq})
            await ws.send_json(
                {"type": "event", "id": rid, "seq": self.seq, "event": self.snapshot()}
            )
        elif kind == "entities/command":
            if error := self.command_errors.get(req["uid"]):
                await err(error["code"], error["message"])
            else:
                await ok({"status": "ok"})
        elif kind == "network/permit_join":
            await ok(None)
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
        else:
            await err("invalid_format", f"unknown variant `{kind}`")
