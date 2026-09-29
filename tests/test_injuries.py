import json
from datetime import date, datetime, timezone
from pathlib import Path

import pytest

from fantasy_manager.providers.injuries import (
    ESPN_TEAM_TO_NHL, INJURIES_URL, fetch_injuries, map_status, team_to_nhl,
)

FIX = Path(__file__).parent / "fixtures" / "injuries" / "espn_injuries.json"


@pytest.fixture
def reports():
    payload = json.loads(FIX.read_text(encoding="utf-8"))
    calls = []

    def fetch(url, params=None):
        calls.append(url)
        return payload

    out = fetch_injuries(fetch)
    assert calls == [INJURIES_URL]
    return {r.player_name: r for r in out}


def test_parse_basic_fields(reports):
    greer = reports["A.J. Greer"]
    assert greer.espn_player_id == 3648015
    assert greer.team_name == "Anaheim Ducks" and greer.team == "ANA"
    assert greer.position == "LW"
    assert greer.status_raw == "Day-To-Day" and greer.status == "dtd"
    assert greer.injury_type == "Upper Body"
    assert greer.return_date == date(2026, 10, 2)
    assert greer.updated == datetime(2026, 9, 27, 13, 59, tzinfo=timezone.utc)
    assert greer.comment
    assert not greer.is_out


def test_statuses(reports):
    assert reports["Troy Terry"].status == "out"
    assert reports["Matthew Poitras"].status == "ir"
    assert reports["Charlie McAvoy"].status == "suspended"
    assert reports["Connor Hellebuyck"].status == "suspended"
    assert reports["Connor Hellebuyck"].is_out


def test_team_abbrev_translation(reports):
    # ESPN abbreviates New Jersey as "NJ"; NHL uses "NJD"
    assert reports["Seamus Casey"].team == "NJD"
    # ESPN calls Utah "Utah Mammoth" (2026-27)
    lam = reports["Maveric Lamoureux"]
    assert lam.team_name == "Utah Mammoth" and lam.team == "UTA"


@pytest.mark.parametrize("raw,expected", [
    ("Day-To-Day", "dtd"),
    ("day-to-day", "dtd"),
    ("Out", "out"),
    ("Injured Reserve", "ir"),
    ("IR", "ir"),
    ("Long-Term Injured Reserve", "ltir"),
    ("LTIR", "ltir"),
    ("Suspension", "suspended"),
    ("Suspended", "suspended"),
    ("Questionable", "dtd"),
    ("", "unknown"),
    (None, "unknown"),
    ("Active", "unknown"),
])
def test_map_status(raw, expected):
    assert map_status(raw) == expected


def test_map_status_type_name_fallback():
    assert map_status(None, "INJURY_STATUS_IR") == "ir"
    assert map_status(None, "INJURY_STATUS_DAYTODAY") == "dtd"
    assert map_status(None, "INJURY_STATUS_SUSPENSION") == "suspended"


def test_team_table():
    assert len(set(ESPN_TEAM_TO_NHL.values())) == 32
    assert team_to_nhl("Utah Hockey Club") == "UTA"
    assert team_to_nhl("Utah Mammoth") == "UTA"
    assert team_to_nhl(espn_abbrev="TB") == "TBL"
    assert team_to_nhl(espn_abbrev="EDM") == "EDM"
    assert team_to_nhl("Nowhere Nobodies") is None
