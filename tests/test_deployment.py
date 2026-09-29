"""NHL deployment: per-game reports (providers.nhl), the ledger pull and summaries
(harness.deployment), Player enrichment (providers.deployment_enrich), role-change alerts
(recommend.flags), goalie start share with actual starts / back-to-backs (valuation.schedule)
and archive v3 inputs."""
import json
from datetime import date, timedelta
from pathlib import Path

import pytest

from fantasy_manager.backtest import archive as A
from fantasy_manager.harness.deployment import (b2b_second_night_rate, deployment_summaries, deployment_summary,
                                                goalie_start_counts, pull_deployment, summarize_rows,
                                                team_goalie_counts)
from fantasy_manager.harness.ledger import DEPLOYMENT_TABLES, TABLES, Ledger
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers import nhl
from fantasy_manager.providers.deployment_enrich import enrich_deployment, role_change_alerts
from fantasy_manager.providers.nhl import (CachedNhlFetch, NhlClient, NhlGoalieGame, NhlSkaterDeploymentSeason,
                                           NhlSkaterPpGame, NhlSkaterToiGame, NhlTeamPpGame, game_window_ttl)
from fantasy_manager.recommend.flags import (PP1_FLOOR, ROLE_PP_DELTA, ROLE_TOI_DELTA, recommend_role_alerts,
                                             role_change)
from fantasy_manager.valuation import schedule as S

FIX = Path(__file__).parent / "fixtures" / "nhl"
DAY = date(2026, 4, 1)
SEASON = 20252026


def load(name):
    return json.loads((FIX / name).read_text(encoding="utf-8"))


GAME_ROUTES = {
    ("skater/timeonice", "true"): "deploy_toi_games_2026-04-01.json",
    ("skater/powerplay", "true"): "deploy_pp_games_2026-04-01.json",
    ("goalie/summary", "true"): "deploy_goalie_games_2026-04-01.json",
    ("team/powerplay", "true"): "deploy_team_pp_games_2026-04-01.json",
    ("skater/timeonice", "false"): "deploy_season_timeonice_20252026.json",
    ("skater/powerplay", "false"): "deploy_season_powerplay_20252026.json",
}


class FakeFetch:
    """Serves the 2026-04-01 COL-VAN fixtures for that date, empty reports for any other window."""

    def __init__(self, fail=()):
        self.calls = []
        self.fail = set(fail)

    def __call__(self, url, params=None):
        self.calls.append((url, params))
        path = url.split("/stats/rest/en/")[-1]
        key = (path, str((params or {}).get("isGame")))
        if key[0] in self.fail:
            raise RuntimeError("boom")
        if key not in GAME_ROUTES:
            raise AssertionError(f"unexpected {url} {params}")
        cay = params["cayenneExp"]
        if key[1] == "true" and '"2026-04-01"' not in cay:
            return {"data": [], "total": 0}
        if key[1] == "false" and "seasonId=20252026" not in cay:
            return {"data": [], "total": 0}
        return load(GAME_ROUTES[key])


@pytest.fixture
def client():
    return NhlClient(fetch_json=FakeFetch(), season=SEASON)


# --------------------------------------------------------------------------- provider

def test_skater_toi_games_parse_and_params(client):
    rows = client.skater_toi_games(SEASON, DAY, DAY)
    url, params = client.fetch_json.calls[-1]
    assert url.endswith("/skater/timeonice") and params["isGame"] == "true" and params["limit"] == -1
    assert params["cayenneExp"] == ('seasonId=20252026 and gameTypeId=2 and gameDate>="2026-04-01" '
                                    'and gameDate<="2026-04-01"')
    assert [s["property"] for s in json.loads(params["sort"])] == ["gameDate", "gameId", "playerId"]
    assert len(rows) == 12
    mack = {r.player_id: r for r in rows}[8477492]
    assert isinstance(mack, NhlSkaterToiGame)
    assert (mack.team, mack.opponent, mack.home, mack.date, mack.game_id) == ("COL", "VAN", True, DAY, 2025021188)
    assert (mack.toi, mack.ev_toi, mack.pp_toi, mack.sh_toi, mack.shifts) == (1461, 1141, 320, 0, 21)
    assert mack.name == "Nathan MacKinnon" and mack.position == "C"


