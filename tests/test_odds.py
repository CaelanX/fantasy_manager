"""providers/odds.py: odds math, parsing, quota, caching, archive and the `fm harness odds` step."""
import json
from datetime import date, datetime, timedelta, timezone
from types import SimpleNamespace

import pytest

from fantasy_manager.providers import odds as O
from fantasy_manager.providers.nhl import NHL_TEAMS

KEY = "test-odds-key-0123456789"
NOW = datetime(2026, 10, 8, 16, 0, tzinfo=timezone.utc)


def _book(key, home, away, hp, ap, point, over, under):
    return {"key": key, "title": key.title(), "last_update": "2026-10-08T15:00:00Z", "markets": [
        {"key": "h2h", "last_update": "2026-10-08T15:00:00Z",
         "outcomes": [{"name": home, "price": hp}, {"name": away, "price": ap}]},
        {"key": "totals", "last_update": "2026-10-08T15:00:00Z",
         "outcomes": [{"name": "Over", "price": over, "point": point},
                      {"name": "Under", "price": under, "point": point}]}]}


def _event(eid, home, away, start, books):
    return {"id": eid, "sport_key": "icehockey_nhl", "sport_title": "NHL", "commence_time": start,
            "home_team": home, "away_team": away, "bookmakers": books}


# Built from the documented v4 /sports/icehockey_nhl/odds response shape.
EVENTS = [
    _event("e1", "Edmonton Oilers", "Calgary Flames", "2026-10-09T02:00:00Z", [
        _book("draftkings", "Edmonton Oilers", "Calgary Flames", -200, 170, 6.5, -105, -115),
        _book("fanduel", "Edmonton Oilers", "Calgary Flames", -190, 160, 6.5, -110, -110),
        _book("betmgm", "Edmonton Oilers", "Calgary Flames", -210, 175, 6.0, -120, 100),
    ]),
    _event("e2", "Montréal Canadiens", "Utah Mammoth", "2026-10-08T23:00:00Z", [
        _book("draftkings", "Montréal Canadiens", "Utah Mammoth", 110, -130, 6.5, 105, -125),
    ]),
    _event("e0", "Boston Bruins", "St Louis Blues", "2026-10-08T15:30:00Z", [   # already started at NOW
        _book("draftkings", "Boston Bruins", "St Louis Blues", -150, 130, 5.5, -110, -110),
    ]),
    _event("e9", "Quebec Nordiques", "Hartford Whalers", "2026-10-09T23:00:00Z", []),
]
HEADERS = {"X-Requests-Remaining": "498", "X-Requests-Used": "2", "X-Requests-Last": "2"}


class Fetch:
    def __init__(self, body=EVENTS, headers=HEADERS):
        self.body, self.headers, self.calls = body, headers, []

    def __call__(self, url, params=None):
        self.calls.append((url, dict(params or {})))
        return self.body, self.headers


class Clock:
    def __init__(self, t=NOW):
        self.t = t

    def __call__(self):
        return self.t


# --------------------------------------------------------------------------- math

def test_moneyline_to_probability_both_signs():
    assert O.american_to_prob(-150) == pytest.approx(0.6)
    assert O.american_to_prob(150) == pytest.approx(0.4)
    assert O.american_to_prob(100) == pytest.approx(0.5)
    assert O.american_to_prob(-110) == pytest.approx(110 / 210)
    for bad in (0, 50, -99):
        with pytest.raises(ValueError):
            O.american_to_prob(bad)
    for price in (-250, -150, -110, 100, 120, 300):
        assert O.prob_to_american(O.american_to_prob(price)) == price


def test_no_vig_normalization():
    h, a = O.no_vig(O.american_to_prob(-110), O.american_to_prob(-110))
    assert h == pytest.approx(0.5) and a == pytest.approx(0.5)
    h, a = O.no_vig(0.6, 0.45)
    assert h == pytest.approx(0.6 / 1.05) and h + a == pytest.approx(1.0)


