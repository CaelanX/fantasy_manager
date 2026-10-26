"""The weekly action plan (recommend/plan.py): add scheduling, move budget, stance and safety rules."""
from datetime import date, timedelta
from types import SimpleNamespace

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.recommend.plan import archive_plan, build_week_plan
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.valuate import valuate_league

MON = date(2026, 10, 5)
DAYS = [MON + timedelta(days=k) for k in range(7)]          # Mon Oct 5 - Sun Oct 11
NO_GOALIES = SimpleNamespace(plan=[], mine=[], free_agents=[])


def mk(cid, team, goals_pg, status="healthy", pos=("C",)):
    lines = {"projected": StatLine(split="projected", gp=80, stats={"G": goals_pg * 80, "GP": 80}),
             "season": StatLine(split="season", gp=20, stats={"G": goals_pg * 20, "GP": 20})}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=list(pos), status=status,
                  lines=lines)


def ctx_for(mine, fas, schedule, moves=2, provider="test", shape=None):
    slots = [RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR")) for s, p in mine]
    shape = shape or {"C": 1, "BN": 1, "IR": 1}
    # filler teams so an "average team's games" exists across the week
    sched = {**{f"Z{i}": DAYS for i in range(4)}, **schedule}
    return LeagueContext(provider=provider, league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=shape,
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=fas, matchup_period=1, as_of=MON, period_end=DAYS[-1], schedule=sched,
                         games_per_day={d: 8 for d in DAYS}, moves_limit_per_period=moves,
                         moves_used_this_period=0, lineup_lock="daily")


def matchup(stance="even"):
    return SimpleNamespace(opponent_team=None, start=MON, end=DAYS[-1], my_points_so_far=0.0,
                           their_points_so_far=0.0, their_projected_remaining=0.0, stance=stance,
                           their_players=[], draws=200)


def plan_for(ctx, stance="even"):
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    return build_week_plan(ctx, values, recs=[], matchup=matchup(stance), goalies=NO_GOALIES)


def _streaming_ctx(moves=2, provider="test"):
    """One C slot. my_c plays Mon only; weak (bench) never plays; x plays Tue-Wed, y Fri-Sun."""
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    fas = [mk("x", "TW", 2.0), mk("y", "FS", 2.0)]
    sched = {"MON": [DAYS[0]], "TW": DAYS[1:3], "FS": DAYS[4:7], "NONE": []}
    return ctx_for(mine, fas, sched, moves=moves, provider=provider)


def test_sequence_streams_two_players_when_it_beats_one_add():
    p = plan_for(_streaming_ctx())
    assert [(m.add.cid, m.drop.cid if m.drop else None) for m in p.moves] == [("x", "weak"), ("y", "x")]
    first, second = p.moves
    assert first.effective == DAYS[1] and second.effective == DAYS[4]       # x from Tue, y from Fri
    assert first.starts == DAYS[1:3] and second.starts == DAYS[4:7]
    assert p.proj_plan - p.proj_lineup == pytest.approx(sum(m.gain_pts for m in p.moves))
    assert p.moves_gain == pytest.approx(5 * 2.0, rel=0.05)               # 5 starts at ~2 pts


def test_never_uses_more_moves_than_left():
    p = plan_for(_streaming_ctx(moves=1))
    assert len(p.moves) == 1 and p.moves[0].add.cid == "y"                 # 3 games beats 2
    assert plan_for(_streaming_ctx(moves=0)).moves == []


def test_protect_stance_keeps_a_move_in_reserve():
    p = plan_for(_streaming_ctx(moves=2), stance="protect")
    assert len(p.moves) == 1
    assert any("reserve" in n for n in p.notes)


def test_bench_games_do_not_count():
    """The pickup only plays on nights my better starter already fills the slot: no move."""
    mine = [("C", mk("my_c", "ALL", 3.0)), ("BN", mk("weak", "NONE", 0.1))]
    sched = {"ALL": DAYS, "NONE": []}
    p = plan_for(ctx_for(mine, [mk("fa", "ALL", 2.0)], sched))
    assert p.moves == []
    assert p.proj_plan == pytest.approx(p.proj_lineup)


def test_injured_player_is_never_the_drop():
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("hurt", "NONE", 0.05, status="out"))]
    sched = {"MON": [DAYS[0]], "TW": DAYS[1:5], "NONE": []}
    p = plan_for(ctx_for(mine, [mk("x", "TW", 2.0)], sched))
    assert all(m.drop is None or m.drop.cid != "hurt" for m in p.moves)


def test_fantrax_adds_count_the_same_day_before_the_first_game():
    """Fantrax's transaction log: a morning add counts that day (period), one after the games the next."""
    p = plan_for(_streaming_ctx(moves=1, provider="fantrax"))
    (m,) = p.moves
    assert m.effective == m.day
    assert any("same day" in n for n in p.notes)


