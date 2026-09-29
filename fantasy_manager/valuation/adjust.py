"""Availability adjustments for injury / suspension status."""
from __future__ import annotations

from typing import Literal

Horizon = Literal["week", "season"]

# status -> (week multiplier, season multiplier)
AVAILABILITY: dict[str, tuple[float, float]] = {
    "healthy": (1.0, 1.0),
    "dtd": (0.75, 0.75),
    "out": (0.0, 0.6),
    "ir": (0.0, 0.4),
    "ltir": (0.0, 0.1),
    "suspended": (0.0, 0.5),
    "unknown": (1.0, 1.0),
}


def availability_multiplier(status: str, horizon: Horizon = "season") -> float:
    week, season = AVAILABILITY.get(status, (1.0, 1.0))
    return week if horizon == "week" else season
