"""Constants of the zigbee-rs hub integration."""

from __future__ import annotations

from datetime import timedelta
from typing import Final

from homeassistant.const import Platform

DOMAIN: Final = "zigbee_rs_hub"

DEFAULT_PORT: Final = 80
DEFAULT_PATH: Final = "/api"
CONF_PATH: Final = "path"

# Subscription topics (device-support spec 14.1).
TOPICS: Final = ["devices", "entities", "events"]

REQUEST_TIMEOUT: Final = 10.0
# Commands wait for the device's confirmation (spec 14.3).
COMMAND_TIMEOUT: Final = 20.0
# Liveness request interval on an idle connection.
KEEPALIVE: Final = 60.0
RECONNECT_MIN: Final = 1.0
RECONNECT_MAX: Final = 60.0

PERMIT_JOIN_DEFAULT: Final = 254

# Hub platform names (spec 4.3, and `scene` of group scenes, 14.9) → HA
# platforms; `update` is not served yet.
PLATFORMS: Final = [
    Platform.BINARY_SENSOR,
    Platform.BUTTON,
    Platform.CLIMATE,
    Platform.COVER,
    Platform.EVENT,
    Platform.FAN,
    Platform.LIGHT,
    Platform.LOCK,
    Platform.NUMBER,
    Platform.SCENE,
    Platform.SELECT,
    Platform.SENSOR,
    Platform.SIREN,
    Platform.SWITCH,
    Platform.TEXT,
    Platform.VALVE,
]

SERVICE_PERMIT_JOIN: Final = "permit_join"
SERVICE_REINTERVIEW: Final = "reinterview_device"
SERVICE_RECONFIGURE: Final = "reconfigure_device"
SERVICE_UPDATE_CATALOG: Final = "update_catalog"

ATTR_DURATION: Final = "duration"
ATTR_INCLUDE_GP: Final = "include_gp"
ATTR_CONFIG_ENTRY_ID: Final = "config_entry_id"
ATTR_FORCE: Final = "force"
ATTR_BASE_URL: Final = "base_url"

# Definitions catalog (device-support 9.3/9.4): the hub downloads nothing;
# the integration fetches the signed release files and uploads them.
CATALOG_DEFAULT_BASE: Final = "https://faronov.github.io/zigbee-rs-definitions/latest"
# Decoded bytes per `catalog/upload_chunk`: the most the hub accepts (without
# PSRAM it holds the frame, the parsed request and the decoded bytes at once).
CATALOG_CHUNK: Final = 4 * 1024
# Largest release file the integration downloads.
CATALOG_MAX_FILE: Final = 4 * 1024 * 1024
CATALOG_DOWNLOAD_TIMEOUT: Final = 120.0
CATALOG_CHUNK_TIMEOUT: Final = 30.0
# A commit verifies (and for a bank decodes) the whole file in flash.
CATALOG_COMMIT_TIMEOUT: Final = 180.0
# How long an interrupted upload waits for the hub to come back.
CATALOG_RECONNECT_WAIT: Final = 120.0
CATALOG_RESUMES: Final = 3
# First check after set-up, then daily (`auto_update`).
CATALOG_FIRST_CHECK: Final = 120.0
CATALOG_INTERVAL: Final = timedelta(days=1)
# `definition_needed` (`auto_fetch`): one attempt per device per hour,
# events of a burst handled together.
CATALOG_FETCH_RETRY: Final = 3600.0
CATALOG_FETCH_DEBOUNCE: Final = 2.0

# Entity keys that are the device's main feature: the entity takes the
# device name (HA's `has_entity_name` with `name = None`).
PRIMARY_KEYS: Final = frozenset(
    {"light", "switch", "cover", "lock", "thermostat", "fan", "siren", "valve"}
)

# (platform, key) pairs with a translation in strings.json
# (`entity.<platform>.<key>`); other keys without a name are humanised.
TRANSLATED_KEYS: Final = frozenset(
    {
        ("button", "identify"),
        ("event", "action"),
        ("event", "lock_event"),
        ("number", "local_temperature_calibration"),
        ("number", "occupancy_timeout"),
        ("number", "on_level"),
        ("number", "transition"),
        ("select", "keypad_lockout"),
        ("select", "power_on_behavior"),
        ("select", "temperature_display_mode"),
        ("sensor", "last_seen"),
        ("sensor", "linkquality"),
        ("sensor", "pi_heating_demand"),
        ("sensor", "rssi"),
    }
)

TO_REDACT: Final = {
    "token",
    "key",
    "keys",
    "network_key",
    "link_key",
    "install_code",
    "security_key",
    "gpd_key",
}