def test_fantrax_add_after_todays_first_game_counts_tomorrow():
    from datetime import datetime, timezone

    from fantasy_manager.recommend.plan import _earliest

    ctx = _streaming_ctx(moves=1, provider="fantrax")
    first = datetime(2026, 10, 5, 23, 0, tzinfo=timezone.utc)
    ctx = ctx.model_copy(update={"game_starts": {"MON": {MON: first}, "TW": {DAYS[1]: first + timedelta(days=1)}}})
    fa = ctx.free_agents[0]                                   # x: no game today, still locked out today
    assert _earliest(ctx, fa, first - timedelta(hours=1)) == MON
    assert _earliest(ctx, fa, first + timedelta(minutes=5)) == DAYS[1]


def test_day_lines_and_archive(tmp_path):
    p = plan_for(_streaming_ctx())
    assert len(p.days) == 7
    tue = p.days[1]
    assert tue.moves and "Add x, drop weak, then quick lineup" in tue.text
    assert p.days[2].text == "Quick lineup is fine"          # x simply has the only game: nothing to call out
    assert archive_plan(p, tmp_path, "1") == "created"
    assert archive_plan(p, tmp_path, "1") == "unchanged"
    files = list((tmp_path / "archive").glob("plan-test-*.json"))
    assert len(files) == 1


def test_weekly_lock_leagues_get_no_plan():
    ctx = _streaming_ctx().model_copy(update={"provider": "fantrax", "lineup_lock": "weekly"})
    p = plan_for(ctx)
    assert not p.available and p.notes


def test_plan_page_is_home_when_configured():
    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    from fantasy_manager.web.app import create_app
    from tests.test_web import make_result

    c = TestClient(create_app(lambda league: make_result(league), home="plan"))
    home = c.get("/?league=espn")
    assert home.status_code == 200
    assert "plan" in home.text.lower() and 'aria-current="page">This week' in home.text
    assert c.get("/overview?league=espn").status_code == 200
    default = TestClient(create_app(lambda league: make_result(league)))
    assert "plan-steps" not in default.get("/?league=espn").text              # tests' default home: overview
    assert default.get("/plan?league=espn").status_code == 200


def test_restreamed_pickup_is_credited_only_with_the_nights_it_keeps():
    """x plays Tue, Wed and Sat; y (Thu-Sun) replaces x on Thu. x's step must count only Tue-Wed
    (the nights it has in the final plan, as its headline says), not the Sat it loses to y."""
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    sched = {"MON": [DAYS[0]], "TWS": [DAYS[1], DAYS[2], DAYS[5]], "TS": DAYS[3:7], "NONE": []}
    p = plan_for(ctx_for(mine, [mk("x", "TWS", 2.0), mk("y", "TS", 2.0)], sched))
    assert [(m.add.cid, m.drop.cid) for m in p.moves] == [("x", "weak"), ("y", "x")]
    x, y = p.moves
    assert x.starts == DAYS[1:3] and y.starts == DAYS[3:7]
    for m in p.moves:
        assert m.gain_pts == pytest.approx(2.0 * len(m.starts), rel=0.05)
    assert p.proj_plan - p.proj_lineup == pytest.approx(x.gain_pts + y.gain_pts)
    assert "x would have started 1 of his 1 game left (Sat)" in y.headline


def test_baseline_is_quick_lineup_and_no_lineup_setting_step():
    """The plan takes no credit for setting lineups: the baseline is the best lineup each day
    (quick lineup) with no moves, and only moves count as gains."""
    p = plan_for(_streaming_ctx())
    assert p.baseline == "quick_lineup"
    assert p.proj_as_set == pytest.approx(p.proj_lineup) and p.win_as_set == p.win_lineup
    assert p.lineup_gain == 0
    assert p.proj_frozen <= p.proj_lineup                     # frozen lineup kept for reference only
    assert all("best lineup each day" not in a.title.lower() for a in p.today)
    assert sum(d.pts_as_set for d in p.days) == pytest.approx(p.proj_lineup, abs=0.05)


def test_day_line_calls_out_a_start_quick_lineup_would_miss():
    """Two Cs play Tue, one slot: quick lineup starts the higher season value (a), our day value
    prefers b (here: a's season value is inflated relative to his day value)."""
    mine = [("C", mk("a", "ALL", 1.0)), ("BN", mk("b", "ALL", 2.0))]
    ctx = ctx_for(mine, [], {"ALL": DAYS}, moves=0)
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    values["a"] = values["a"].model_copy(update={"fpg_season": 9.0})
    p = build_week_plan(ctx, values, recs=[], matchup=matchup(), goalies=NO_GOALIES)
    line = p.days[1]
    assert not line.quick_ok and line.ins and line.ins[0].startswith("b over a")
    assert "after quick lineup start b over a" in line.text


def test_restream_chain_story_and_long_term_against_the_actual_drop():
    p = plan_for(_streaming_ctx())
    first, second = p.moves
    assert (first.role, second.role) == ("rental", "swap") and first.chain == second.chain == 1
    assert first.note.startswith("Rent x Tue-Wed: 2 nights your C slot would otherwise miss")
    assert second.note.startswith("Then swap x for y Fri: x has no games left")
    assert "last move this week" in second.note                # 2 moves left, both used
    # the long-term line compares y with the player this step drops (x), not the waiver rec's drop
    assert second.reasons and second.reasons[0].startswith("Rest of season: y") and "vs x " in second.reasons[0]
    assert "vs weak" not in " ".join(second.reasons)
    assert p.net_week and p.net_week[0].startswith("Net this week: weak gone; y stays; x rented and released")


