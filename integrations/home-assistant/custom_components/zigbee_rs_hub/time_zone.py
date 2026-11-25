"""The POSIX TZ string of an IANA time zone, for `hub/time/set`."""

from __future__ import annotations

from importlib import resources
from pathlib import Path
import re
import zoneinfo

# What the hub accepts as `tz_name` (device-support 14.6).
_NAME = re.compile(r"[A-Za-z][A-Za-z0-9/_+-]{0,63}")


def _tzif(name: str) -> bytes | None:
    """The zone's TZif file, where `zoneinfo` looks for it: TZPATH, then `tzdata`."""
    parts = name.split("/")
    if not _NAME.fullmatch(name) or ".." in parts:
        return None
    for root in zoneinfo.TZPATH:
        path = Path(root, *parts)
        try:
            if path.is_file():
                return path.read_bytes()
        except OSError:
            continue
    try:
        return resources.files("tzdata").joinpath("zoneinfo", *parts).read_bytes()
    except (ModuleNotFoundError, OSError):
        return None


def posix_tz(name: str) -> str | None:
    """The POSIX TZ string of an IANA zone; `None` without one.

    It is the footer of the zone's TZif file (version 2 or later, RFC 8536
    3.3), for times after its last transition. Blocking (reads a file).
    """
    data = _tzif(name)
    if not data or data[:4] != b"TZif" or data[4:5] < b"2" or not data.endswith(b"\n"):
        return None
    footer = data[data.rfind(b"\n", 0, len(data) - 1) + 1 : -1]
    try:
        tz = footer.decode("ascii")
    except UnicodeDecodeError:
        return None
    return tz if tz.isprintable() and tz else None


def zone_name(name: str) -> bool:
    """Whether the hub takes `name` as `tz_name`."""
    return bool(_NAME.fullmatch(name))
