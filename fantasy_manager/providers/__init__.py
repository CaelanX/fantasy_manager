"""League provider factory."""
from __future__ import annotations

from typing import Any

from .base import LeagueProvider, ProviderError


class ProviderUnavailable(ProviderError):
    """The provider's module is missing or incomplete (feature not available yet)."""


PROVIDERS = ("espn", "fantrax")


def get_provider(name: str, settings: Any, cache: Any) -> LeagueProvider:
    """Instantiate the provider for ``name`` ("espn" or "fantrax")."""
    name = (name or "").lower()
    if name == "espn":
        from .espn import EspnProvider

        return EspnProvider(settings, cache)
    if name == "fantrax":
        try:
            from .fantrax import FantraxProvider
        except (ImportError, AttributeError, SyntaxError) as e:
            raise ProviderUnavailable(f"Fantrax support is not available yet ({e}).") from e
        return FantraxProvider(settings, cache)
    raise ProviderError(f"unknown league provider {name!r}; expected one of {', '.join(PROVIDERS)}")


def clear_league_cache(name: str, settings: Any, cache: Any) -> int:
    """Drop every cached response that holds ``name``'s league state (rosters / lineups, settings,
    free agents), so the next load reads the league live. NHL / news caches are left alone.
    Returns how many entries were removed."""
    name = (name or "").lower()
    if name == "espn":
        from .espn import ESPN_HOST

        return cache.clear(url_contains=ESPN_HOST)
    if name == "fantrax":
        from .fantrax import clear_cache

        return clear_cache(settings.fm_data_dir)
    return 0


__all__ = ["get_provider", "clear_league_cache", "ProviderError", "ProviderUnavailable", "LeagueProvider",
           "PROVIDERS"]
