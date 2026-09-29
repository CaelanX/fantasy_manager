"""User preferences set from the dashboard or `fm mode`, kept in ``<FM_DATA_DIR>/prefs.json``.

A tiny JSON object ({"dynasty_mode": "balanced", ...}). Reads never fail: a missing, unreadable
or corrupt file behaves like an empty one. Writes are atomic (temp file + ``os.replace``).

Dynasty mode precedence: prefs.json (dashboard toggle / ``fm mode``) > ``FANTRAX_MODE`` (env or
.env) > ``DEFAULT_DYNASTY_MODE``. A per-run ``fm --mode`` override sits above all three and is
never persisted.
"""
from __future__ import annotations

import json
import os
import tempfile
import threading
from pathlib import Path
from typing import Any

PREFS_FILE = "prefs.json"
DYNASTY_MODE_KEY = "dynasty_mode"
DYNASTY_MODES = ("contend", "balanced", "rebuild")
DEFAULT_DYNASTY_MODE = "balanced"
HARNESS_AUTO_APPLY_KEY = "harness_auto_apply"
# source code -> human label ("Dynasty mode: balanced (from dashboard/prefs)")
MODE_SOURCE_LABEL = {
    "prefs": "dashboard/prefs",
    "env": "FANTRAX_MODE",
    "default": "default",
    "option": "--mode, this run only",
}

_lock = threading.Lock()


def _data_dir(data_dir: str | Path | None) -> Path:
    if data_dir is not None:
        return Path(data_dir)
    from .config import get_settings

    return Path(get_settings().fm_data_dir)


def prefs_path(data_dir: str | Path | None = None) -> Path:
    return _data_dir(data_dir) / PREFS_FILE


def load_prefs(data_dir: str | Path | None = None) -> dict[str, Any]:
    """The whole prefs object; {} when the file is missing, unreadable or not a JSON object."""
    try:
        raw = prefs_path(data_dir).read_text(encoding="utf-8")
        data = json.loads(raw)
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def get_pref(key: str, default: Any = None, data_dir: str | Path | None = None) -> Any:
    return load_prefs(data_dir).get(key, default)


def set_pref(key: str, value: Any, data_dir: str | Path | None = None) -> None:
    """Set (or, with ``value=None``, remove) one pref; atomic write, creates the data dir."""
    path = prefs_path(data_dir)
    with _lock:
        data = load_prefs(path.parent)
        if value is None:
            data.pop(key, None)
        else:
            data[key] = value
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp = tempfile.mkstemp(prefix=".prefs-", suffix=".tmp", dir=path.parent)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                json.dump(data, f, indent=2, sort_keys=True)
                f.write("\n")
            os.replace(tmp, path)
        except BaseException:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise


def normalize_mode(value: Any) -> str | None:
    """'Rebuild ' -> 'rebuild'; None for anything that is not a known dynasty mode."""
    if not isinstance(value, str):
        return None
    v = value.strip().lower()
    return v if v in DYNASTY_MODES else None


def dynasty_mode_info(settings: Any = None) -> tuple[str, str]:
    """(effective mode, source) with source one of "prefs", "env", "default"."""
    if settings is None:
        from .config import get_settings

        settings = get_settings()
    pref = normalize_mode(get_pref(DYNASTY_MODE_KEY, data_dir=getattr(settings, "fm_data_dir", None) or "."))
    if pref:
        return pref, "prefs"
    env = normalize_mode(getattr(settings, "fantrax_mode", None))
    if env and "fantrax_mode" in (getattr(settings, "model_fields_set", None) or set()):
        return env, "env"
    return DEFAULT_DYNASTY_MODE, "default"


def harness_auto_apply(data_dir: str | Path | None = None) -> bool:
    """``harness_auto_apply`` (default True): may ``fm harness daily`` apply a Tier A params
    refit on a refit day (from 2026-11-16) when it passes the gate?"""
    v = get_pref(HARNESS_AUTO_APPLY_KEY, True, data_dir=data_dir)
    if isinstance(v, str):
        return v.strip().lower() not in ("0", "false", "no", "off")
    return bool(v)


def effective_dynasty_mode(settings: Any = None) -> str:
    return dynasty_mode_info(settings)[0]


def mode_source_label(source: str | None) -> str:
    return MODE_SOURCE_LABEL.get(source or "", source or "default")


__all__ = ["DEFAULT_DYNASTY_MODE", "DYNASTY_MODES", "DYNASTY_MODE_KEY", "HARNESS_AUTO_APPLY_KEY", "MODE_SOURCE_LABEL",
           "PREFS_FILE", "dynasty_mode_info", "effective_dynasty_mode", "get_pref", "harness_auto_apply",
           "load_prefs", "mode_source_label",
           "normalize_mode", "prefs_path", "set_pref"]
