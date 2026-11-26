"""Migration from ZHA to the hub (docs/migration.md).

ZHA's zigbee.db (zigpy's application database, schemas v12 to v17) and its
newest network backup become the hub's import document
(`zigbee-rs-hub/import` v1); names and areas come from Home Assistant's
registries. The document goes to the hub in requests of a few kilobytes
(`network/import/preview`, `network/import`).
"""

from __future__ import annotations

from collections.abc import Callable
import json
import re
import sqlite3
from typing import Any

FORMAT = "zigbee-rs-hub/import"
OCB = "zigpy/open-coordinator-backup"
# Bytes of the arrays of one request at most (the hub takes 16 KiB).
BATCH = 6 * 1024

# Basic attributes → document fields.
BASIC = {
    0x0000: "zcl_version",
    0x0001: "app_version",
    0x0002: "stack_version",
    0x0003: "hw_version",
    0x0004: "manufacturer",
    0x0005: "model",
    0x0006: "date_code",
    0x0007: "power_source",
    0x4000: "sw_build_id",
}


def kept(cluster: int, attr: int) -> bool:
    """Cached values the hub keeps (hub_devices::persist::persistent_value)."""
    match cluster:
        case 0x0001:
            return 0x0031 <= attr <= 0x0036
        case 0x0201:
            return attr in (0x0015, 0x0016, 0x001B)
        case 0x0202:
            return attr == 0x0001
        case 0x0300:
            return 0x400A <= attr <= 0x400C
        case 0x0702:
            return 0x0300 <= attr <= 0x0306
        case 0x0B04:
            return 0x0600 <= attr <= 0x0605
    return False


def ieee_hex(s: Any) -> str | None:
    """Big-endian hex of an IEEE written `aa:bb:…`, `0x…` or plain."""
    if not isinstance(s, str):
        return None
    d = re.sub(r"[:-]", "", s.strip().removeprefix("0x").removeprefix("0X")).lower()
    return d if re.fullmatch(r"[0-9a-f]{16}", d) else None


def _plain(s: str) -> str:
    return s.replace(":", "").lower()


def _nwk(s: str) -> str:
    return s.lower().removeprefix("0x").rjust(4, "0")


def zigpy_to_ocb(b: dict[str, Any]) -> dict[str, Any]:
    """An Open Coordinator Backup of zigpy's own backup (`NetworkBackup.as_dict()`).

    As zigpy's `_network_backup_to_open_coordinator_backup` writes it; an
    OCB passes through.
    """
    if "metadata" in b:
        return b
    ni, node = b["network_info"], b["node_info"]
    coord = _plain(node["ieee"])
    devices: dict[str, dict[str, Any]] = {}

    def dev(ieee: str) -> dict[str, Any]:
        return devices.setdefault(
            ieee, {"ieee_address": ieee, "nwk_address": None, "is_child": False}
        )

    for ieee, nwk in ni.get("nwk_addresses", {}).items():
        dev(_plain(ieee))["nwk_address"] = _nwk(nwk)
    for ieee in ni.get("children", []):
        dev(_plain(ieee))["is_child"] = True
    seqs = {}
    for k in ni.get("key_table", []):
        ieee = _plain(k["partner_ieee"])
        seqs[ieee] = k.get("seq", 0)
        dev(ieee)["link_key"] = {
            "key": _plain(k["key"]),
            "tx_counter": k.get("tx_counter", 0),
            "rx_counter": k.get("rx_counter", 0),
        }
    tclk = ni.get("tc_link_key") or {}
    internal: dict[str, Any] = {
        "creation_time": b.get("backup_time"),
        "node": {
            "ieee": coord,
            "nwk": _nwk(node.get("nwk", "0000")),
            "type": node.get("logical_type", "coordinator"),
            "model": node.get("model"),
            "manufacturer": node.get("manufacturer"),
            "version": node.get("version"),
        },
        "link_key_seqs": seqs,
        "tx_power": ni.get("tx_power"),
    }
    if tclk:
        internal["network"] = {
            "tc_link_key": {"key": _plain(tclk["key"]), "frame_counter": tclk.get("tx_counter", 0)},
            "tc_address": _plain(tclk.get("partner_ieee") or node["ieee"]),
            "nwk_manager": _nwk(ni.get("nwk_manager_id", "0000")),
        }
    internal.update(ni.get("metadata") or {})
    nk = ni["network_key"]
    return {
        "metadata": {"version": 1, "format": OCB, "source": ni.get("source"), "internal": internal},
        "stack_specific": ni.get("stack_specific") or {},
        "coordinator_ieee": coord,
        "pan_id": _nwk(ni["pan_id"]),
        "extended_pan_id": _plain(ni["extended_pan_id"]),
        "nwk_update_id": ni.get("nwk_update_id", 0),
        "security_level": ni.get("security_level", 5),
        "channel": ni["channel"],
        "channel_mask": ni.get("channel_mask", []),
        "network_key": {
            "key": _plain(nk["key"]),
            "sequence_number": nk.get("seq", 0),
            "frame_counter": nk.get("tx_counter", 0),
        },
        "devices": [devices[k] for k in sorted(devices)],
    }