def test_pp_goalie_team_games_parse(client):
    pp = {r.player_id: r for r in client.skater_pp_games(SEASON, DAY, DAY)}
    assert isinstance(pp[8477492], NhlSkaterPpGame)
    assert pp[8477492].pp_toi == 320 and pp[8477492].pp_share == pytest.approx(0.888)
    assert pp[8481641].pp_toi == 0 and pp[8481641].pp_share == 0.0
    goalies = {g.player_id: g for g in client.goalie_games(SEASON, DAY, DAY)}
    assert isinstance(goalies[8478406], NhlGoalieGame)
    b, w, lank = goalies[8478406], goalies[8475809], goalies[8480947]
    assert b.started and (b.sa, b.sv, b.ga, b.toi) == (19, 13, 6, 2121)
    assert not w.started and w.decision == "L"                  # relief appearance
    assert lank.started and lank.decision == "W" and lank.team == "VAN" and lank.home is False
    teams = {t.team: t for t in client.team_pp_toi_games(SEASON, DAY, DAY)}
    assert isinstance(teams["COL"], NhlTeamPpGame)
    # the team report has no abbreviation: it comes from the other team's opponentTeamAbbrev
    assert teams["COL"].team_name == "Colorado Avalanche" and teams["COL"].pp_toi == 360
    assert teams["VAN"].pp_toi == 254 and teams["VAN"].pp_opportunities == 3
    _, params = client.fetch_json.calls[-1]
    assert json.loads(params["sort"])[-1]["property"] == "teamId"


def test_game_rows_paginate_on_game_and_player():
    rows = [{"gameDate": "2026-04-01", "gameId": g, "playerId": p} for g in (1, 2) for p in (10, 11)]

    def fetch(url, params=None):
        if params["limit"] == -1:
            return {"data": rows[:3], "total": 4}
        return {"data": rows[params["start"]:params["start"] + 100], "total": 4}

    got = NhlClient(fetch_json=fetch, season=SEASON).skater_toi_games(SEASON, DAY, DAY)
    assert [(r.game_id, r.player_id) for r in got] == [(1, 10), (1, 11), (2, 10), (2, 11)]


def test_skater_deployment_season(client):
    rep = client.skater_deployment_season(SEASON)
    mcd = rep[8478402]
    assert isinstance(mcd, NhlSkaterDeploymentSeason)
    assert mcd.team == "EDM" and mcd.gp == 82
    assert mcd.toi_per_game == pytest.approx(1379.12195) and mcd.pp_share == pytest.approx(0.864)
    assert client.skater_deployment_season(20262027) == {}          # preseason: no rows


def test_game_window_ttl_and_cached_fetch():
    today = date(2026, 9, 29)
    past = {"isGame": "true", "cayenneExp": 'seasonId=20262027 and gameDate>="2026-09-28" and gameDate<="2026-09-28"'}
    now = {"isGame": "true", "cayenneExp": 'seasonId=20262027 and gameDate>="2026-09-29" and gameDate<="2026-09-29"'}
    assert game_window_ttl(past, today) == nhl.TTL_GAME_WINDOW_CLOSED == 30 * 24 * 3600
    assert game_window_ttl(now, today) == nhl.TTL_GAME_WINDOW_OPEN == 12 * 3600
    assert game_window_ttl({"isGame": "false", "cayenneExp": "seasonId=20262027"}, today) is None

    class Cache:
        def __init__(self):
            self.ttls = []

        def get_json(self, url, params=None, ttl=0):
            self.ttls.append(ttl)
            return {}

    cache = Cache()
    f = CachedNhlFetch(cache, today=today)
    f("u", past), f("u", now), f("u", {"isGame": "false"})
    f2 = CachedNhlFetch(cache, today=today, ttl_for=lambda url, params: ("x", 42.0))
    f2("u", None)
    assert cache.ttls == [30 * 24 * 3600, 12 * 3600, nhl.TTL_SEASON_REPORT, 42.0]


# --------------------------------------------------------------------------- ledger pull

