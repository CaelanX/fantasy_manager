"""Availability adjustments for injury / suspension status.

The multipliers come from ``params.availability`` at call time (a harness version may override
them); ``AVAILABILITY`` is a read-only import-time snapshot kept for display and old imports.
"""
from __future__ import annotations

from typing import Any, Literal, Mapping

from . import params as _params

Horizon = Literal["week", "season"]

# status -> (week multiplier, season multiplier); import-time snapshot, live code uses the accessor
AVAILABILITY: dict[str, tuple[float, float]] = _params.availability_table()


def availability_multiplier(status: str, horizon: Horizon = "season",
                            params: Mapping[str, Any] | None = None) -> float:
    return _params.availability(status, horizon, params)