def _starts_at(sched, hour_utc=1):
    """``game_starts`` for ``sched``: every game at ``hour_utc`` UTC the next calendar day (evening in
    North America)."""
    from datetime import datetime, timezone

    return {t: {d: datetime(d.year, d.month, d.day, tzinfo=timezone.utc) + timedelta(days=1, hours=hour_utc)
                for d in ds} for t, ds in sched.items()}


def test_move_timing_evening_before_and_before_first_game(monkeypatch):
    """The swap's drop (x) plays Thu, the day before y counts (Fri): make it Thu evening after x's game
    starts. The first add drops a player with no games: make it before Tue's first game."""
    from datetime import timezone

    import fantasy_manager.recommend.plan as plan_mod

    monkeypatch.setattr(plan_mod, "_local_zone", lambda: timezone(timedelta(hours=-6)))
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    sched = {"MON": [DAYS[0]], "TWT": DAYS[1:4], "FS": DAYS[4:7], "NONE": []}
    ctx = ctx_for(mine, [mk("x", "TWT", 2.0), mk("y", "FS", 2.0)], sched)
    ctx = ctx.model_copy(update={"game_starts": _starts_at({**ctx.schedule})})
    p = plan_for(ctx)
    first, second = p.moves
    assert first.timing == "Make this move before Tue's first game (7:00 PM your time)"
    assert second.effective == DAYS[4] and second.day == DAYS[3]           # made Thu evening, counts Fri
    assert second.timing == ("Make this move the evening before (after x's game starts Thu at 7:00 PM your "
                             "time), so he keeps those points and y counts Fri")
    assert any(m.day == DAYS[3] for m in p.days[3].moves)                  # listed on Thu's day line


def test_clock_uses_the_offset_of_that_day_across_dst():
    """Nov 1 2026: Mountain time falls back (MDT -6 -> MST -7) at 08:00 UTC."""
    from datetime import datetime, timezone, tzinfo

    import fantasy_manager.recommend.plan as plan_mod

    class Mountain(tzinfo):
        switch = datetime(2026, 11, 1, 8, 0)

        def utcoffset(self, dt):
            return timedelta(hours=-6) if dt.replace(tzinfo=None) < self.switch - timedelta(hours=6) else timedelta(hours=-7)

        def dst(self, dt):
            return timedelta(0)

        def fromutc(self, dt):
            off = timedelta(hours=-6) if dt.replace(tzinfo=None) < self.switch else timedelta(hours=-7)
            return (dt + off).replace(tzinfo=self)

    old = plan_mod._local_zone
    plan_mod._local_zone = lambda: Mountain()
    try:
        assert plan_mod._clock(datetime(2026, 10, 31, 17, 0, tzinfo=timezone.utc)) == "11:00 AM"
        assert plan_mod._clock(datetime(2026, 11, 1, 18, 0, tzinfo=timezone.utc)) == "11:00 AM"   # WPG @ CAR 1 PM ET
    finally:
        plan_mod._local_zone = old


def test_local_zone_ignores_unknown_names(monkeypatch):
    import fantasy_manager.recommend.plan as plan_mod

    monkeypatch.setenv("FM_TZ", "Not/AZone")
    monkeypatch.delenv("TZ", raising=False)
    assert plan_mod._local_zone() is None                                   # the OS zone is used


def test_day_note_when_the_plan_is_below_quick_lineup():
    """weak (bench) plays only Thu; x plays Tue, Wed, Fri, Sat. Dropping weak for x costs Thu."""
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "THU", 0.5))]
    sched = {"MON": [DAYS[0]], "THU": [DAYS[3]], "X": [DAYS[1], DAYS[2], DAYS[4], DAYS[5]]}
    p = plan_for(ctx_for(mine, [mk("x", "X", 2.0)], sched, moves=1))
    (m,) = p.moves
    assert (m.add.cid, m.drop.cid) == ("x", "weak")
    thu = p.days[3]
    assert thu.pts_plan < thu.pts_as_set
    assert thu.note == (f"Lower than quick lineup Thu: weak (dropped Tue) would have played; x's extra nights "
                        f"make up for it ({p.moves_gain:+.1f} for the week)")
    assert thu.note in thu.text
    assert all(not d.note for d in p.days if d.day != DAYS[3])


def test_restream_must_stay_within_long_term_tolerance():
    """y plays the weekend but is a much weaker player than x long term: no x -> y swap."""
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    sched = {"MON": [DAYS[0]], "TW": DAYS[1:3], "FS": DAYS[4:7], "NONE": []}
    p = plan_for(ctx_for(mine, [mk("x", "TW", 2.0), mk("y", "FS", 0.5)], sched))
    assert all(not (m.drop is not None and m.drop.cid == "x") for m in p.moves)