def _typed(ty: int, b: bytes) -> Any:
    """A v17 typed value (`data_type`, serialized bytes)."""
    if ty == 0x10 and b:
        return bool(b[0])
    sizes = {0x08: 1, 0x18: 1, 0x20: 1, 0x30: 1, 0x09: 2, 0x19: 2, 0x21: 2, 0x31: 2}
    if ty in sizes or 0x08 <= ty <= 0x27:
        n = sizes.get(ty, (ty & 0x07) + 1)
        return int.from_bytes(b[:n], "little") if len(b) >= n else None
    if 0x28 <= ty <= 0x2F:
        n = (ty & 0x07) + 1
        return int.from_bytes(b[:n], "little", signed=True) if len(b) >= n else None
    if ty in (0x41, 0x42) and b:
        return b[1 : 1 + b[0]].decode("utf-8", "replace").rstrip("\0")
    return None


def _value(v: Any, ty: int | None) -> Any:
    if isinstance(v, bytes):
        if ty is not None:
            return _typed(ty, v)
        try:
            return v.decode().rstrip("\0")
        except UnicodeDecodeError:
            return None
    return v


def read_zha_db(path: str) -> dict[str, Any]:
    """Devices, groups and the newest network backup of a zigbee.db.

    Returns `{"version": N, "devices": {ieee: entry}, "groups": [...], "backup": dict | None}`
    (entries in the document's form, without names).
    """
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        tables = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        versions = [int(m.group(1)) for t in tables if (m := re.fullmatch(r"endpoints_v(\d+)", t))]
        v = max(versions, default=0)
        if v < 12:
            raise ValueError("not a zigpy database (no endpoints_v12 or newer)")
        dev_t = f"zigbee_devices_v{v}" if f"zigbee_devices_v{v}" in tables else f"devices_v{v}"
        out: dict[str, dict[str, Any]] = {}
        # zigpy's Status: 2 (ENDPOINTS_INIT) is a finished interview, 0 and 1 are not.
        dev_cols = {r[1] for r in db.execute(f"PRAGMA table_info({dev_t})")}
        status = "status" if "status" in dev_cols else "NULL"
        for ieee, nwk, st in db.execute(f"SELECT ieee, nwk, {status} FROM {dev_t}"):
            if i := ieee_hex(ieee):
                out[i] = {"ieee": i, "nwk": nwk, "endpoints": [], "source_id": ieee}
                if st is not None:
                    out[i]["interviewed"] = st == 2
        for ieee, ep, profile, dt in db.execute(
            f"SELECT ieee, endpoint_id, profile_id, device_type FROM endpoints_v{v} ORDER BY ieee, endpoint_id"
        ):
            if (d := out.get(ieee_hex(ieee) or "")) and ep > 0:
                d["endpoints"].append(
                    {"id": ep, "profile": profile, "device_id": dt, "in": [], "out": []}
                )
        if f"clusters_v{v}" in tables:
            rows = db.execute(
                f"SELECT ieee, endpoint_id, cluster_type, cluster_id FROM clusters_v{v} ORDER BY rowid"
            ).fetchall()
        else:
            rows = (
                db.execute(f"SELECT ieee, endpoint_id, 0, cluster FROM in_clusters_v{v}").fetchall()
                + db.execute(
                    f"SELECT ieee, endpoint_id, 1, cluster FROM out_clusters_v{v}"
                ).fetchall()
            )
        for ieee, ep, ty, c in rows:
            d = out.get(ieee_hex(ieee) or "")
            for e in d["endpoints"] if d else []:
                if e["id"] == ep:
                    e["in" if ty == 0 else "out"].append(c)
        if f"node_descriptors_v{v}" in tables:
            for ieee, lt, mac, mfr, buf, inc, mask, outg in db.execute(
                "SELECT ieee, logical_type, mac_capability_flags, manufacturer_code, maximum_buffer_size,"
                " maximum_incoming_transfer_size, server_mask, maximum_outgoing_transfer_size"
                f" FROM node_descriptors_v{v}"
            ):
                if d := out.get(ieee_hex(ieee) or ""):
                    if 0 <= lt <= 2:
                        d["type"] = ("coordinator", "router", "end_device")[lt]
                    d["manufacturer_code"] = mfr
                    d["node_descriptor"] = {
                        "logical_type": lt,
                        "mac_capabilities": mac,
                        "manufacturer_code": mfr,
                        "max_buffer_size": buf,
                        "max_incoming_transfer": inc,
                        "server_mask": mask,
                        "max_outgoing_transfer": outg,
                    }
        at = f"attributes_cache_v{v}"
        cols = {r[1] for r in db.execute(f"PRAGMA table_info({at})")}
        cl, attr = ("cluster_id", "attr_id") if "cluster_id" in cols else ("cluster", "attrid")
        where = "cluster_type = 0" if "cluster_type" in cols else "1"
        if "status" in cols:
            where += " AND (status IS NULL OR status = 0)"
        if "manufacturer_code" in cols:
            where += " AND manufacturer_code IS NULL"
        dtype = "data_type" if "data_type" in cols else "NULL"
        for ieee, ep, c, a, val, ty in db.execute(
            f"SELECT ieee, endpoint_id, {cl}, {attr}, value, {dtype} FROM {at} WHERE {where}"
        ):
            d = out.get(ieee_hex(ieee) or "")
            if d is None or (val := _value(val, ty)) is None:
                continue
            if c == 0 and a in BASIC:
                d.setdefault(BASIC[a], val)
            elif kept(c, a) and ep is not None:
                d.setdefault("attributes", []).append(
                    {"ep": ep, "cluster": c, "attr": a, "value": val}
                )
        if f"gp_devices_v{v}" in tables:
            for (ieee,) in db.execute(f"SELECT ieee FROM gp_devices_v{v}"):
                if i := ieee_hex(ieee):
                    out[i] = {"ieee": i, "type": "green_power"}
        groups: dict[int, dict[str, Any]] = {}
        if f"groups_v{v}" in tables:
            for gid, name in db.execute(f"SELECT group_id, name FROM groups_v{v}"):
                groups[gid] = {"id": gid, "name": name, "members": []}
            for gid, ieee, ep in db.execute(
                f"SELECT group_id, ieee, endpoint_id FROM group_members_v{v}"
            ):
                if gid in groups and (i := ieee_hex(ieee)):
                    groups[gid]["members"].append({"ieee": i, "ep": ep})
        backup = None
        if f"network_backups_v{v}" in tables:
            row = db.execute(
                f"SELECT backup_json FROM network_backups_v{v} ORDER BY id DESC LIMIT 1"
            ).fetchone()
            if row:
                backup = json.loads(row[0])
        return {"version": v, "devices": out, "groups": list(groups.values()), "backup": backup}
    finally:
        db.close()


