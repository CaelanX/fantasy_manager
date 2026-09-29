"""Versioned valuation-parameter overrides for the harness (M3, ``fm harness refit``).

Layout under ``<fm_data_dir>/harness/params/`` (``valuation.params.params_dir()``):

* ``vNNNN.json`` - one file per version: ``{version, parent, created, params, hash,
  changed_keys, metrics{holdout_before, holdout_after, hist_before, hist_after, n_live}, status,
  applied_by, applied_at, changelog[], shadow}``. ``params`` holds only the overridden keys
  (the same nested shape as the packaged ``valuation/fitted_params.json``) and is cumulative:
  a version carries its parent's overrides plus its own changes, so activating it alone
  reproduces it exactly. ``hash`` is ``params_hash`` of packaged merged with ``params``.
* ``active.json`` - the pointer ``{"version": "v0003"}``; ``{"version": null}`` (or no file)
  means the packaged params only.

Statuses: ``proposed`` (fitted, not applied), ``active`` (the pointer's target), ``shadow``
(the version an active one replaced; shadow-scored weekly against it, champion/challenger),
``rolled_back`` (was active, then rolled back) and ``retired`` (an old shadow once a newer
promotion happened). The parent of the first version is ``packaged``.

Every write is mirrored into the ledger's ``param_versions`` table when a ledger is given
(``sync_ledger`` rebuilds the table from the files: the files are the source of truth).
After ``apply`` / ``rollback`` the in-process params are reloaded (``valuation.params.reload``).
"""
from __future__ import annotations

import json
import os
import re
import tempfile
from datetime import date, datetime
from pathlib import Path
from typing import Any, Iterable, Mapping

from ..valuation import params as vparams

PACKAGED = "packaged"
STATUSES = ("proposed", "active", "shadow", "rolled_back", "retired")
_VERSION_RE = re.compile(r"^v(\d{4,})$")


def _now() -> str:
    return datetime.now().isoformat(timespec="seconds")


