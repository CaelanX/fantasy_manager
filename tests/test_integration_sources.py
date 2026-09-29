"""Integration of the new data sources: enrich steps 8-10 (deployment, Daily Faceoff lines,
MoneyPuck xG), the in-season xG goal shrink / actual starts / back-to-backs in valuation, the
xG path of the sell-high / buy-low flags, the daily deployment pull, the new CLI commands, the
dashboard strips / badges and the digest alerts section."""
import json
from datetime import date, datetime, timedelta, timezone

import pytest
from rich.console import Console
from typer.testing import CliRunner

from fantasy_manager import cli
from fantasy_manager.harness.deployment import pull_deployment
from fantasy_manager.harness.ledger import Ledger
from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot,
                                    ScoringConfig, StatLine)
from fantasy_manager.providers import espn
from fantasy_manager.providers.dailyfaceoff import GoalieStart, LinePlayer, TeamLines
from fantasy_manager.providers.enrich import enrich_context
from fantasy_manager.providers.moneypuck import CREDIT, MpSkater
from fantasy_manager.providers.nhl import NhlGoalieGame, NhlSkaterToiGame
from fantasy_manager.recommend.flags import recommend_flags
from fantasy_manager.report.digest import build_digest
from fantasy_manager.scoring import from_config
from fantasy_manager.valuation.valuate import valuate_league

AS_OF = date(2026, 10, 20)
MCDAVID, SKINNER, PICKARD = 8478402, 8479973, 8475717


def pl(cid, name, team, pos, nhl=None, lines=None, **kw):
    return Player(cid=cid, name=name, name_norm=name.lower(), ids={"nhl": str(nhl)} if nhl else {}, team=team,
                  positions=list(pos), lines=lines or {}, **kw)


def line(split, gp, **stats):
    return StatLine(split=split, gp=gp, stats={"GP": float(gp), **{k: float(v) for k, v in stats.items()}})


def ctx_with(mine, fas=(), as_of=AS_OF, provider="test", weights=None):
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in mine]
    return LeagueContext(provider=provider, league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights=weights or {"G": 3.0, "A": 2.0, "SOG": 0.5}),
                         roster_shape={"C": 2, "G": 1, "BN": 5},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=list(fas), matchup_period=1, as_of=as_of)


class S:
    def __init__(self, d):
        self.fm_data_dir = d


class DeadNhl:
    """Every NHL call fails: the enrich steps 1-7 degrade to warnings."""

    def __getattr__(self, name):
        def fail(*a, **k):
            raise RuntimeError("offline")
        return fail


class FakeDfo:
    def __init__(self):
        self.warnings: list[str] = []
        self.calls = 0

    def all_lines(self, teams=None):
        self.calls += 1
        return {"EDM": TeamLines(team="EDM", source="beat writer", players=[
            LinePlayer(name="Connor McDavid", dfo_id=11, position="C", group="f1", groups=["f1", "pp1"], line="f1",
                       pp_unit="pp1"),
            LinePlayer(name="Stuart Skinner", dfo_id=31, position="G", group="g", groups=["g"], line="g",
                       goalie_depth=1)])}

    def starting_goalies(self, day):
        return [GoalieStart(game="EDM@CGY", team="EDM", opponent="CGY", goalie_name="Stuart Skinner", dfo_id=31,
                            strength="Confirmed", source="Daily Faceoff",
                            created_at=datetime(2026, 10, 20, 15, tzinfo=timezone.utc))]


class FakeMp:
    """MoneyPuck rows for last season only (this season's file is empty before games)."""

    def skaters(self, year, situation="all", with_pp=False):
        if year != 2025:
            return []
        base = dict(nhl_id=MCDAVID, season=2025, name="Connor McDavid", team="EDM", position="C", gp=80,
                    toi_min=1700.0, ixg=40.0, goals=48.0, sog=300.0, ixg_adj=40.0)
        return [MpSkater(situation="all", **base),
                MpSkater(situation="5on5", onice_sh_pct=0.11, onice_xg_pct=0.6, **base)]


def enrich_ctx():
    mcd = pl("c1", "Connor McDavid", "EDM", ["C"], MCDAVID, {"prior": line("prior", 80, G=48, A=80, SOG=300)})
    skinner = pl("g1", "Stuart Skinner", "EDM", ["G"], SKINNER, {"prior": line("prior", 60, GS=58, W=30)})
    pickard = pl("g2", "Calvin Pickard", "EDM", ["G"], PICKARD)
    return ctx_with([mcd, skinner, pickard])


