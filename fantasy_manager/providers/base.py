"""League provider interface."""
from __future__ import annotations

from typing import Protocol, runtime_checkable

from ..models import LeagueContext


@runtime_checkable
class LeagueProvider(Protocol):
    def load(self) -> LeagueContext:
        """Fetch league settings, rosters and free agents as a LeagueContext."""
        ...


class ProviderError(Exception):
    """User-facing provider failure (missing credentials, unknown team, access denied...)."""