def test_pull_deployment_stores_rows_and_is_idempotent(tmp_path, client):
    with Ledger(tmp_path) as led:
        assert set(DEPLOYMENT_TABLES).isdisjoint(TABLES)
        sched = {"COL": [DAY - timedelta(days=1), DAY], "VAN": [DAY]}
        out = pull_deployment(led, client, DAY, schedule=sched)
        assert out["skaters"] == 12 and out["goalies"] == 3 and out["team_pp_source"] == "report"
        again = pull_deployment(led, client, DAY, schedule=sched)
        assert again["skaters"] == 12
        assert led.count("deployment_daily") == 12 and led.count("goalie_starts") == 3
        mack = led.query("SELECT * FROM deployment_daily WHERE nhl_id=8477492")[0]
        assert mack["toi"] == pytest.approx(24.35) and mack["pp_toi"] == pytest.approx(320 / 60, abs=1e-3)
        assert mack["team_pp_toi"] == pytest.approx(6.0) and mack["pp_share"] == pytest.approx(0.8889, abs=1e-4)
        assert mack["shifts"] == 21 and mack["team"] == "COL" and mack["opponent"] == "VAN"
        rossi = led.query("SELECT * FROM deployment_daily WHERE nhl_id=8482079")[0]
        assert rossi["pp_share"] == pytest.approx(201 / 254, abs=1e-4)
        kiv = led.query("SELECT * FROM deployment_daily WHERE nhl_id=8481641")[0]
        assert kiv["pp_share"] == 0.0
        g = {r["nhl_id"]: r for r in led.query("SELECT * FROM goalie_starts")}
        assert g[8478406]["started"] == 1 and g[8478406]["back_to_back"] == 1      # COL played the day before
        assert g[8475809]["started"] == 0 and g[8480947]["back_to_back"] == 0
        assert g[8478406]["toi"] == pytest.approx(2121 / 60, abs=1e-3)
        assert led.query("SELECT * FROM deployment_pulls")[0]["n_skaters"] == 12


def test_pull_without_games_is_a_clean_noop(tmp_path, client):
    with Ledger(tmp_path) as led:
        out = pull_deployment(led, client, date(2026, 9, 29), schedule={"EDM": [date(2026, 9, 29)]})
        assert out == {"start": "2026-09-29", "end": "2026-09-29", "skaters": 0, "goalies": 0, "team_pp_source": None}
        assert led.count("deployment_daily") == 0 and led.count("goalie_starts") == 0
        assert led.query("SELECT n_skaters, n_goalies FROM deployment_pulls") == [{"n_skaters": 0, "n_goalies": 0}]


def test_pull_derives_team_pp_when_team_report_fails(tmp_path):
    c = NhlClient(fetch_json=FakeFetch(fail={"team/powerplay"}), season=SEASON)
    with Ledger(tmp_path) as led:
        out = pull_deployment(led, c, DAY, schedule={"COL": [DAY], "VAN": [DAY]})
        assert out["team_pp_source"] == "derived"
        # the fixture keeps 6 of each team's skaters, so sum/5 covers part of the team's PP time
        col = [r for r in led.query("SELECT * FROM deployment_daily WHERE team='COL'")]
        want = (320 + 278 + 276 + 272 + 168 + 0) / 5 / 60
        assert all(r["team_pp_toi"] == pytest.approx(want, abs=1e-3) for r in col)


class FakeNhl:
    """Synthetic per-game rows keyed by date (for back-to-back and summary tests)."""

    def __init__(self, toi=None, goalies=None, weeks=None):
        self.toi, self.goalies, self.weeks = toi or {}, goalies or {}, weeks
        self.week_calls = 0

    def _span(self, src, start, end):
        return [r for d, rows in src.items() if start <= d <= end for r in rows]

    def skater_toi_games(self, season, start, end):
        return self._span(self.toi, start, end)

    def skater_pp_games(self, season, start, end):
        return []

    def team_pp_toi_games(self, season, start, end):
        return []

    def goalie_games(self, season, start, end):
        return self._span(self.goalies, start, end)

    def week_schedule(self, day):
        self.week_calls += 1
        if self.weeks is None:
            raise RuntimeError("offline")
        from fantasy_manager.providers.nhl import NhlGame, NhlWeekSchedule
        games = [NhlGame(game_id=i, game_type=2, date=day, away=a, home=h)
                 for i, (a, h) in enumerate(self.weeks.get(day, []))]
        return NhlWeekSchedule(days={day: games})


def _toi(pid, d, team, toi, pp, team_pp):
    """One synthetic game: team_pp minutes spread so the team sums to 5 * team_pp."""
    return NhlSkaterToiGame(player_id=pid, date=d, team=team, toi=toi * 60, pp_toi=pp * 60, ev_toi=(toi - pp) * 60,
                            sh_toi=0, shifts=20)


def _goalie(pid, d, team, started=True):
    return NhlGoalieGame(player_id=pid, date=d, team=team, started=started, sa=30, sv=27, ga=3, toi=3600)