def goalie_ledger(path, games=6, skinner_starts=4):
    led = Ledger(path)
    rows = []
    start = date(2026, 10, 8)
    for i in range(games):
        d = (start + timedelta(days=2 * i)).isoformat()
        me = i < skinner_starts
        rows += [{"nhl_id": SKINNER, "game_date": d, "team": "EDM", "started": int(me), "back_to_back": 0},
                 {"nhl_id": PICKARD, "game_date": d, "team": "EDM", "started": int(not me), "back_to_back": 0}]
    led.upsert("goalie_starts", rows, ("nhl_id", "game_date"))
    return led


# --------------------------------------------------------------------------- enrich steps 8-10

def test_enrich_new_steps_off(tmp_path):
    ctx = enrich_ctx()
    enrich_context(ctx, S(tmp_path), None, nhl=DeadNhl(), injuries_fetch=DeadNhl().x, teams=["EDM"],
                   deployment=False, lines=False, xg=False, lines_client=FakeDfo(), xg_client=FakeMp())
    notes = " ".join(ctx.source_notes)
    assert "Deployment" not in notes and "Daily Faceoff" not in notes and "MoneyPuck" not in notes
    p = ctx.my_team.players[0]
    assert p.line is None and p.ixg_per_game is None and p.toi_per_game is None
    assert not (tmp_path / "harness.db").exists()


def test_enrich_new_steps_on_with_fakes(tmp_path):
    ctx = enrich_ctx()
    led = goalie_ledger(tmp_path / "led")
    dfo = FakeDfo()
    try:
        enrich_context(ctx, S(tmp_path), None, nhl=DeadNhl(), injuries_fetch=DeadNhl().x, teams=["EDM"],
                       ledger=led, lines_client=dfo, xg_client=FakeMp())
    finally:
        led.close()
    by = {p.cid: p for p in ctx.all_players()}
    assert by["c1"].line == "f1" and by["c1"].pp_unit == "pp1"
    assert by["g1"].confirmed_start is True and by["g2"].confirmed_start is False
    assert "Confirmed" in by["g1"].start_source
    assert by["c1"].ixg_per_game == pytest.approx(0.5) and by["c1"].xg_split == "prior"
    assert ctx.goalie_actual_starts == {"g1": (4, 6), "g2": (2, 6)}
    assert ctx.b2b_second_night["EDM"] == (0.35, 0)
    notes = " ".join(ctx.source_notes)
    assert "Daily Faceoff: lines for 1 teams" in notes and CREDIT in notes and "Deployment" in notes
    assert (tmp_path / "lines" / f"lines-{AS_OF.isoformat()}.json").exists()   # daily snapshot
    assert any("Deployment (current season report) unavailable" in w for w in ctx.warnings)


def test_enrich_network_steps_need_a_cache(tmp_path):
    ctx = enrich_ctx()
    enrich_context(ctx, S(tmp_path), None, nhl=DeadNhl(), injuries_fetch=DeadNhl().x, teams=["EDM"])
    notes = " ".join(ctx.source_notes)
    assert "Daily Faceoff lines skipped (no HTTP cache)" in notes
    assert "MoneyPuck expected goals skipped (no HTTP cache)" in notes
    assert "no harness ledger" in notes and not (tmp_path / "harness.db").exists()


# --------------------------------------------------------------------------- valuation

def _skater(gp, split="season", ixg=0.3, **kw):
    lines = {"prior": line("prior", 82, G=20, A=30, SOG=200)}
    if gp:
        lines["season"] = line("season", gp, G=0.8 * gp, A=0.5 * gp, SOG=3 * gp)
    return pl("s", "Shooter", "EDM", ["C"], 1, lines, ixg_per_game=ixg, goals_minus_ixg=0.5 * max(gp, 1),
              xg_split=split, **kw)


def _value(p, **ctx_kw):
    ctx = ctx_with([p], **ctx_kw)
    return valuate_league(ctx, from_config(ctx.scoring))[p.cid]