def test_team_total_split():
    h, a = O.split_total(6.0, 0.5)
    assert h == pytest.approx(3.0, abs=1e-6) and a == pytest.approx(3.0, abs=1e-6)
    h, a = O.split_total(6.0, 0.65)
    assert h + a == pytest.approx(6.0) and h > 3.3 and a < 2.7
    assert O.win_probability(h, a) == pytest.approx(0.65, abs=1e-6)
    h2, a2 = O.split_total(6.0, 0.35)                      # symmetric
    assert h2 == pytest.approx(a, abs=1e-6) and a2 == pytest.approx(h, abs=1e-6)
    shares = [O.split_total(6.5, p)[0] for p in (0.3, 0.45, 0.55, 0.7, 0.8)]
    assert shares == sorted(shares)                        # monotone in the win probability
    with pytest.raises(ValueError):
        O.split_total(0, 0.5)


def test_expected_total_from_line():
    assert O.expected_total(5.5, None) == 5.5
    even = O.expected_total(6.5, 0.5)
    assert 6.5 < even < 6.8                                # Poisson median 6.5 -> mean ~6.67
    assert O.expected_total(6.5, 0.4) < even < O.expected_total(6.5, 0.6)
    assert 5.9 < O.expected_total(6.0, 0.5) < 6.3          # integer line: pushes excluded


def test_team_name_map_complete():
    assert len(O.TEAM_ABBREVS) == 32
    assert set(O.TEAM_ABBREVS.values()) == set(NHL_TEAMS)
    assert O.team_abbrev("Utah Mammoth") == "UTA" and O.team_abbrev("Utah Hockey Club") == "UTA"
    assert O.team_abbrev("Montréal Canadiens") == "MTL" and O.team_abbrev("St Louis Blues") == "STL"
    assert O.team_abbrev("St. Louis Blues") == "STL" and O.team_abbrev("Quebec Nordiques") is None


def test_eastern_date():
    assert O.eastern_date(datetime(2026, 10, 9, 2, 0, tzinfo=timezone.utc)) == date(2026, 10, 8)   # EDT
    assert O.eastern_date(datetime(2026, 12, 9, 4, 30, tzinfo=timezone.utc)) == date(2026, 12, 8)  # EST
    assert O.eastern_date(datetime(2026, 12, 9, 5, 30, tzinfo=timezone.utc)) == date(2026, 12, 9)


# --------------------------------------------------------------------------- parsing / quota

def test_parse_fixture():
    games, warnings = O.parse_events(EVENTS, now=NOW)
    assert [(g.away, g.home) for g in games] == [("UTA", "MTL"), ("CGY", "EDM")]   # started game dropped
    assert any("Quebec Nordiques" in w for w in warnings)
    mtl, edm = games
    assert mtl.game_date == date(2026, 10, 8) and edm.game_date == date(2026, 10, 8)  # 02:00Z = 22:00 EDT
    assert edm.commence_time_utc == "2026-10-09T02:00:00Z" and edm.event_id == "e1"
    assert edm.bookmaker_count == 3 and len(edm.books) == 3
    assert edm.home_ml == -200 and edm.away_ml == 170                     # median implied probability
    assert edm.total == 6.5 and (edm.over_price, edm.under_price) == (-107, -112)   # books at 6.5 only
    ph = [O.no_vig(O.american_to_prob(h), O.american_to_prob(a))[0] for h, a in ((-200, 170), (-190, 160),
                                                                               (-210, 175))]
    assert edm.home_win_prob == pytest.approx(sorted(ph)[1], abs=1e-4)
    assert edm.implied_home_total + edm.implied_away_total == pytest.approx(edm.expected_total, abs=0.02)
    assert edm.implied_home_total > edm.implied_away_total
    assert mtl.home_ml == 110 and mtl.implied_home_total < mtl.implied_away_total
    d = edm.to_dict()
    assert d["game_date"] == "2026-10-08" and O.GameOdds.from_dict(json.loads(json.dumps(d))) == edm


def test_quota_parsing():
    assert O.parse_quota(HEADERS) == {"remaining": 498, "used": 2, "last": 2}
    assert O.parse_quota({"x-requests-remaining": "497.0"}) == {"remaining": 497}
    assert O.parse_quota({}) is None and O.parse_quota({"x-requests-used": "n/a"}) is None


# --------------------------------------------------------------------------- client

