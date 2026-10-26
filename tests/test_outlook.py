"""Contention window / mode advisor (analysis.outlook) and the trade finder by need
(recommend.trades.recommend_need_trades, `fm outlook`, `fm trades --need`)."""
import json
from datetime import date, timedelta
from types import SimpleNamespace

import pytest

from fantasy_manager.analysis.outlook import (CONTEND_ODDS, TeamOutlook, build_outlook, outlook_json,
                                              playoff_odds, recommend_mode, recommendation_text)
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig
from fantasy_manager.recommend.trades import (_complementary, find_needs, pitch_of, recommend_need_trades)
from fantasy_manager.valuation.valuate import PlayerValue

from .test_trades import build

SHAPE = {"C": 1, "LW": 1, "D": 2, "G": 1, "BN": 2, "IR": 1}


def _pl(cid, pos, born=None):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=pos, status="healthy",
                  birth_date=born)


def _pv(p, fpg):
    return PlayerValue(player=p, fpg=fpg, fpg_season=fpg, fpg_week=fpg, vorp=fpg - 1.0, vorp_week=fpg - 1.0,
                       horizon_values={"season": fpg, "week": fpg, "vorp_season": fpg - 1.0,
                                       "vorp_week": fpg - 1.0})


def _row(tid, now, record, mine=False, future=None):
    t = FantasyTeam(team_id=tid, name=tid, owner_is_me=mine, slots=[])
    return TeamOutlook(team=t, record=record, win_pct=None, now=now, future=future or [])


def _dyn_league(my_record, their_record):
    """Me: an old star (age 35) in the lineup, a young bench prospect; them: a 21-year-old riser."""
    as_of = date(2026, 10, 26)

    def born(age):
        return as_of - timedelta(days=int(age * 365.25))

    spec = {
        "me": [("C", "old_star", ["C"], 4.0, 35), ("LW", "my_lw", ["LW"], 2.0, 28), ("D", "my_d1", ["D"], 2.0, 27),
               ("D", "my_d2", ["D"], 1.5, 29), ("G", "my_g", ["G"], 2.5, 30), ("BN", "kid", ["C"], 1.0, 19)],
        "opp": [("C", "riser", ["C"], 3.0, 21), ("LW", "o_lw", ["LW"], 2.5, 26), ("D", "o_d1", ["D"], 2.4, 25),
                ("D", "o_d2", ["D"], 2.0, 31), ("G", "o_g", ["G"], 2.6, 28), ("BN", "o_vet", ["LW"], 3.5, 27)],
    }
    values, dyn, teams = {}, {}, []
    for tid, rows in spec.items():
        slots = []
        for slot, cid, pos, f, age in rows:
            p = _pl(cid, pos, born(age))
            values[cid] = _pv(p, f)
            dyn[cid] = SimpleNamespace(age=float(age), pedigree=1.0, value=10.0 + f)
            slots.append(RosterSlot(slot=slot, player=p, starting=slot not in ("BN", "IR")))
        rec = my_record if tid == "me" else their_record
        teams.append(FantasyTeam(team_id=tid, name=f"Team {tid}", owner_is_me=tid == "me", slots=slots, record=rec))
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=SHAPE,
                        teams=teams, free_agents=[], matchup_period=1, as_of=as_of, dynasty=True,
                        dynasty_mode="balanced")
    return ctx, values, dyn


def test_playoff_odds_follow_strength_and_record():
    rows = [_row("a", 40.0, (3, 1, 0)), _row("b", 30.0, (1, 3, 0)), _row("c", 35.0, (2, 2, 0)),
            _row("d", 32.0, (2, 2, 0))]
    odds = playoff_odds(rows, weeks=10, spots=2, sims=400)
    assert odds["a"] > odds["c"] > odds["b"]
    assert sum(odds.values()) == pytest.approx(2.0)      # exactly two spots per draw
    assert playoff_odds(rows, weeks=10, spots=2, sims=400) == odds   # fixed seed: reproducible
    done = playoff_odds(rows, weeks=0, spots=2, sims=50)  # season over: the record decides
    assert done["a"] == 1.0 and done["b"] == 0.0


def test_recommend_mode_thresholds():
    me = _row("m", 30.0, (2, 2, 0), mine=True)
    me.now_rank = 6
    me.playoff_odds = CONTEND_ODDS + 0.05
    assert recommend_mode(me, 12)[0] == "contend"
    me.playoff_odds = 0.10
    assert recommend_mode(me, 12)[0] == "rebuild"
    me.playoff_odds, me.future_rank = 0.25, 3          # long shot now, window ahead
    assert recommend_mode(me, 12)[0] == "rebuild"
    me.future_rank = 9                                  # long shot and no better later
    mode, why = recommend_mode(me, 12)
    assert mode == "balanced" and any("#9 of 12" in w for w in why)