def test_back_to_back_from_ledger_and_week_schedule(tmp_path):
    d1, d2 = date(2026, 10, 10), date(2026, 10, 11)
    fake = FakeNhl(goalies={d1: [_goalie(1, d1, "AAA")], d2: [_goalie(1, d2, "AAA"), _goalie(2, d2, "BBB")]},
                   weeks={d1 - timedelta(days=1): [("AAA", "CCC")]})
    with Ledger(tmp_path) as led:
        pull_deployment(led, fake, d1)                     # previous day from the NHL week schedule
        assert led.query("SELECT back_to_back FROM goalie_starts WHERE game_date=?", (d1.isoformat(),)) == [
            {"back_to_back": 1}]
        calls = fake.week_calls
        pull_deployment(led, fake, d2)                     # previous day is in the ledger: no request
        assert fake.week_calls == calls
        b2b = {r["nhl_id"]: r["back_to_back"] for r in led.query(
            "SELECT nhl_id, back_to_back FROM goalie_starts WHERE game_date=?", (d2.isoformat(),))}
        assert b2b == {1: 1, 2: 0}
        # a range pull decides within the range from its own rows; offline + unknown -> None
        led2 = Ledger(tmp_path / "other")
        off = FakeNhl(goalies=fake.goalies)
        pull_deployment(led2, off, d1, d2)
        rows = {(r["nhl_id"], r["game_date"]): r["back_to_back"] for r in led2.query("SELECT * FROM goalie_starts")}
        assert rows == {(1, d1.isoformat()): None, (1, d2.isoformat()): 1, (2, d2.isoformat()): 0}
        led2.close()


# --------------------------------------------------------------------------- summaries

def _rows(tois, pps=None, team_pp=4.0, start=date(2026, 10, 8)):
    pps = pps or [1.0] * len(tois)
    return [{"game_date": (start + timedelta(days=2 * i)).isoformat(), "toi": t, "pp_toi": p,
             "team_pp_toi": team_pp} for i, (t, p) in enumerate(zip(tois, pps))]


def test_summary_math_with_season_baseline():
    rows = _rows([15, 15, 16, 14, 15, 18, 18, 17, 19, 18], [0.4, 0.4, 0.4, 0.4, 0.4, 2.4, 2.4, 2.4, 2.4, 2.4])
    s = summarize_rows(rows, last_n=5)
    assert s["gp"] == 10 and s["baseline_source"] == "season"
    assert s["toi_avg"] == pytest.approx(18.0) and s["baseline_toi"] == pytest.approx(15.0)
    assert s["toi_trend"] == pytest.approx(3.0)
    assert s["pp_share_avg"] == pytest.approx(0.6) and s["baseline_pp_share"] == pytest.approx(0.1)
    assert s["pp_share_trend"] == pytest.approx(0.5)
    assert s["season_toi"] == pytest.approx(16.5) and s["season_pp_share"] == pytest.approx(0.35)
    assert s["pp_toi_avg"] == pytest.approx(2.4) and s["season_pp_toi"] == pytest.approx(1.4)


def test_summary_prior_baseline_and_short_windows():
    rows = _rows([20, 20, 21, 19, 20], [2.0] * 5)
    s = summarize_rows(rows, 5, prior={"toi": 17.0, "pp_share": 0.2})
    assert s["baseline_source"] == "prior" and s["toi_trend"] == pytest.approx(3.0)
    assert s["pp_share_trend"] == pytest.approx(0.3)
    assert summarize_rows(rows, 5)["toi_trend"] is None                # no baseline at all
    short = summarize_rows(rows[:3], 5, prior={"toi": 17.0, "pp_share": 0.2})
    assert short["gp"] == 3 and short["toi_trend"] is None and short["toi_avg"] == pytest.approx(20.333, abs=1e-3)
    empty = summarize_rows([], 5)
    assert empty["gp"] == 0 and empty["toi_avg"] is None
    # a game without a team power play does not dilute the PP share
    rows[0]["team_pp_toi"] = 0.0
    assert summarize_rows(rows, 5)["pp_share_avg"] == pytest.approx(0.5)


