"""`fm schedule` / `fm matchup` with the fake provider, and the /schedule and /matchup pages."""
import json
from datetime import date

import pytest
from rich.console import Console
from typer.testing import CliRunner

from fantasy_manager import cli
from fantasy_manager.analysis import matchup as mu
from fantasy_manager.providers import espn

from .test_matchup import TODAY, make_league

runner = CliRunner()


def _patch(monkeypatch, tmp_path):
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("ESPN_LEAGUE_ID", "1")
    monkeypatch.setenv("FM_OFFLINE", "1")  # enrichment and ESPN extras degrade to warnings, never the network
    cli.get_settings.cache_clear()
    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: make_league()[0])
    monkeypatch.setattr(cli, "console", Console(width=250))


def test_schedule_command(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["schedule", "--week", "2026-10-07"])
    assert res.exit_code == 0, res.output
    assert "week of Mon Oct 05 - Sun Oct 11" in res.output and "AAA" in res.output
    assert "Best NHL teams to stream from" in res.output
    res = runner.invoke(cli.app, ["schedule", "--stream", "--playoffs", "--season"])
    assert res.exit_code == 0, res.output
    assert "Streaming targets: F" in res.output and "FA1" in res.output
    assert "Fantasy playoffs" in res.output and "Games per NHL team per fantasy week" in res.output
    res = runner.invoke(cli.app, ["--json", "schedule", "--week", "2026-10-05", "--stream", "--playoffs"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.output)
    assert data["week"]["start"] == "2026-10-05" and [r["team"] for r in data["week"]["rows"]][0] == "AAA"
    assert data["streaming"]["by_slot"]["F"][0]["cid"] == "fa1"
    assert data["playoffs"]["source"].startswith("NHL calendar weeks") and data["season"] is None
    assert any("League periods unavailable" in n for n in data["playoffs"]["notes"])   # offline ESPN
    bad = runner.invoke(cli.app, ["schedule", "--week", "soon"])
    assert bad.exit_code == 1


def test_matchup_command(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["matchup"])
    assert res.exit_code == 0, res.output
    assert "unknown opponent" in res.output and "Live matchup scores unavailable" in res.output
    scores = mu.ScoreInfo(period=1, start=date(2026, 10, 5), end=date(2026, 10, 11), opponent_id="2",
                          my_points=5.0, their_points=50.0, source="fake")
    monkeypatch.setattr(mu, "fetch_scores", lambda *a, **k: scores)
    res = runner.invoke(cli.app, ["matchup", "--seed", "4"])
    assert res.exit_code == 0, res.output
    assert "Mine vs Theirs" in res.output and "Win probability" in res.output and "Chase" in res.output
    res = runner.invoke(cli.app, ["--json", "matchup", "--seed", "4"])
    data = json.loads(res.output)
    assert data["opponent_team"] == "Theirs" and data["stance"] == "chase"
    assert 0.0 <= data["win_probability"] < 0.4 and data["my_points_so_far"] == 5.0
    assert set(data["gap_by_position"]) == {"F", "D", "G"} and data["meta"]["provider"] == "espn"


# --------------------------------------------------------------------------- web

pytest.importorskip("fastapi")
pytest.importorskip("jinja2")

from fastapi.testclient import TestClient  # noqa: E402

from fantasy_manager.web import views  # noqa: E402
from fantasy_manager.web.app import LoadResult, create_app  # noqa: E402


def _result(league="espn"):
    ctx, values = make_league()
    return LoadResult(ctx=ctx, values=values, dynasty=None, recs=[])


@pytest.fixture
def client():
    return TestClient(create_app(lambda league: _result(league)))


def test_schedule_page(client):
    r = client.get("/schedule?league=espn&week=2026-10-06")
    assert r.status_code == 200
    h = r.text
    assert "Week of Oct 5" in h and "Games by team" in h and 'class="gcell offn"' in h
    assert "Streaming targets" in h and "FA1" in h and "Best teams to stream from" in h
    assert "Fantasy playoffs" in h and "Season at a glance" in h
    assert 'href="/schedule?league=espn&amp;week=2026-10-12"' in h          # next week
    assert '<a class="chip-f" href="/schedule?league=espn" aria-current="page">This week</a>' in h  # as_of Oct 8
    assert 'aria-current="page">Schedule</a>' in h                          # nav tab
    assert client.get("/schedule?week=garbage").status_code == 200          # bad date -> this week


def test_matchup_page_without_scores(client):
    r = client.get("/matchup")
    assert r.status_code == 200
    assert "No opponent was found" in r.text and 'aria-current="page">Matchup</a>' in r.text


def test_matchup_page_with_scores(client, monkeypatch):
    scores = mu.ScoreInfo(period=1, start=date(2026, 10, 5), end=date(2026, 10, 11), opponent_id="2",
                          my_points=70.0, their_points=10.0, source="fake")
    monkeypatch.setattr(mu, "fetch_scores", lambda *a, **k: scores)
    h = client.get("/matchup?league=espn").text
    assert "Mine <span class=\"vs\">vs</span> Theirs" in h and "Win probability" in h
    assert "wp-bar" in h and "Protect" in h and "Gap by position" in h and "Their key players" in h
    assert "70.0" in h and "Your lineup" in h and "Their lineup" in h


def test_provider_for():
    base = type("B", (), {"provider": "P"})()
    loader = type("L", (), {"_bases": {"espn": (1.0, base)}})()
    assert views.provider_for(loader, "espn") == "P"
    assert views.provider_for(loader, "fantrax") is None and views.provider_for(object(), "espn") is None