def _write_json(path: Path, data: Mapping[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=f".{path.stem}-", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2, sort_keys=True, default=str)
            f.write("\n")
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def merged_hash(override: Mapping[str, Any]) -> str:
    """params_hash of the packaged params deep-merged with ``override`` ({} -> packaged hash)."""
    return vparams.params_hash(vparams.deep_merge(vparams.load_packaged(), override))


class ParamsStore:
    """The versions directory (default ``valuation.params.params_dir()``) plus an optional
    ledger to mirror into."""

    def __init__(self, pdir: str | Path | None = None, ledger: Any = None, data_dir: str | Path | None = None):
        if pdir is None:
            base = data_dir if data_dir is not None else getattr(ledger, "data_dir", None)
            pdir = os.environ.get(vparams.PARAMS_DIR_ENV) or vparams.params_dir(base)
        self.dir = Path(pdir)
        self.ledger = ledger

    # -- reading --------------------------------------------------------------
    def path(self, version: str) -> Path:
        return self.dir / f"{version}.json"

    def get(self, version: str) -> dict[str, Any] | None:
        if version == PACKAGED or not _VERSION_RE.match(version or ""):
            return None
        try:
            rec = json.loads(self.path(version).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        return rec if isinstance(rec, dict) else None

    def versions(self) -> list[dict[str, Any]]:
        """Every readable version record, oldest first."""
        out = []
        if not self.dir.is_dir():
            return out
        for p in sorted(self.dir.glob("v*.json")):
            if _VERSION_RE.match(p.stem):
                rec = self.get(p.stem)
                if rec is not None:
                    out.append(rec)
        return out

    history = versions

    def active_name(self) -> str:
        """The pointer's version, or ``packaged``."""
        try:
            ptr = json.loads((self.dir / vparams.ACTIVE_FILE).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return PACKAGED
        v = ptr.get("version") if isinstance(ptr, dict) else None
        return v if isinstance(v, str) and self.get(v) is not None else PACKAGED

    def active(self) -> dict[str, Any] | None:
        name = self.active_name()
        return None if name == PACKAGED else self.get(name)

    def active_params(self) -> dict[str, Any]:
        """The active version's override params ({} for packaged)."""
        rec = self.active()
        return dict(rec.get("params") or {}) if rec else {}

    def next_version(self) -> str:
        nums = [int(m.group(1)) for p in (self.dir.glob("v*.json") if self.dir.is_dir() else [])
                if (m := _VERSION_RE.match(p.stem))]
        return f"v{(max(nums) + 1 if nums else 1):04d}"

    # -- writing --------------------------------------------------------------
    def _save(self, rec: Mapping[str, Any]) -> None:
        _write_json(self.path(rec["version"]), rec)
        if self.ledger is not None:
            self._mirror([rec])

    def _log(self, rec: dict[str, Any], event: str, by: str, note: str | None = None) -> None:
        rec.setdefault("changelog", []).append({"at": _now(), "event": event, "by": by, "note": note})

    def _point(self, version: str | None) -> None:
        _write_json(self.dir / vparams.ACTIVE_FILE, {"version": version, "updated": _now()})
        vparams.reload()

    def propose(self, params: Mapping[str, Any], changed_keys: Iterable[str], metrics: Mapping[str, Any] | None = None,
                note: str | None = None, parent: str | None = None, by: str = "refit",
                extra: Mapping[str, Any] | None = None) -> dict[str, Any]:
        """Write a new ``proposed`` version (``params``: the full cumulative override)."""
        version = self.next_version()
        rec: dict[str, Any] = {
            "version": version, "parent": parent or self.active_name(), "created": _now(),
            "params": json.loads(json.dumps(dict(params))), "hash": merged_hash(params),
            "changed_keys": sorted(changed_keys), "metrics": dict(metrics or {}), "status": "proposed",
            "applied_by": None, "applied_at": None, "changelog": [], **dict(extra or {})}
        self._log(rec, "proposed", by, note)
        self._save(rec)
        return rec

    def apply(self, version: str, by: str = "manual", note: str | None = None,
              as_of: date | None = None) -> dict[str, Any]:
        """Activate ``version``: the previously active version becomes its ``shadow`` (scored
        weekly against it), an older shadow is retired, and the pointer moves."""
        rec = self.get(version)
        if rec is None:
            raise ValueError(f"unknown params version {version!r}")
        prev = self.active_name()
        if prev == version:
            return rec
        for other in self.versions():
            if other["version"] == version:
                continue
            if other.get("status") == "shadow":
                other["status"] = "retired"
                self._log(other, "retired", by, f"superseded by {version}")
                self._save(other)
            if other["version"] == prev:
                other["status"] = "shadow"
                self._log(other, "shadowed", by, f"replaced by {version}")
                self._save(other)
        rec["status"] = "active"
        rec["applied_by"] = by
        rec["applied_at"] = _now()
        rec["shadow"] = {"version": prev, "since": (as_of or date.today()).isoformat(), "weeks": {}}
        self._log(rec, "applied", by, note)
        self._save(rec)
        self._point(version)
        return rec

    def rollback(self, to: str | None = None, by: str = "manual", note: str | None = None) -> dict[str, Any]:
        """Roll the active version back to ``to`` (default its parent; ``packaged`` for none).
        Returns {"from", "to", "hash"}; the rolled-back version is marked ``rolled_back``."""
        cur = self.active()
        if cur is None and (to is None or to == PACKAGED):
            raise ValueError("nothing to roll back: the packaged params are active")
        target = to or (cur or {}).get("parent") or PACKAGED
        if target != PACKAGED and self.get(target) is None:
            raise ValueError(f"unknown params version {target!r}")
        if cur is not None and cur["version"] == target:
            raise ValueError(f"{target} is already active")
        if cur is not None:
            cur["status"] = "rolled_back"
            self._log(cur, "rolled_back", by, note or f"rolled back to {target}")
            self._save(cur)
        for other in self.versions():
            if other.get("status") == "shadow":
                other["status"] = "retired"
                self._log(other, "retired", by, "rollback")
                self._save(other)
        if target == PACKAGED:
            self._point(None)
        else:
            rec = self.get(target)
            assert rec is not None
            rec["status"] = "active"
            rec["applied_by"] = by
            rec["applied_at"] = _now()
            rec["shadow"] = None
            self._log(rec, "reactivated", by, note or f"rollback from {cur['version'] if cur else PACKAGED}")
            self._save(rec)
            self._point(target)
        return {"from": cur["version"] if cur else PACKAGED, "to": target, "hash": vparams.params_hash()}

    def update(self, rec: Mapping[str, Any]) -> None:
        """Save a modified record (e.g. its shadow-scoring weeks)."""
        self._save(dict(rec))

    # -- ledger mirror --------------------------------------------------------
    @staticmethod
    def ledger_row(rec: Mapping[str, Any]) -> dict[str, Any]:
        return {"version": rec["version"], "parent": rec.get("parent"), "params_hash": rec.get("hash"),
                "status": rec.get("status"), "created_at": rec.get("created"),
                "metrics_json": json.dumps(rec.get("metrics") or {}, sort_keys=True, default=str),
                "changelog": json.dumps(rec.get("changelog") or [], default=str)}

    def _mirror(self, recs: Iterable[Mapping[str, Any]]) -> None:
        self.ledger.upsert("param_versions", [self.ledger_row(r) for r in recs], ("version",))

    def sync_ledger(self, ledger: Any = None) -> int:
        """Rebuild the ledger's ``param_versions`` rows from the version files."""
        led = ledger if ledger is not None else self.ledger
        if led is None:
            return 0
        recs = self.versions()
        led.execute("DELETE FROM param_versions")
        led.commit()
        if recs:
            led.upsert("param_versions", [self.ledger_row(r) for r in recs], ("version",))
        return len(recs)


# ---- functional API -------------------------------------------------------------------------

def store(ledger: Any = None, pdir: str | Path | None = None) -> ParamsStore:
    return ParamsStore(pdir, ledger)


def propose(params: Mapping[str, Any], changed_keys: Iterable[str], metrics: Mapping[str, Any] | None = None,
            ledger: Any = None, pdir: str | Path | None = None, **kw: Any) -> dict[str, Any]:
    return ParamsStore(pdir, ledger).propose(params, changed_keys, metrics, **kw)


def apply(version: str, ledger: Any = None, pdir: str | Path | None = None, **kw: Any) -> dict[str, Any]:
    return ParamsStore(pdir, ledger).apply(version, **kw)


def rollback(to: str | None = None, ledger: Any = None, pdir: str | Path | None = None, **kw: Any) -> dict[str, Any]:
    return ParamsStore(pdir, ledger).rollback(to, **kw)


def history(ledger: Any = None, pdir: str | Path | None = None) -> list[dict[str, Any]]:
    return ParamsStore(pdir, ledger).versions()


__all__ = ["PACKAGED", "STATUSES", "ParamsStore", "apply", "history", "merged_hash", "propose", "rollback", "store"]
