"""Teammate-aware goalie start shares (valuation.schedule.teammate_shares, reason TEAMMATE_OUT)."""
from datetime import date, timedelta

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers.nhl import games_per_day
from fantasy_manager.recommend.lineup import apply_confirmed_starts
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.schedule import (ROSTER_ONLY_SHARE, TEAMMATE_CAP, context_window,
                                                redistribute_shares, teammate_shares)
from fantasy_manager.valuation.valuate import valuate_league

D = date(2026, 10, 7)
SCORING = {"W": 4.0, "SV": 0.2}


def share_of(gs: float) -> float:
    return (gs + 10 * 0.5) / (82 + 10)      # shrunk prior-season share (k=10 toward 0.5)


def goalie(cid, gs, status="healthy", note=None, team="EDM", nhl=None):
    stats = {"GS": gs, "W": gs * 0.5, "SV": gs * 25.0, "GP": max(gs, 1)}
    return Player(cid=cid, name=f"{cid.title()} {cid.title()}son", name_norm=cid,
                  ids={"nhl": str(nhl)} if nhl else {}, team=team, positions=["G"], status=status,
                  status_note=note, lines={"prior": StatLine(split="prior", gp=max(int(gs), 1), stats=stats)})


def make_ctx(players, as_of=D, lock="daily", nhl_goalies=None):
    # EDM plays every other day from opening night: 82 games, 4 in the first 7 days
    edm = [D + timedelta(days=2 * i) for i in range(82)]
    sched = {"EDM": edm, "CGY": [D + timedelta(days=2 * i + 1) for i in range(82)]}
    slots = [RosterSlot(slot="G", player=p, starting=True) for p in players]
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights=SCORING),
                         roster_shape={"G": 2, "BN": 3},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=[], matchup_period=1, as_of=as_of, schedule=sched,
                         games_per_day=games_per_day(sched), season_start=D, lineup_lock=lock,
                         nhl_goalies=nhl_goalies or {})


def suspended_until(days: int) -> str:
    return f"suspension (est. return {(D + timedelta(days=days)).isoformat()})"


def codes(pv):
    return {r.code: r for r in pv.reasons}


def test_backup_takes_over_week_when_starter_suspended_all_week():
    starter = goalie("helle", 60, status="suspended", note=suspended_until(30))
    backup = goalie("skin", 22)
    vals = valuate_league(make_ctx([starter, backup]), PointsScoring(SCORING))
    b = vals["skin"]
    assert b.start_share >= 0.85
    assert b.start_share == pytest.approx(TEAMMATE_CAP)       # 0.29 + 0.71 capped at 0.9
    r = codes(b)["TEAMMATE_OUT"]
    assert "Helleson suspended until ~Nov 6 (~15 of 82 games)" in r.text and "Skinson's start share 0.29 -> 0.90 this week" in r.text
    assert r.baseline == pytest.approx(share_of(22), abs=1e-4)
    assert b.proj_week == pytest.approx(b.fpg * 4 * 1.2 * TEAMMATE_CAP)
    # the suspended starter himself is untouched (no TEAMMATE_OUT, week value 0)
    assert "TEAMMATE_OUT" not in codes(vals["helle"]) and vals["helle"].proj_week == 0.0


def test_backup_season_share_rises_by_games_missed_fraction():
    starter = goalie("helle", 60, status="suspended", note=suspended_until(30))   # misses 15 of 82
    backup = goalie("skin", 22)
    vals = valuate_league(make_ctx([starter, backup]), PointsScoring(SCORING))
    expected = share_of(22) + share_of(60) * 15 / 82
    b = vals["skin"]
    assert b.fpg_season == pytest.approx(b.fpg * expected)
    assert "0.29 -> 0.42 rest of season" in codes(b)["TEAMMATE_OUT"].text
    assert b.share_source == "teammate"


def test_healthy_teammate_leaves_shares_unchanged():
    starter, backup = goalie("helle", 60), goalie("skin", 22)
    vals = valuate_league(make_ctx([starter, backup]), PointsScoring(SCORING))
    b = vals["skin"]
    assert b.start_share == pytest.approx(share_of(22))
    assert b.fpg_season == pytest.approx(b.fpg * share_of(22))
    assert "TEAMMATE_OUT" not in codes(b) and b.share_source == "history"


