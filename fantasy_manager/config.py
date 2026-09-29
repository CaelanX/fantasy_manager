"""Runtime configuration loaded from environment variables / `.env`."""
from __future__ import annotations

from datetime import date
from functools import lru_cache
from pathlib import Path
from typing import Annotated, Any, Literal

from pydantic import Field, SecretStr, field_validator, model_validator
from pydantic_settings import BaseSettings, NoDecode, SettingsConfigDict


def default_espn_year(today: date | None = None) -> int:
    """ESPN labels a season by the year it ends: Sep 2026 onward -> 2027."""
    today = today or date.today()
    return today.year + 1 if today.month >= 9 else today.year


GOALIE_PREFIX = "goalie."
POINTS_ALIASES = {"FIGHTS": "FT", "FTS": "FT"}


def parse_points(raw: Any) -> dict[str, float]:
    """Parse "G=3,A=2" (or an existing mapping) into {stat: points}.

    A ``goalie.`` prefix marks a goalie-group override ("goalie.G=20,goalie.A=3"): it is kept
    as the key ``goalie.<STAT>``; ``split_points`` separates the two groups.
    """
    if raw is None or raw == "":
        return {}
    items: list[tuple[str, Any]]
    if isinstance(raw, dict):
        items = list(raw.items())
    else:
        items = []
        for part in str(raw).split(","):
            part = part.strip()
            if not part:
                continue
            if "=" not in part:
                raise ValueError(f"bad points entry {part!r}; expected STAT=value")
            items.append(tuple(part.split("=", 1)))  # type: ignore[arg-type]
    out: dict[str, float] = {}
    for key, val in items:
        k = str(key).strip()
        prefix = ""
        if k.lower().startswith(GOALIE_PREFIX):
            prefix, k = GOALIE_PREFIX, k[len(GOALIE_PREFIX):]
        k = k.strip().upper()
        if not k:
            raise ValueError(f"bad points entry {key!r}; expected STAT=value")
        out[prefix + POINTS_ALIASES.get(k, k)] = float(str(val).strip())
    return out


def split_points(points: dict[str, float]) -> tuple[dict[str, float], dict[str, float]]:
    """{stat: pts} with optional ``goalie.`` keys -> (base weights, goalie-group overrides)."""
    base = {k: v for k, v in points.items() if not k.startswith(GOALIE_PREFIX)}
    goalie = {k[len(GOALIE_PREFIX):]: v for k, v in points.items() if k.startswith(GOALIE_PREFIX)}
    return base, goalie


def format_points(weights: dict[str, float], goalie_weights: dict[str, float] | None = None) -> str:
    """Inverse of parse_points: "G=4,A=2,...,goalie.G=20"."""
    parts = [f"{k}={v:g}" for k, v in weights.items()]
    parts += [f"{GOALIE_PREFIX}{k}={v:g}" for k, v in (goalie_weights or {}).items()]
    return ",".join(parts)


class Settings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        env_ignore_empty=True,
        extra="ignore",
        case_sensitive=False,
    )

    # ESPN
    espn_league_id: int | None = None
    espn_year: int | None = None
    espn_s2: str | None = Field(default=None, exclude=True, repr=False)
    espn_swid: str | None = Field(default=None, exclude=True, repr=False)
    espn_team: str | None = None

    # Fantrax
    fantrax_league_id: str | None = None
    # Secrets are SecretStr (masked in repr/str) and excluded from model_dump / JSON output.
    fantrax_cookie: SecretStr | None = Field(default=None, exclude=True)
    # Programmatic login (recommended): the tool logs in itself and keeps the session in
    # <FM_DATA_DIR>/fantrax_session.json. Stored only in the local .env.
    fantrax_username: SecretStr | None = Field(default=None, exclude=True)
    fantrax_password: SecretStr | None = Field(default=None, exclude=True)
    fantrax_cookie_file: Path | None = None
    fantrax_team: str | None = None
    fantrax_points: Annotated[dict[str, float], NoDecode] = {}
    fantrax_dynasty: bool = False
    fantrax_keeper_horizon_years: int = 3
    # Dynasty priority: contend (win this season first), balanced, or rebuild. A mode chosen on the
    # dashboard or with `fm mode` (<FM_DATA_DIR>/prefs.json) takes precedence; see prefs.py.
    fantrax_mode: Literal["contend", "balanced", "rebuild"] = "balanced"

    # LLM / notifications
    openrouter_api_key: str | None = Field(default=None, exclude=True, repr=False)
    fm_llm_model: str = "openrouter/free"
    # Comma-separated fallback models tried in order when the primary is rate-limited or empty.
    fm_llm_fallbacks: str = ""
    # Only free models (openrouter/free or ids ending in ':free') are allowed unless this is true.
    fm_llm_allow_paid: bool = False
    discord_webhook_url: str | None = Field(default=None, exclude=True, repr=False)
    slack_webhook_url: str | None = Field(default=None, exclude=True, repr=False)

    # Runtime
    fm_data_dir: Path = Path("./data")
    fm_offline: bool = False

    @field_validator("fantrax_mode", mode="before")
    @classmethod
    def _parse_mode(cls, v: Any) -> Any:
        return v.strip().lower() if isinstance(v, str) else v

    @field_validator("fantrax_points", mode="before")
    @classmethod
    def _parse_points(cls, v: Any) -> dict[str, float]:
        return parse_points(v)

    @model_validator(mode="after")
    def _fill_year(self) -> "Settings":
        if self.espn_year is None:
            self.espn_year = default_espn_year()
        return self


def secret_value(v: SecretStr | str | None) -> str:
    """Plain value of a SecretStr setting ('' when unset). Never log the result."""
    if v is None:
        return ""
    return v.get_secret_value() if isinstance(v, SecretStr) else str(v)


@lru_cache(maxsize=1)
def get_settings() -> Settings:
    return Settings()