def test_deployment_summary_from_ledger(tmp_path):
    start = date(2026, 10, 7)
    toi = {}
    for i in range(12):
        d = start + timedelta(days=i)
        own = 0.5 if i < 7 else 1.5
        # team PP = sum / 5 = (own + 4 * 2.0 + (2.0 - own)) / 5 = 2.0 minutes every game
        toi[d] = [_toi(7, d, "AAA", 14.0 if i < 7 else 17.0, own, 4.0), _toi(99, d, "AAA", 15.0, 2.0 - own, 4.0)] + [
            _toi(100 + k, d, "AAA", 15.0, 2.0, 4.0) for k in range(4)]
    with Ledger(tmp_path) as led:
        pull_deployment(led, FakeNhl(toi=toi), start, start + timedelta(days=11), schedule={"AAA": []})
        s = deployment_summary(led, 7, start + timedelta(days=11), last_n=5)
        assert s["gp"] == 12 and s["toi_avg"] == pytest.approx(17.0) and s["baseline_toi"] == pytest.approx(14.0)
        assert s["toi_trend"] == pytest.approx(3.0)
        assert s["pp_share_avg"] == pytest.approx(0.75) and s["baseline_pp_share"] == pytest.approx(0.25)
        assert s["pp_share_trend"] == pytest.approx(0.5) and s["season_pp_share"] == pytest.approx(
            (7 * 0.5 + 5 * 1.5) / 24, abs=1e-3)
        # as_of cuts the season: only games up to that date count
        assert deployment_summary(led, 7, start + timedelta(days=3))["gp"] == 4
        # last season's games are outside this season
        assert deployment_summary(led, 7, date(2027, 9, 15))["gp"] == 0
        many = deployment_summaries(led, [7, 100, 999], start + timedelta(days=11), last_n=10)
        assert set(many) == {7, 100} and many[7]["last_n"] == 10


# --------------------------------------------------------------------------- enrichment

def _player(cid, nhl_id, name=None, positions=("C",), **kw):
    return Player(cid=cid, name=name or cid, name_norm=cid, ids={"nhl": str(nhl_id)} if nhl_id else {},
                  team="EDM", positions=list(positions), **kw)


def _ctx(mine=(), theirs=(), fas=(), as_of=date(2026, 9, 29)):
    def team(tid, name, me, ps):
        return FantasyTeam(team_id=tid, name=name, owner_is_me=me,
                           slots=[RosterSlot(slot="C", player=p, starting=True) for p in ps])
    return LeagueContext(provider="fantrax", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 2},
                         teams=[team("1", "Mine", True, list(mine)), team("2", "Rivals", False, list(theirs))],
                         free_agents=list(fas), matchup_period=1, as_of=as_of)


def test_enrich_preseason_falls_back_to_prior_season(tmp_path, client):
    mcd, leon, nobody = _player("f:1", 8478402), _player("f:2", 8477934), _player("f:3", 1)
    g = _player("f:4", 8478406, positions=("G",))
    ctx = _ctx([mcd, g], [leon], [nobody])
    with Ledger(tmp_path) as led:
        out = enrich_deployment(ctx, led, date(2026, 9, 29), nhl=client)
    assert out["prior_season"] == 2 and out["missing"] == 1 and out["ledger"] == 0 and out["skaters"] == 3
    assert mcd.toi_per_game == pytest.approx(1379.12195 / 60, abs=1e-3)
    assert mcd.pp_toi_per_game == pytest.approx(216.4756 / 60, abs=1e-3) and mcd.pp_share == pytest.approx(0.864)
    assert mcd.toi_trend is None and mcd.pp_share_trend is None
    assert leon.pp_share == pytest.approx(0.854)
    assert nobody.toi_per_game is None and g.toi_per_game is None
    assert role_change_alerts(ctx) == []


def test_enrich_from_ledger_with_prior_baseline(tmp_path, client):
    """5 games this season: the trend is measured against last season's averages."""
    start = date(2026, 10, 7)
    toi = {start + timedelta(days=i): [_toi(8478402, start + timedelta(days=i), "EDM", 25.0, 4.5, 5.0)]
           + [_toi(200 + k, start + timedelta(days=i), "EDM", 15.0, 5.0, 5.0) for k in range(4)] for i in range(5)}
    mcd = _player("f:1", 8478402)
    ctx = _ctx([mcd], as_of=start + timedelta(days=4))
    with Ledger(tmp_path) as led:
        pull_deployment(led, FakeNhl(toi=toi), start, start + timedelta(days=4), schedule={"EDM": []})
        out = enrich_deployment(ctx, led, nhl=client)
    assert out["ledger"] == 1 and out["trends"] == 1
    assert mcd.toi_per_game == pytest.approx(25.0)
    team_pp = (4.5 + 20.0) / 5
    assert mcd.pp_share == pytest.approx(4.5 / team_pp, abs=1e-3)
    assert mcd.toi_trend == pytest.approx(25.0 - 1379.12195 / 60, abs=1e-2)
    assert mcd.pp_share_trend == pytest.approx(4.5 / team_pp - 0.864, abs=1e-3)
    d = out["details"]["f:1"]
    assert d["baseline_source"] == "prior" and d["gp"] == 5