def document(
    zha: dict[str, Any],
    backup: dict[str, Any] | None,
    names: dict[str, tuple[str | None, str | None]],
) -> dict[str, Any]:
    """The import document of `read_zha_db`'s result.

    `backup` replaces the database's newest one; `names`: name and area by IEEE.
    """
    b = backup if backup is not None else zha["backup"]
    network = zigpy_to_ocb(b) if b else None
    coord = network["coordinator_ieee"] if network else None
    devices = []
    for ieee, entry in zha["devices"].items():
        d = dict(entry)
        if ieee == coord or d.get("nwk") == 0:
            d["type"] = "coordinator"
        name, area = names.get(ieee, (None, None))
        if name:
            d["name"] = name
        if area:
            d["area"] = area
        devices.append(d)
    return {
        "format": FORMAT,
        "version": 1,
        "source": {"kind": "zha", "name": f"ZHA (zigpy database v{zha['version']})"},
        "network": network,
        "devices": devices,
        "groups": zha["groups"],
    }


def requests(doc: dict[str, Any], options: dict[str, Any]) -> list[dict[str, Any]]:
    """The document in requests: `document` first, then batches; `more` on all but the last.

    Every list goes in batches: a request over 16 KiB is refused.
    """
    head = dict(doc)
    net_devices: list[Any] = []
    if isinstance(head.get("network"), dict):
        head["network"] = dict(head["network"])
        net_devices = head["network"].pop("devices", None) or []
        head["network"]["devices"] = []
    devices = head.pop("devices", None) or []
    groups = head.pop("groups", None) or []
    out: list[dict[str, Any]] = [{"document": head, **options}]
    for key, items in (
        ("network_devices", net_devices),
        ("devices", devices),
        ("groups", groups),
    ):
        batch: list[Any] = []
        size = 0
        for e in items:
            n = len(json.dumps(e)) + 1
            if batch and size + n > BATCH:
                out.append({key: batch})
                batch, size = [], 0
            batch.append(e)
            size += n
        if batch:
            out.append({key: batch})
    for i, r in enumerate(out):
        r["more"] = i < len(out) - 1
    return out


async def send(
    request: Callable[..., Any], req_type: str, doc: dict[str, Any], options: dict[str, Any]
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """Send `doc`; the last answer (with the first one's network) and every device row."""
    rows: list[dict[str, Any]] = []
    first: dict[str, Any] | None = None
    last: dict[str, Any] = {}
    for r in requests(doc, options):
        last = await request(req_type, **r)
        if isinstance(last.get("devices"), list):
            rows.extend(last["devices"])
        if first is None:
            first = last.get("network")
    if "network" not in last and first is not None:
        last["network"] = first
    return last, rows
