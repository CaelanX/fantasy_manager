from datetime import date

import pytest

from fantasy_manager.config import Settings, default_espn_year, parse_points


def test_parse_points():
    assert parse_points("G=3, a=2,SOG=0.4") == {"G": 3.0, "A": 2.0, "SOG": 0.4}
    assert parse_points("") == {}
    with pytest.raises(ValueError):
        parse_points("G3")


def test_default_year():
    assert default_espn_year(date(2026, 9, 28)) == 2027
    assert default_espn_year(date(2027, 3, 1)) == 2027


def test_settings_from_env(monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)  # no .env here
    monkeypatch.setenv("ESPN_LEAGUE_ID", "4242")
    monkeypatch.setenv("ESPN_YEAR", "")
    monkeypatch.setenv("FANTRAX_POINTS", "G=3,A=2")
    monkeypatch.setenv("FANTRAX_DYNASTY", "true")
    monkeypatch.setenv("FM_OFFLINE", "1")
    s = Settings()
    assert s.espn_league_id == 4242 and s.espn_year == default_espn_year()
    assert s.fantrax_points == {"G": 3.0, "A": 2.0}
    assert s.fantrax_dynasty and s.fm_offline and s.fm_llm_model == "openrouter/free"


def test_fantrax_mode(monkeypatch, tmp_path):
    import pydantic

    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FANTRAX_MODE", raising=False)
    assert Settings(_env_file=None).fantrax_mode == "balanced"
    monkeypatch.setenv("FANTRAX_MODE", "Rebuild")
    assert Settings(_env_file=None).fantrax_mode == "rebuild"
    monkeypatch.setenv("FANTRAX_MODE", "tank")
    with pytest.raises(pydantic.ValidationError):
        Settings(_env_file=None)