def test_redraft_outlook_labels_and_never_changes_mode():
    ctx, values = build()
    ctx.teams[0].record, ctx.teams[1].record, ctx.teams[2].record = (0, 4, 0), (4, 0, 0), (2, 2, 0)
    o = build_outlook(ctx, values, None, sims=200)
    assert not o.dynasty and o.recommended in ("contender", "bubble", "pretender")
    assert o.me is not None and o.me.mine and all(not r.future for r in o.teams)
    assert [r.now_rank for r in o.teams] == sorted(r.now_rank for r in o.teams)
    data = outlook_json(o)
    assert data["text"] == o.recommended and len(data["teams"]) == 3
    assert ctx.dynasty_mode == "contend"               # untouched


def test_dynasty_rebuild_sells_the_old_star_and_buys_the_riser():
    ctx, values, dyn = _dyn_league(my_record=(0, 8, 0), their_record=(8, 0, 0))
    o = build_outlook(ctx, values, dyn, sims=100)       # no schedule: 0 weeks left, the record decides
    assert o.me.playoff_odds == 0.0 and o.recommended == "rebuild"
    assert recommendation_text(o) == "recommended: rebuild (currently balanced)"
    assert ctx.dynasty_mode == "balanced"
    assert o.sell and o.sell[0].player.cid == "old_star"
    assert o.me.future and o.me.future[0] < o.me.now   # an aging lineup falls
    assert [a.player.cid for a in o.buy][:1] == ["riser"]


def test_dynasty_contend_sells_the_bench_prospect_and_buys_starters():
    ctx, values, dyn = _dyn_league(my_record=(8, 0, 0), their_record=(0, 8, 0))
    o = build_outlook(ctx, values, dyn, sims=100)
    assert o.recommended == "contend"
    assert [a.player.cid for a in o.sell] == ["kid"]                 # dynasty value, not in my lineup
    assert o.buy and o.buy[0].player.cid in ("riser", "o_vet")       # beats my weakest starter now
    assert "likely seller" in o.buy[0].why                           # they are rebuilding


def test_complementary_directions():
    assert _complementary("contend", "rebuild") and _complementary("pretender", "contender")
    assert not _complementary("contend", "contend") and not _complementary("balanced", "rebuild")
    assert not _complementary(None, "rebuild")


def test_need_trades_fill_my_weak_slot_with_a_pitch():
    ctx, values = build()                               # me: C-rich, D-poor; Team A: D-rich
    needs = find_needs(ctx, values)
    assert needs and needs[0].slot == "D" and needs[0].gap < 0
    recs = recommend_need_trades(ctx, values, lineup_fn=False, limit=5)
    assert recs
    top = recs[0]
    assert any(p.positions == ["D"] for p in top.add)
    codes = [r.code for r in top.reasons]
    assert codes[:2] == ["MY_EDGE", "MARKET_VIEW"] and "NEED_MATCH" in codes and "PITCH" in codes
    pitch = pitch_of(top)
    assert pitch.startswith(f"Hi {top.counterparty}, I'd send ") and "D" in pitch
    assert "VORP" not in pitch and "EV" not in pitch    # the numbers stay with me
    assert all(r.kind == "trade" and r.score > 0 for r in recs)


def test_need_trades_flag_star_consolidation():
    ctx, values = build()
    ctx.roster_shape = {"C": 1, "LW": 1, "D": 2, "G": 1, "BN": 3, "IR": 1}
    # my LW is weak; Team B's benched star plays C/LW; I have two good C to send
    for cid, f in (("m_lw", 0.5), ("m_c1", 4.2), ("m_c2", 4.0)):
        values[cid] = _pv(values[cid].player, f)
    recs = recommend_need_trades(ctx, values, lineup_fn=False, limit=10, per_team=5)
    stars = [r for r in recs if any(x.code == "STAR_CONSOLIDATION" for x in r.reasons)]
    assert stars and all(len(r.drop) == 2 and [p.cid for p in r.add] == ["b_star"] for r in stars)
    assert "best player in the deal (b_star)" in next(x.text for x in stars[0].reasons
                                                       if x.code == "STAR_CONSOLIDATION")
    assert "Two everyday players for one" in pitch_of(stars[0])


def test_cli_outlook_and_need_trades(monkeypatch, tmp_path):
    from fantasy_manager import cli
    from .test_cli import _patch2, runner

    _patch2(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["outlook"])
    assert res.exit_code == 0, res.output
    assert "Outlook:" in res.output and "Playoff odds" in res.output
    res = runner.invoke(cli.app, ["--json", "outlook"])
    data = json.loads(res.output)["outlook"]
    assert data["recommended"] in ("contender", "bubble", "pretender") and data["teams"]
    res = runner.invoke(cli.app, ["trades", "--need"])
    assert res.exit_code == 0, res.output
    assert "Your needs:" in res.output
    res = runner.invoke(cli.app, ["--json", "trades", "--need"])
    data = json.loads(res.output)
    assert {"needs", "need_trades", "direction"} <= set(data)