def test_enrich_prefers_season_report_when_ledger_missed_days(tmp_path):
    class Rep:
        def skater_deployment_season(self, season):
            return {5: NhlSkaterDeploymentSeason(player_id=5, season=season, gp=30, toi_per_game=1200.0,
                                                 pp_toi_per_game=120.0, pp_share=0.5)}

    d = date(2026, 12, 1)
    toi = {d: [_toi(5, d, "EDM", 22.0, 3.0, 4.0)]}
    p = _player("f:5", 5)
    ctx = _ctx([p], as_of=d)
    with Ledger(tmp_path) as led:
        pull_deployment(led, FakeNhl(toi=toi), d, schedule={"EDM": []})
        out = enrich_deployment(ctx, led, nhl=Rep())
    assert out["season_report"] == 1 and p.toi_per_game == pytest.approx(20.0) and p.pp_share == 0.5


def test_enrich_warns_when_reports_fail(tmp_path):
    class Broken:
        def skater_deployment_season(self, season):
            raise RuntimeError("down")

    p = _player("f:1", 8478402)
    ctx = _ctx([p])
    with Ledger(tmp_path) as led:
        out = enrich_deployment(ctx, led, nhl=Broken())
    assert out["missing"] == 1 and any("Deployment" in w for w in ctx.warnings)


# --------------------------------------------------------------------------- role alerts

def _trend_player(cid, toi=16.0, toi_trend=0.0, pp=0.3, pp_trend=0.0, **kw):
    return _player(cid, 1, toi_per_game=toi, toi_trend=toi_trend, pp_share=pp, pp_share_trend=pp_trend, **kw)


def test_alert_thresholds_are_monotone_upwards():
    fa = [_trend_player(f"t{x}", toi_trend=x) for x in (1.4, 1.5, 2.0, 3.0, 5.0)]
    ctx = _ctx(fas=fa)
    recs = {r.subjects[0].cid: r for r in recommend_role_alerts(ctx)}
    assert "t1.4" not in recs and set(recs) == {"t1.5", "t2.0", "t3.0", "t5.0"}
    s = [recs[c].strength for c in ("t1.5", "t2.0", "t3.0", "t5.0")]
    assert s == sorted(s) and s[0] == pytest.approx(4.0) and s[-1] == pytest.approx(10.0)
    r = recs["t2.0"]
    assert r.kind == "alert" and r.counterparty == "FA" and r.subjects[0].cid == "t2.0"
    assert r.reasons[0].code == "ROLE_TOI" and r.reasons[0].value == pytest.approx(18.0)
    assert r.reasons[0].baseline == pytest.approx(16.0) and "+2.0" in r.reasons[0].text
    assert r.predicted_gain is None and r.rank_in_kind is not None

    pp = [_trend_player(f"p{x}", pp=0.3, pp_trend=x) for x in (0.14, 0.15, 0.25, 0.4)]
    recs = {r.subjects[0].cid: r for r in recommend_role_alerts(_ctx(fas=pp))}
    assert set(recs) == {"p0.15", "p0.25", "p0.4"}
    s = [recs[c].strength for c in ("p0.15", "p0.25", "p0.4")]
    assert s == sorted(s) and recs["p0.15"].reasons[0].code == "ROLE_PP"
    assert ROLE_TOI_DELTA == 1.5 and ROLE_PP_DELTA == 0.15


def test_pp1_promotion_by_usage_and_both_signals():
    # 0.1 -> 0.55 share: PP1 promotion floors the strength at 7
    promo = _trend_player("promo", pp=0.1, pp_trend=0.45)
    c = role_change(promo)
    assert c["pp1"] and c["strength"] >= PP1_FLOOR
    both = _trend_player("both", toi_trend=2.0, pp=0.1, pp_trend=0.45)
    assert role_change(both)["strength"] == pytest.approx(min(10.0, role_change(promo)["strength"] + 1.0))
    # exact numbers from a deployment summary: baseline 0.15 -> recent 0.6
    d = {"toi_avg": 17.0, "baseline_toi": 16.5, "toi_trend": 0.5, "pp_share_avg": 0.6, "baseline_pp_share": 0.15,
         "pp_share_trend": 0.45, "last_n": 5}
    ctx = _ctx(theirs=[_trend_player("x", pp_trend=0.45)])
    r = recommend_role_alerts(ctx, details={"x": d})[0]
    assert r.counterparty == "Rivals" and "PP1" in r.title
    assert "60%" in r.reasons[0].text and "15%" in r.reasons[0].text and "last 5 GP" in r.reasons[0].text


