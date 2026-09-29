"""Point presets for the backtest and fantasy points per game under a ScoringConfig.

``espn`` and ``fantrax`` are the live leagues' point values (captured from
``fm settings --league X --json``; the same values are stored as test fixtures in
``tests/fixtures/backtest/scoring_*.json``). Stats the NHL season reports do not carry
(ESPN ``HAT`` hat tricks and ``DEF``, Fantrax ``FT`` fights) simply contribute nothing, so
backtest FPG is slightly below what the platforms would show for those players.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Mapping

from ..models import ScoringConfig
from ..scoring import PointsScoring

PRESETS: dict[str, ScoringConfig] = {
    "espn": ScoringConfig(kind="points", weights={
        "G": 2.0, "A": 1.0, "PPP": 0.5, "SHP": 0.5, "SOG": 0.1, "HIT": 0.1, "BLK": 0.5, "DEF": 0.1,
        "HAT": 1.0, "W": 2.0, "SV": 0.2, "GA": -1.5, "SO": 2.0, "OTL": 1.0}),
    "fantrax": ScoringConfig(kind="points", weights={
        "G": 4.0, "A": 2.0, "PPP": 1.0, "SHG": 2.0, "SOG": 0.5, "HIT": 0.3, "BLK": 0.5, "PIM": 0.5,
        "ENG": 2.0, "FT": 3.0, "W": 5.0, "SV": 0.25, "GA": -1.0, "SO": 7.5},
        goalie_weights={"G": 20.0, "A": 3.0}),
    # A generic points league (Yahoo/Fantrax-style defaults).
    "default": ScoringConfig(kind="points", weights={
        "G": 3.0, "A": 2.0, "PPP": 1.0, "SHP": 1.0, "SOG": 0.4, "HIT": 0.2, "BLK": 0.4,
        "W": 4.0, "SV": 0.2, "GA": -1.0, "SO": 3.0, "OTL": 1.0}),
}

# Stats the backtest data cannot supply (their weight is ignored).
UNAVAILABLE = frozenset({"HAT", "DEF", "FT", "STP", "GAA", "SVPCT"})


def load_scoring(name_or_path: str) -> tuple[str, ScoringConfig]:
    """('espn', cfg) for a preset name, or (stem, cfg) for a JSON file holding a ScoringConfig
    (either bare or under a "scoring" key, as saved from ``fm settings --json``)."""
    key = (name_or_path or "espn").strip()
    if key.lower() in PRESETS:
        return key.lower(), PRESETS[key.lower()]
    p = Path(key)
    if not p.exists():
        raise ValueError(f"unknown scoring {key!r}; use one of {', '.join(PRESETS)} or a JSON file")
    d = json.loads(p.read_text(encoding="utf-8"))
    return p.stem, ScoringConfig.model_validate(d.get("scoring", d))


def scorer(cfg: ScoringConfig) -> PointsScoring:
    if cfg.kind != "points":
        raise ValueError("the backtest scores points leagues only")
    return PointsScoring(cfg.weights, cfg.goalie_weights)


def fpg(rates: Mapping[str, float] | None, scoring: PointsScoring) -> float:
    """Fantasy points per game of per-game rates (0.0 for empty rates)."""
    return float(scoring.value(dict(rates))) if rates else 0.0


def missing_weights(cfg: ScoringConfig) -> list[str]:
    return sorted(k for k in {**cfg.weights, **cfg.goalie_weights} if k in UNAVAILABLE)