def test_xg_shrink_only_in_season_with_five_gp():
    shrunk = _value(_skater(10))
    plain = _value(_skater(10, ixg=None))
    assert "XG_SHRINK" in {r.code for r in shrunk.reasons}
    xr = next(r for r in shrunk.reasons if r.code == "XG_SHRINK")
    assert xr.value == pytest.approx(0.8) and xr.baseline == pytest.approx(0.3) and "MoneyPuck" in xr.text
    assert shrunk.rates["G"] < plain.rates["G"]
    for p in (_skater(4), _skater(10, split="prior"), _skater(0)):       # < 5 GP, last season's xG, preseason
        v = _value(p)
        assert "XG_SHRINK" not in {r.code for r in v.reasons}
    # never the prior-season baseline: with no season line the rates equal the no-xG rates
    assert _value(_skater(0)).rates == _value(_skater(0, ixg=None)).rates


def test_actual_starts_and_back_to_backs_are_opt_in():
    g = pl("g", "Starter", "EDM", ["G"], 2, {"prior": line("prior", 60, GS=60, W=30, SV=1500, GA=150, SA=1650)})
    sched = {"EDM": [AS_OF, AS_OF + timedelta(days=1), AS_OF + timedelta(days=4)],
             "CGY": [AS_OF, AS_OF + timedelta(days=2)]}
    gpd = {d: 1 for ds in sched.values() for d in ds}
    base = ctx_with([g])
    base.schedule, base.games_per_day = sched, gpd
    plain = valuate_league(base, from_config(base.scoring))["g"]
    codes = {r.code for r in plain.reasons}
    assert "START_ACTUAL" not in codes and "B2B" not in codes and plain.share_source == "history"

    ctx = base.model_copy(update={"goalie_actual_starts": {"g": (3, 10)}, "b2b_second_night": {"EDM": (0.2, 6)}})
    v = valuate_league(ctx, from_config(ctx.scoring))["g"]
    codes = {r.code for r in v.reasons}
    assert "START_ACTUAL" in codes and "B2B" in codes and v.share_source == "actual"
    assert v.start_share < plain.start_share            # 3 starts in 10 games pulls the share down
    b2b = next(r for r in v.reasons if r.code == "B2B")
    assert b2b.value < b2b.baseline                     # the #1 rarely starts the second night
    # fewer than 5 ledger games: ignored
    few = base.model_copy(update={"goalie_actual_starts": {"g": (3, 4)}})
    assert "START_ACTUAL" not in {r.code for r in valuate_league(few, from_config(few.scoring))["g"].reasons}


# --------------------------------------------------------------------------- flags

def _hot(cid, gmx):
    lines = {"prior": line("prior", 82, G=20, A=30, SOG=200),
             "season": line("season", 20, G=12, A=8, SOG=60),
             "last15": line("last15", 6, G=6, A=3, SOG=18)}
    return pl(cid, cid, "EDM", ["C"], 3, lines, ixg_per_game=(12 - gmx) / 20, goals_minus_ixg=gmx,
              xg_split="season")


def test_sell_high_uses_xg_luck_instead_of_shooting_pct():
    lucky, earned = _hot("lucky", 6.0), _hot("earned", 0.5)
    no_xg = _hot("noxg", 0.0).model_copy(update={"ixg_per_game": None, "goals_minus_ixg": None})
    ctx = ctx_with([lucky, earned, no_xg])
    recs = {r.drop[0].cid: r for r in recommend_flags(ctx, {}) if r.kind == "sell_high"}
    assert "lucky" in recs and "earned" not in recs      # the L15 shooting% spike alone no longer flags him
    codes = {x.code for x in recs["lucky"].reasons}
    assert "XG_LUCK" in codes and "SHOOTING_PCT" not in codes
    xr = next(x for x in recs["lucky"].reasons if x.code == "XG_LUCK")
    assert xr.value == pytest.approx(6.0 / 20 * 3.0) and "MoneyPuck.com" in xr.text
    assert "SHOOTING_PCT" in {x.code for x in recs["noxg"].reasons}   # fallback heuristic unchanged


# --------------------------------------------------------------------------- daily deployment pull

class GameNhl:
    def __init__(self, games):
        self.games = games

    def skater_toi_games(self, season, start, end):
        return [g for g in self.games if isinstance(g, NhlSkaterToiGame) and start <= g.date <= end]

    def skater_pp_games(self, season, start, end):
        return []

    def team_pp_toi_games(self, season, start, end):
        return []

    def goalie_games(self, season, start, end):
        return [g for g in self.games if isinstance(g, NhlGoalieGame) and start <= g.date <= end]