def test_role_loss_only_for_my_players_and_monotone_downwards():
    mine = [_trend_player(f"m{x}", toi_trend=-x) for x in (1.4, 1.5, 3.0)]
    theirs = [_trend_player("rival_down", toi_trend=-3.0)]
    fas = [_trend_player("fa_down", pp=0.6, pp_trend=-0.5)]
    recs = {r.subjects[0].cid: r for r in recommend_role_alerts(_ctx(mine, theirs, fas))}
    assert set(recs) == {"m1.5", "m3.0"}
    assert recs["m3.0"].strength > recs["m1.5"].strength
    assert recs["m1.5"].title.startswith("Role loss") and recs["m1.5"].counterparty == "Mine"
    assert recs["m3.0"].reasons[0].value == pytest.approx(13.0)
    # PP1 lost (0.6 -> 0.1) on my team
    lost = _trend_player("lost", pp=0.6, pp_trend=-0.5)
    r = recommend_role_alerts(_ctx([lost]))[0]
    assert "off PP1" in r.title and r.strength >= PP1_FLOOR
    # goalies and players without trends never alert
    g = _trend_player("g", toi_trend=5.0, positions=("G",))
    assert recommend_role_alerts(_ctx([g, _player("none", 3)])) == []


# --------------------------------------------------------------------------- goalie starts / back-to-backs

def _gl(gs, gp, split="prior"):
    return Player(cid="g", name="g", name_norm="g", ids={}, team="EDM", positions=["G"],
                  lines={split: StatLine(split=split, gp=gp, stats={"GS": gs, "GP": gp})})


def test_start_share_with_actual_starts():
    g = _gl(60, 60)
    base = S.start_share(g)                                   # (60 + 5) / 70
    assert base == pytest.approx(65 / 70)
    # 3 starts in 10 team games, shrunk k=10 toward the prior share
    assert S.start_share(g, actual=(3, 10)) == pytest.approx((3 + 10 * base) / 20)
    assert S.start_share(g, actual=(0, 0)) == pytest.approx(base)       # no games yet: unchanged
    assert S.actual_start_share(0.5, 12, 10) == pytest.approx((10 + 5) / 20)  # capped at team games
    # the season line is not double counted when actual starts are given
    g.lines["season"] = StatLine(split="season", gp=10, stats={"GS": 3, "GP": 10})
    assert S.start_share(g, actual=(3, 10)) == pytest.approx((3 + 10 * base) / 20)
    # more actual starts -> higher share (monotone)
    assert S.start_share(g, actual=(2, 10)) < S.start_share(g, actual=(5, 10)) < S.start_share(g, actual=(9, 10))
    skater = Player(cid="s", name="s", name_norm="s", ids={}, team="EDM", positions=["C"])
    assert S.start_share(skater, actual=(0, 10)) == 1.0


def test_week_start_share_back_to_backs():
    d = date(2026, 10, 12)
    days = [d, d + timedelta(days=2), d + timedelta(days=3), d + timedelta(days=5)]   # one back-to-back
    assert S.second_nights(days, d) == 1
    assert S.second_nights([d - timedelta(days=1), d], d) == 1          # previous game before the window
    share = 0.7
    p1 = (share - 0.15 * 0.35) / 0.85
    assert S.week_start_share(share, days, d) == pytest.approx((3 * p1 + 0.35) / 4)
    assert S.week_start_share(share, days, d) < share
    no_b2b = [d, d + timedelta(days=2), d + timedelta(days=4)]
    assert S.week_start_share(share, no_b2b, d) == pytest.approx(p1) and p1 > share
    backup = S.week_start_share(0.3, days, d)
    assert backup > S.week_start_share(0.3, no_b2b, d)                   # backups get the second nights
    assert S.week_start_share(0.1, days, d) == 0.1                       # third goalie unchanged
    assert S.week_start_share(0.7, days, d, b2b_p=0.6) > S.week_start_share(0.7, days, d)
    # schedule_factor: opt-in only
    g = _gl(50, 60)
    g.lines["season"] = StatLine(split="season", gp=0, stats={})
    sched = {"EDM": days}
    w = S.week_window(d, d, sched, {})
    assert S.schedule_factor(g, sched, {}, w, share=0.7).start_share == 0.7
    assert S.schedule_factor(g, sched, {}, w, share=0.7, b2b_p=0.35).start_share == pytest.approx(
        S.week_start_share(0.7, days, d))


