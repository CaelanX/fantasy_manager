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


__all__ = ["get_provider", "ProviderError", "ProviderUnavailable", "LeagueProvider", "PROVIDERS"]