def test_daily_pull_marks_only_complete_days(tmp_path):
    d0, d1 = date(2026, 9, 28), date(2026, 10, 8)
    sched = {"EDM": [d1], "CGY": [d1]}
    with Ledger(tmp_path) as led:
        out = pull_deployment(led, GameNhl([]), d0, schedule=sched, record_empty=False)
        assert out["not_final"] == [] and led.count("deployment_pulls") == 1     # schedule-confirmed off day
        out = pull_deployment(led, GameNhl([]), d1, schedule=sched, record_empty=False)
        assert out["not_final"] == [d1.isoformat()] and led.count("deployment_pulls") == 1
        partial = [NhlGoalieGame(player_id=1, date=d1, team="EDM", started=True, sa=30, sv=28, ga=2, toi=3600)]
        out = pull_deployment(led, GameNhl(partial), d1, schedule=sched, record_empty=False)
        assert out["not_final"] == [d1.isoformat()] and led.count("goalie_starts") == 1   # rows kept
        full = partial + [NhlGoalieGame(player_id=2, date=d1, team="CGY", started=True, sa=30, sv=27, ga=3,
                                        toi=3600)]
        out = pull_deployment(led, GameNhl(full), d1, schedule=sched, record_empty=False)
        assert out["not_final"] == [] and led.count("deployment_pulls") == 2


def test_harness_daily_runs_deployment_and_lines(monkeypatch, tmp_path):
    from types import SimpleNamespace

    from fantasy_manager import cli_harness
    from fantasy_manager.harness import realized as R

    monkeypatch.setattr(cli_harness, "_settings", lambda: SimpleNamespace(fm_data_dir=tmp_path, fm_offline=True))
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    sched = {"EDM": [date.today() + timedelta(days=10)]}
    monkeypatch.setattr(cli_harness, "_daily_league",
                        lambda lg, *a, **k: {"league": lg, "warnings": [], "skipped": "test", "_schedule": sched})
    seen = {}

    def fake_lines(settings, today, client=None):
        seen["lines"] = today
        return {"day": today.isoformat(), "snapshot": "taken", "teams": 32, "starts": 4}
    monkeypatch.setattr(cli_harness, "_lines_step", fake_lines)
    monkeypatch.setattr(cli_harness, "_deployment_step",
                        lambda ledger, settings, day, schedule, client=None: {
                            "day": day.isoformat(), "skaters": 0, "goalies": 0, "not_final": [],
                            "scheduled": sum(1 for ds in schedule.values() if day in ds)})
    res = CliRunner().invoke(cli_harness.harness_app, ["daily", "--league", "espn", "--no-archive"])
    assert res.exit_code == 0, res.output
    assert "schedule-confirmed" in res.output and "Daily Faceoff snapshot taken" in res.output
    assert seen["lines"] == date.today()
    res = CliRunner().invoke(cli_harness.harness_app, ["daily", "--league", "espn", "--no-archive", "--json"])
    out = json.loads(res.output)
    assert out["deployment"]["scheduled"] == 0 and "_schedule" not in out["leagues"][0]


# --------------------------------------------------------------------------- CLI

runner = CliRunner()


def _cli_ctx():
    me_c = pl("my_c", "Mine Center", "EDM", ["C"], 10, {"season": line("season", 10, G=5)})
    me_g = pl("my_g", "Mine Goalie", "EDM", ["G"], 11, {"prior": line("prior", 50, GS=50, W=25)})
    fa = pl("fa_c", "Free Center", "EDM", ["C"], 12, {"season": line("season", 10, G=6)}, pct_owned_change=12.0)
    ctx = ctx_with([me_c, me_g], [fa], as_of=date.today(), provider="espn")
    ctx.teams[0].slots[0] = RosterSlot(slot="C", player=me_c, starting=True)
    return ctx


def _cli_patch(monkeypatch, tmp_path):
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("ESPN_LEAGUE_ID", "1")
    monkeypatch.setenv("FM_OFFLINE", "1")
    cli.get_settings.cache_clear()
    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: _cli_ctx())
    monkeypatch.setattr(cli, "console", Console(width=250))

    def fake_enrich(ctx, settings, cache, deep=False, **kw):
        by = {p.cid: p for p in ctx.all_players()}
        by["my_c"].line, by["my_c"].pp_unit, by["my_c"].line_change = "f1", "pp1", "PP2 -> PP1"
        by["my_g"].confirmed_start, by["my_g"].start_source = True, "DFO Confirmed: Mine Goalie starts EDM@CGY"
        ctx.schedule = {"EDM": [ctx.as_of]}
        ctx.source_notes.append("Daily Faceoff: lines for 1 teams")
        return ctx
    monkeypatch.setattr("fantasy_manager.providers.enrich.enrich_context", fake_enrich)