def test_b2b_rate_and_start_counts_from_ledger(tmp_path):
    start = date(2026, 10, 7)
    goalies = {}
    team_dates = []
    # AAA plays pairs of consecutive days (6 back-to-backs); #1 (id 1) starts every first night and
    # 2 of the 6 second nights; #2 (id 2) the other 4
    for k in range(6):
        d1 = start + timedelta(days=3 * k)
        d2 = d1 + timedelta(days=1)
        team_dates += [d1, d2]
        goalies[d1] = [_goalie(1, d1, "AAA")]
        goalies[d2] = [_goalie(1 if k < 2 else 2, d2, "AAA")]
    as_of = team_dates[-1]
    with Ledger(tmp_path) as led:
        pull_deployment(led, FakeNhl(goalies=goalies), start, as_of, schedule={"AAA": team_dates})
        counts = team_goalie_counts(led, as_of)
        assert counts["AAA"]["games"] == 12 and counts["AAA"]["starts"] == {1: 8, 2: 4}
        assert counts["AAA"]["starter"] == 1 and counts["AAA"]["b2b"] == 6 and counts["AAA"]["b2b_starter"] == 2
        assert goalie_start_counts(led, 1, as_of) == (8, 12)
        assert goalie_start_counts(led, 3, as_of, team="AAA") == (0, 12)
        assert goalie_start_counts(led, 3, as_of) == (0, 0)
        p, n = b2b_second_night_rate(led, "AAA", as_of)
        assert n == 6 and p == pytest.approx((2 + 5 * 0.35) / (6 + 5))
        # fewer than 5 back-to-backs: the 0.35 default
        early = team_dates[7]
        assert b2b_second_night_rate(led, "AAA", early) == (0.35, 4)
        assert b2b_second_night_rate(led, "ZZZ", as_of) == (0.35, 0)
        g = _gl(60, 60)
        assert S.start_share(g, actual=goalie_start_counts(led, 1, as_of)) == pytest.approx(
            (8 + 10 * 65 / 70) / 22)


# --------------------------------------------------------------------------- archive v3

def test_archive_v3_round_trip_and_v2_read(tmp_path):
    p = _player("espn:9", 8478402, pct_owned=90.0, toi_per_game=21.5, pp_toi_per_game=3.25, pp_share=0.61,
                toi_trend=1.8, pp_share_trend=0.2, pct_owned_change=-1.5, adp=12.0, line="f1", pp_unit="pp1",
                confirmed_start=False, ixg_per_game=0.41, onice_xg_pct=55.1)
    p.lines["projected"] = StatLine(split="projected", gp=80, stats={"G": 40.0, "GP": 80})
    ctx = _ctx([p], as_of=date(2026, 10, 20))
    path, status = A.archive_projections(ctx, tmp_path)
    assert status == "created"
    snap = A.load_snapshot(path)
    assert snap["version"] == A.ARCHIVE_VERSION == 3
    inp = snap["players"][0]["inputs"]
    assert inp["toi_per_game"] == 21.5 and inp["pp_share"] == 0.61 and inp["toi_trend"] == 1.8
    assert inp["pct_owned_change"] == -1.5 and inp["adp"] == 12.0 and inp["line"] == "f1"
    assert inp["pp_unit"] == "pp1" and inp["confirmed_start"] is False and inp["onice_xg_pct"] == 55.1
    assert "pk_unit" not in inp and "goals_minus_ixg" not in inp          # only fields that are set
    sig = A.signal_inputs(inp)
    assert set(sig) == set(A.SIGNAL_FIELDS) and sig["pk_unit"] is None and sig["ixg_per_game"] == 0.41

    v2 = {"version": 2, "provider": "espn", "as_of": "2026-10-19", "scoring": {"kind": "points"},
          "players": [{"cid": "espn:9", "name": "x", "nhl_id": 8478402, "projected": None, "fm": None,
                       "inputs": {"status": "healthy", "pct_owned": 90.0}}]}
    f = tmp_path / "archive" / "projections-espn-2026-10-19.json"
    f.write_text(json.dumps(v2), encoding="utf-8")
    old = A.load_snapshot(f)
    assert old["version"] == 2 and old["players"][0]["inputs"] == {"status": "healthy", "pct_owned": 90.0}
    assert all(v is None for v in A.signal_inputs(old["players"][0]["inputs"]).values())
    assert all(v is None for v in A.signal_inputs(None).values())
