"""Import from ZHA, and names and areas from ZHA or Zigbee2MQTT (docs/migration.md).

`import_from_zha` reads ZHA's zigbee.db and its newest network backup (or a
backup file), with the names and areas of ZHA's devices in Home Assistant,
and previews the import on the hub or applies it. Applying needs ZHA
stopped (its config entry disabled): two coordinators must never run the
same network. Afterwards the areas of ZHA's devices go to the hub's new
devices. `copy_names_areas` does the same for devices imported otherwise
(the hub's web UI, hubctl), from ZHA's or Zigbee2MQTT's (MQTT) devices.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
from typing import Any

from homeassistant.components import persistent_notification
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from homeassistant.helpers import area_registry as ar, device_registry as dr

from . import migrate
from .const import DOMAIN
from .hub import find_device

_LOGGER = logging.getLogger(__name__)

# An apply restores the network: NCP resets, up to a minute or two.
APPLY_TIMEOUT = 300.0
PREVIEW_TIMEOUT = 60.0
# How long the hub's new devices are waited for to take their areas.
AREA_WAIT = 20.0
NOTIFICATION = "zigbee_rs_hub_migration"


def source_names(
    hass: HomeAssistant, sources: tuple[str, ...] = ("zha", "mqtt")
) -> dict[str, tuple[str | None, str | None]]:
    """Names the user gave and areas of ZHA's (and Zigbee2MQTT's) devices, by IEEE."""
    areas = ar.async_get(hass)
    reg = dr.async_get(hass)
    out: dict[str, tuple[str | None, str | None]] = {}
    entries = [e for s in sources for e in hass.config_entries.async_entries(s)]
    for d in (d for e in entries for d in dr.async_entries_for_config_entry(reg, e.entry_id)):
        ieee = None
        for domain, ident in d.identifiers:
            if domain == "zha" and "zha" in sources:
                ieee = migrate.ieee_hex(ident)
            elif domain == "mqtt" and "mqtt" in sources and ident.startswith("zigbee2mqtt_"):
                ieee = migrate.ieee_hex(ident.removeprefix("zigbee2mqtt_"))
        if ieee is None:
            continue
        area = areas.async_get_area(d.area_id) if d.area_id else None
        out[ieee] = (d.name_by_user, area.name if area else None)
    return out


def _zha_running(hass: HomeAssistant) -> bool:
    return any(e.state is ConfigEntryState.LOADED for e in hass.config_entries.async_entries("zha"))


def _read_file(path: str) -> dict[str, Any]:
    with open(path, encoding="utf-8") as f:
        return json.load(f)


async def async_import_from_zha(
    hass: HomeAssistant, hub: Any, data: dict[str, Any]
) -> dict[str, Any]:
    """The `import_from_zha` service: preview, or apply when `apply`."""
    path = data.get("database") or hass.config.path("zigbee.db")
    if not await hass.async_add_executor_job(os.path.exists, path):
        raise ServiceValidationError(f"no ZHA database at {path}")
    apply = data.get("apply", False)
    if apply and _zha_running(hass):
        raise ServiceValidationError(
            "ZHA is running: disable its integration first (Settings → Devices & services → "
            "Zigbee Home Automation → ⋮ → Disable). Two coordinators must never run one network."
        )
    try:
        zha = await hass.async_add_executor_job(migrate.read_zha_db, path)
        backup = None
        if data.get("backup"):
            backup = await hass.async_add_executor_job(_read_file, data["backup"])
    except (OSError, ValueError, KeyError) as err:
        raise HomeAssistantError(f"cannot read {path}: {err}") from err
    names = source_names(hass, ("zha",))
    doc = migrate.document(zha, backup, names)
    if doc["network"] is None:
        raise ServiceValidationError(
            "the ZHA database has no network backup: create one in ZHA (Configure → Download backup) "
            "and pass it as backup"
        )
    options: dict[str, Any] = {}
    for key in ("force", "confirm", "confirm_ieee_write_once", "skip_backup", "remove_missing"):
        if data.get(key):
            options[key] = True
    if data.get("keep_ieee"):
        options["allow_ieee_change"] = False
    if data.get("reconfigure"):
        options["reconfigure"] = data["reconfigure"]
    timeout = APPLY_TIMEOUT if apply else PREVIEW_TIMEOUT

    async def request(req_type: str, **params: Any) -> Any:
        return await hub.async_request(req_type, timeout=timeout, **params)

    req_type = "network/import" if apply else "network/import/preview"
    result, rows = await migrate.send(request, req_type, doc, options)
    result.pop("backup", None)
    answer: dict[str, Any] = {"result": result, "rows": rows}
    if apply and result.get("stored") is False:
        raise HomeAssistantError(
            "the hub imported the devices but could not store them (its device registry is "
            "not writable): they are lost at its next restart; check network/info's registry "
            "and import again"
        )
    if apply:
        areas = {i: a for i, (_, a) in names.items() if a}
        answer["areas"] = await _apply_areas(hass, hub, areas, overwrite=False, wait=AREA_WAIT)
        d = result.get("devices", {})
        persistent_notification.async_create(
            hass,
            f"Imported from ZHA: {d.get('new', 0)} new, {d.get('replace', 0)} replaced, "
            f"{d.get('keep', 0)} kept, {d.get('skip', 0)} skipped; {result.get('groups', 0)} groups. "
            "Devices are verified when they next talk (sleepy ones when they wake). "
            "Keep ZHA disabled; once the devices work here, delete the ZHA integration "
            "(its devices then leave Home Assistant).",
            title="zigbee-rs hub: migration from ZHA",
            notification_id=NOTIFICATION,
        )
    else:
        net = result.get("network", {})
        s = result.get("summary", {})
        persistent_notification.async_create(
            hass,
            f"Preview of the import from ZHA: network {net.get('ext_pan_id')} "
            f"({net.get('action')}, needs {', '.join(net.get('needs', [])) or 'nothing'}), "
            f"{s.get('new', 0)} new, {s.get('replace', 0)} replaced, {s.get('keep', 0)} kept, "
            f"{s.get('skip', 0)} skipped; restore check: {result.get('restore_check')}. "
            "Disable ZHA, then call the service again with apply.",
            title="zigbee-rs hub: migration from ZHA",
            notification_id=NOTIFICATION,
        )
    return answer


async def _apply_areas(
    hass: HomeAssistant, hub: Any, areas: dict[str, str], overwrite: bool, wait: float
) -> int:
    """Areas (by name) of the hub's devices with these IEEEs; waits `wait` s for them."""
    reg = dr.async_get(hass)
    area_reg = ar.async_get(hass)
    todo = dict(areas)
    done = 0
    loop = asyncio.get_running_loop()
    until = loop.time() + wait
    while todo:
        for ieee in list(todo):
            dev = find_device(reg, (DOMAIN, f"zb:{ieee}"), hub.entry.entry_id)
            if dev is None:
                continue
            name = todo.pop(ieee)
            if dev.area_id and not overwrite:
                continue
            area = area_reg.async_get_area_by_name(name) or area_reg.async_create(name)
            reg.async_update_device(dev.id, area_id=area.id)
            done += 1
        if not todo or loop.time() >= until:
            break
        await asyncio.sleep(0.5)
    return done


async def async_copy_names_areas(hass: HomeAssistant, hub: Any, overwrite: bool) -> dict[str, int]:
    """The `copy_names_areas` service: from ZHA's and Zigbee2MQTT's devices to the hub's."""
    reg = dr.async_get(hass)
    names = source_names(hass)
    named = 0
    for ieee, (name, _) in names.items():
        dev = find_device(reg, (DOMAIN, f"zb:{ieee}"), hub.entry.entry_id)
        if dev is None or not name or (dev.name_by_user and not overwrite):
            continue
        reg.async_update_device(dev.id, name_by_user=name)
        named += 1
    areas = {i: a for i, (_, a) in names.items() if a}
    return {"names": named, "areas": await _apply_areas(hass, hub, areas, overwrite, wait=0)}