def test_cli_alerts_and_advise_include_alerts(monkeypatch, tmp_path):
    _cli_patch(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["--json", "alerts"])
    assert res.exit_code == 0, res.output
    alerts = json.loads(res.output)["alerts"]
    assert alerts and all(a["kind"] == "alert" for a in alerts)
    assert any("Mine Center" in a["title"] for a in alerts)
    res = runner.invoke(cli.app, ["--json", "advise"])
    assert res.exit_code == 0, res.output
    assert any(r["kind"] == "alert" for r in json.loads(res.output)["recommendations"])
    res = runner.invoke(cli.app, ["alerts"])
    assert res.exit_code == 0 and "Alerts" in res.output


def test_cli_lines_goalies_trending(monkeypatch, tmp_path):
    _cli_patch(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["lines"])
    assert res.exit_code == 0, res.output
    assert "Mine Center" in res.output and "PP1" in res.output and "PP2 -> PP1" in res.output
    res = runner.invoke(cli.app, ["--json", "lines"])
    rows = {r["cid"]: r for r in json.loads(res.output)["players"]}
    assert rows["my_c"]["pp_unit"] == "pp1" and rows["my_g"]["confirmed_start"] is True
    res = runner.invoke(cli.app, ["goalies"])
    assert res.exit_code == 0, res.output
    assert "My goalies tonight" in res.output and "starting" in res.output
    res = runner.invoke(cli.app, ["lines", "XYZ"])
    assert res.exit_code == 1 and "unknown NHL team" in res.output
    monkeypatch.setattr(espn.EspnProvider, "trending", lambda self, limit=50, fallers=False: [
        {"cid": "espn:1", "espn_id": "1", "name": "Riser Guy", "team": "EDM", "positions": ["C"],
         "status": "healthy", "pct_owned": 30.0, "pct_owned_change": 9.5 if not fallers else -4.0,
         "pct_started": 20.0, "in_league": "fa"}])
    res = runner.invoke(cli.app, ["trending"])
    assert res.exit_code == 0, res.output
    assert "Riser Guy" in res.output and "+9.5" in res.output and "free agent" in res.output
    res = runner.invoke(cli.app, ["--json", "trending", "--fallers"])
    assert json.loads(res.output)["players"][0]["pct_owned_change"] == -4.0


def test_cli_footer_credits():
    ctx = _cli_ctx()
    ctx.my_team.players[0].ixg_per_game = 0.3
    ctx.my_team.players[0].pp_unit = "pp1"
    assert cli.credit_lines(ctx) == [cli.MONEYPUCK_CREDIT, cli.DFO_CREDIT]
    ctx.source_notes.append(f"{CREDIT} (2025-26 for 1 skaters)")
    assert cli.credit_lines(ctx) == [cli.DFO_CREDIT]


# --------------------------------------------------------------------------- web + digest

def _web_result():
    from tests.test_web import make_result

    res = make_result()
    by = {p.cid: p for p in res.ctx.all_players()}
    by["a"].line, by["a"].pp_unit = "f1", "pp1"
    by["a"].ixg_per_game, by["a"].goals_minus_ixg, by["a"].xg_split = 0.3, 3.0, "season"
    by["g"].confirmed_start, by["g"].start_source = True, "DFO Confirmed: Linus Ullmark starts OTT@BOS"
    res.recs.append(Recommendation(kind="alert", score=9.0, strength=8.0, title="PP1: Tim Stützle (PP2 -> PP1)",
                                   subjects=[by["a"]], reasons=[Reason(code="LINE_CHANGE", text="PP2 -> PP1")]))
    return res


def test_web_alerts_starts_badges_xg_and_credits():
    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    from fantasy_manager.web.app import create_app

    client = TestClient(create_app(lambda league: _web_result()))
    h = client.get("/").text
    assert "Alerts <i>1</i>" in h and "PP1: Tim St" in h                      # By type chip + top moves
    assert "Confirmed starts tonight" in h and "starting" in h
    assert '<span class="ub">F1</span>' in h and "ub-pp" in h
    assert "Expected goals: MoneyPuck.com" in h and "Daily Faceoff" in h           # footer credits
    h = client.get("/recommendations?kind=alert").text
    assert "PP1: Tim St" in h and "Goals &minus; ixG /GP" in h                     # xG row in the comparison
    h = client.get("/player/a").text
    assert "xG luck" in h and "+0.30" in h
    r = client.get("/roster").text
    assert '<span class="ub">F1</span>' in r