def test_client_calls_once_per_20h_and_never_stores_key(tmp_path):
    fetch, clock = Fetch(), Clock()
    c = O.OddsClient(KEY, fetch_json=fetch, data_dir=tmp_path, clock=clock)
    games = c.nhl_odds()
    assert len(games) == 2 and len(fetch.calls) == 1 and c.quota["remaining"] == 498 and not c.from_cache
    url, params = fetch.calls[0]
    assert url == O.ODDS_URL and params["markets"] == "h2h,totals" and params["regions"] == "us"
    assert params["oddsFormat"] == "american" and params["apiKey"] == KEY
    assert KEY not in (tmp_path / O.CACHE_FILE).read_text(encoding="utf-8") and KEY not in repr(c)

    clock.t = NOW + timedelta(hours=19)            # a new client (next run) reuses the cache
    c2 = O.OddsClient(KEY, fetch_json=fetch, data_dir=tmp_path, clock=clock)
    assert len(c2.raw_events()) == 4 and len(fetch.calls) == 1 and c2.from_cache
    assert c2.nhl_odds() == []                     # both games have started by then
    assert c2.quota == {"remaining": 498, "used": 2, "last": 2}

    clock.t = NOW + timedelta(hours=21)
    c3 = O.OddsClient(KEY, fetch_json=fetch, data_dir=tmp_path, clock=clock)
    c3.raw_events()
    assert len(fetch.calls) == 2 and not c3.from_cache

    off = O.OddsClient(KEY, fetch_json=fetch, data_dir=tmp_path, offline=True, clock=Clock(NOW + timedelta(days=5)))
    off.raw_events()
    assert len(fetch.calls) == 2 and off.from_cache
    with pytest.raises(O.OddsError):
        O.OddsClient(KEY, fetch_json=fetch, data_dir=tmp_path / "empty", offline=True).raw_events()


def test_errors_never_carry_the_key(tmp_path):
    def boom(url, params=None):
        raise RuntimeError(f"connection failed for {url}?apiKey={params['apiKey']}")
    c = O.OddsClient(KEY, fetch_json=boom, data_dir=tmp_path)
    with pytest.raises(O.OddsError) as ei:
        c.nhl_odds()
    assert KEY not in str(ei.value) and "***" in str(ei.value)
    assert not (tmp_path / O.CACHE_FILE).exists()


def test_no_key_is_a_no_op(tmp_path, caplog):
    fetch = Fetch()
    c = O.OddsClient(None, fetch_json=fetch, data_dir=tmp_path)
    assert not c.available and c.nhl_odds() == [] and c.raw_events() == [] and fetch.calls == []
    assert not O.OddsClient("  ").available
    s = SimpleNamespace(fm_data_dir=tmp_path, fm_offline=False)       # settings without the field at all
    assert not O.OddsClient.from_settings(s).available
    from fantasy_manager.cli_harness import _odds_step
    assert _odds_step(s, date(2026, 10, 8)) == {"day": "2026-10-08", "skipped": "ODDS_API_KEY not set"}
    assert not (tmp_path / "archive").exists()


def test_settings_field_is_secret(monkeypatch, tmp_path):
    from fantasy_manager.config import Settings, secret_value
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("ODDS_API_KEY", KEY)
    s = Settings(_env_file=None)
    assert secret_value(s.odds_api_key) == KEY and s.odds_region == "us"
    assert KEY not in repr(s) and "odds_api_key" not in s.model_dump()
    assert O.OddsClient.from_settings(s).available
    monkeypatch.delenv("ODDS_API_KEY")
    assert Settings(_env_file=None).odds_api_key is None


# --------------------------------------------------------------------------- archive