def test_backup_out_does_not_inflate_the_starter():
    starter = goalie("helle", 60)
    backup = goalie("skin", 22, status="ir", note="est. return 2026-12-01")
    vals = valuate_league(make_ctx([starter, backup]), PointsScoring(SCORING))
    assert vals["helle"].start_share == pytest.approx(share_of(60))
    assert "TEAMMATE_OUT" not in codes(vals["helle"])


def test_three_goalie_team_splits_in_proportion():
    starter = goalie("helle", 60, status="ltir")                  # rest of the season
    b1, b2 = goalie("skin", 22), goalie("third", 4)
    ctx = make_ctx([starter, b1, b2])
    vals = valuate_league(ctx, PointsScoring(SCORING))
    s0, s1, s2 = share_of(60), share_of(22), share_of(4)
    w1, w2 = s1 + s0 * s1 / (s1 + s2), s2 + s0 * s2 / (s1 + s2)
    assert w1 < TEAMMATE_CAP
    assert vals["skin"].start_share == pytest.approx(w1)
    assert vals["third"].start_share == pytest.approx(w2)
    assert vals["skin"].start_share + vals["third"].start_share == pytest.approx(s0 + s1 + s2)
    # LTIR without a timetable: the season split is the same as the week's
    assert vals["skin"].fpg_season == pytest.approx(vals["skin"].fpg * w1)
    assert "for the rest of the season" in codes(vals["third"])["TEAMMATE_OUT"].text


def test_partial_week_absence_uses_games_missed_in_window():
    # back on day 4: misses the games on days 0 and 2, plays days 4 and 6
    starter = goalie("helle", 60, status="suspended", note=suspended_until(4))
    backup = goalie("skin", 22)
    ctx = make_ctx([starter, backup])
    base = {"helle": share_of(60), "skin": share_of(22)}
    out = teammate_shares(ctx, base, context_window(ctx))
    assert out["skin"].week == pytest.approx(share_of(22) + share_of(60) * 0.5)
    assert out["skin"].absent[0].week_frac == pytest.approx(0.5)


def test_redistribute_cap_spills_to_the_others():
    new = redistribute_shares({"a": 0.7, "b": 0.5, "c": 0.1}, {"a": 1.0})
    assert new["b"] == pytest.approx(0.9)
    assert new["c"] == pytest.approx(0.1 + 0.7 - 0.4)       # b could only take 0.4
    # a lower-share absent goalie donates nothing
    assert redistribute_shares({"a": 0.7, "b": 0.3}, {"b": 1.0}) == {"a": 0.7}


def test_nhl_roster_excludes_minor_leaguers_and_counts_callups():
    starter = goalie("helle", 60, status="suspended", note=suspended_until(30), nhl=1)
    backup = goalie("skin", 22, nhl=2)
    farm = goalie("farm", 4, nhl=3)                     # in the pool but in the AHL
    ctx = make_ctx([starter, backup, farm], nhl_goalies={"EDM": {1: "Helle", 2: "Skin", 9: "Callup"}})
    base = {"helle": share_of(60), "skin": share_of(22), "farm": share_of(4)}
    out = teammate_shares(ctx, base, context_window(ctx))
    assert "farm" not in out
    s0, s1 = share_of(60), share_of(22)
    assert out["skin"].week == pytest.approx(s1 + s0 * s1 / (s1 + ROSTER_ONLY_SHARE))


def test_confirmed_start_still_overrides_today():
    starter = goalie("helle", 60, status="suspended", note=suspended_until(30))
    backup = goalie("skin", 22)
    ctx = make_ctx([starter, backup])
    vals = valuate_league(ctx, PointsScoring(SCORING))
    backup.confirmed_start, backup.start_source = False, "Daily Faceoff: Confirmed"
    new, used = apply_confirmed_starts(ctx, vals, "week")
    pv = vals["skin"]
    per_start = pv.proj_week / (pv.games_next7 * TEAMMATE_CAP)
    assert used["skin"] == (0.0, pytest.approx(TEAMMATE_CAP))
    assert new["skin"].proj_week == pytest.approx(pv.proj_week - per_start * TEAMMATE_CAP)