def test_overview_matchup_strip(monkeypatch):
    pytest.importorskip("fastapi")
    from types import SimpleNamespace

    from fastapi.testclient import TestClient

    from fantasy_manager.web import views
    from fantasy_manager.web.app import create_app

    m = SimpleNamespace(my_team="My Team", opponent_team="Rival", my_projected_total=101.5,
                        their_projected_total=88.0, days_left=4, stance="protect")
    monkeypatch.setattr(views, "matchup_page", lambda res, provider=None: {"m": m, "pct": 71, "gaps": []})
    h = TestClient(create_app(lambda league: _web_result())).get("/").text
    assert "This week's matchup" in h and "101.5" in h and "71%" in h and "Rival" in h
    monkeypatch.setattr(views, "matchup_page", lambda res, provider=None: (_ for _ in ()).throw(RuntimeError("x")))
    h = TestClient(create_app(lambda league: _web_result())).get("/").text
    assert "This week's matchup" not in h


def test_digest_alerts_starts_and_credit():
    res = _web_result()
    d = build_digest(res.ctx, res.values, res.recs, {}, datetime(2026, 10, 7, 8, 30))
    assert "## Alerts" in d.markdown and "PP1: Tim St" in d.markdown.split("## Alerts")[1]
    assert "## Confirmed starts tonight" in d.markdown and "Linus Ullmark (" in d.markdown
    assert "Expected goals: MoneyPuck.com" in d.markdown and "Daily Faceoff" in d.html
    assert "Goalies tonight: Linus Ullmark starting" in d.summary and "Alerts 1" in d.summary


def test_fantrax_daily_lock_drops_weekly_wording():
    from fantasy_manager.analysis.matchup import lineups_locked
    from fantasy_manager.recommend.lineup import WEEKLY_LOCK_TEXT, recommend_lineup
    from fantasy_manager.valuation.valuate import PlayerValue

    a, b = pl("a", "a", "EDM", ["C"]), pl("b", "b", "EDM", ["C"])
    vals = {x.cid: PlayerValue(player=x, fpg=f, fpg_season=f, fpg_week=f, vorp=f) for x, f in ((a, 1.0), (b, 3.0))}
    t = FantasyTeam(team_id="1", name="me", owner_is_me=True,
                    slots=[RosterSlot(slot="C", player=a, starting=True), RosterSlot(slot="BN", player=b, starting=False)])
    for lock, weekly in (("daily", False), ("weekly", True), (None, True)):
        ctx = LeagueContext(provider="fantrax", league_id="1", season=2027, name="T", lineup_lock=lock,
                            scoring=ScoringConfig(kind="points"), roster_shape={"C": 1, "BN": 1},
                            teams=[t], free_agents=[], matchup_period=1, as_of=date(2026, 10, 1))
        recs = recommend_lineup(ctx, vals)
        assert recs and all((WEEKLY_LOCK_TEXT in r.title) == weekly for r in recs), lock
        assert lineups_locked(ctx) is weekly


def test_pipeline_loader_keeps_provider_cache_until_replaced(monkeypatch):
    pytest.importorskip("fastapi")
    from fantasy_manager.web import app as web_app

    class Cache:
        closed = False

        def close(self):
            self.closed = True

    res = _web_result()
    caches = []

    def base_loader(league):
        caches.append(Cache())
        return web_app.BaseLoad(ctx=res.ctx, values=res.values, provider=None, news_by_cid={},
                                loaded_at=datetime(2026, 10, 7), seconds=0.1, cache=caches[-1])
    now = [0.0]
    monkeypatch.setattr(web_app, "finish", lambda base, mode, source, recalc_only=False: base)
    ld = web_app.PipelineLoader(ttl=10, clock=lambda: now[0], base_loader=base_loader)
    ld("espn")
    assert not caches[0].closed                    # the matchup preview still needs the provider's cache
    now[0] = 11.0
    ld("espn")
    assert caches[0].closed and not caches[1].closed