def test_archive_dedupe_load_and_team_context(tmp_path):
    games, _ = O.parse_events(EVENTS, now=NOW)
    day = date(2026, 10, 8)
    p, st = O.archive_odds(games, tmp_path, day, fetched_at=NOW, quota={"remaining": 498})
    assert st == "written" and p == tmp_path / "archive" / "odds-2026-10-08.json"
    before = p.read_bytes()
    _, st2 = O.archive_odds(games[:1], tmp_path, day, fetched_at=NOW + timedelta(hours=1))
    assert st2 == "exists" and p.read_bytes() == before          # one file per day, first write kept
    snap = O.load_odds(tmp_path, day)
    assert snap["date"] == "2026-10-08" and snap["quota"] == {"remaining": 498} and snap["games"] == games
    assert O.load_odds(tmp_path, date(2026, 10, 9)) is None

    ctx = O.team_context(day, data_dir=tmp_path)
    assert set(ctx) == {"EDM", "CGY", "MTL", "UTA"}
    edm, cgy = ctx["EDM"], ctx["CGY"]
    assert edm["home"] and not cgy["home"] and edm["opponent"] == "CGY" and cgy["opponent"] == "EDM"
    assert edm["implied_total"] == games[1].implied_home_total and cgy["implied_total"] == games[1].implied_away_total
    assert edm["win_prob"] + cgy["win_prob"] == pytest.approx(1.0) and edm["as_of"] == "2026-10-08"
    # a later day with no archive of its own falls back to the most recent snapshot listing its games
    later = [O.GameOdds.from_dict({**games[1].to_dict(), "game_date": "2026-10-10"})]
    O.archive_odds(later, tmp_path, date(2026, 10, 9))
    assert set(O.team_context(date(2026, 10, 10), data_dir=tmp_path)) == {"EDM", "CGY"}
    assert O.team_context(date(2026, 10, 10), data_dir=tmp_path)["EDM"]["as_of"] == "2026-10-09"
    assert O.team_context(date(2026, 11, 1), data_dir=tmp_path) == {}


# --------------------------------------------------------------------------- CLI

def _cli(monkeypatch, tmp_path, fetch, key=KEY):
    from rich.console import Console

    from fantasy_manager import cli_harness
    settings = SimpleNamespace(fm_data_dir=tmp_path, fm_offline=False, odds_api_key=key, odds_region="us")
    monkeypatch.setattr(cli_harness, "_settings", lambda: settings)
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    monkeypatch.setattr(O.OddsClient, "from_settings", classmethod(
        lambda cls, s, fetch_json=None: cls(s.odds_api_key, fetch_json=fetch, data_dir=s.fm_data_dir, clock=Clock())))
    # online settings: keep the Daily Faceoff snapshot step off the network
    monkeypatch.setattr(cli_harness, "_lines_step", lambda settings, today, client=None: {"day": str(today),
                                                                                          "snapshot": "test"})
    return cli_harness


def test_harness_odds_command(monkeypatch, tmp_path):
    from typer.testing import CliRunner
    fetch = Fetch()
    ch = _cli(monkeypatch, tmp_path, fetch)
    res = CliRunner().invoke(ch.harness_app, ["odds", "--json", "--all"])
    assert res.exit_code == 0, res.output
    out = json.loads(res.output)
    assert out["archive"] == "written" and out["quota"]["remaining"] == 498 and out["source"] == "api"
    assert KEY not in res.output and "books" not in out["games"][0]
    today = date.today()
    assert (tmp_path / "archive" / f"odds-{today.isoformat()}.json").exists()
    res = CliRunner().invoke(ch.harness_app, ["odds", "--all"])          # rerun: served from today's archive
    assert res.exit_code == 0, res.output
    assert len(fetch.calls) == 1 and "already archived today" in res.output and "498 API credits left" in res.output
    assert KEY not in res.output


def test_harness_odds_command_without_key(monkeypatch, tmp_path):
    from typer.testing import CliRunner
    fetch = Fetch()
    ch = _cli(monkeypatch, tmp_path, fetch, key=None)
    res = CliRunner().invoke(ch.harness_app, ["odds"])
    assert res.exit_code == 0 and "ODDS_API_KEY not set" in res.output and fetch.calls == []


def test_daily_runs_the_odds_step(monkeypatch, tmp_path):
    from typer.testing import CliRunner

    from fantasy_manager.harness import realized as R
    fetch = Fetch()
    ch = _cli(monkeypatch, tmp_path, fetch)
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    monkeypatch.setattr(ch, "_daily_league", lambda lg, *a, **k: {"league": lg, "warnings": [], "skipped": "test"})
    res = CliRunner().invoke(ch.harness_app, ["daily", "--league", "espn", "--json"])
    assert res.exit_code == 0, res.output
    odds = json.loads(res.output)["odds"]
    assert odds["games"] == 2
    assert odds["archive"] == "written" and odds["quota"]["remaining"] == 498 and len(fetch.calls) == 1
    res = CliRunner().invoke(ch.harness_app, ["daily", "--league", "espn"])      # idempotent: no second call
    assert res.exit_code == 0, res.output
    assert "odds:" in res.output and "already archived today" in res.output and len(fetch.calls) == 1
