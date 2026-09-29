import math
from datetime import date

import pytest

from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig,
                                    StatLine)
from fantasy_manager.scoring import (CategoriesScoring, RotoScoring, fit_to_context, from_config,
                                     pool_from_players)


def test_counting_z_scores_match_known_mean_and_sigma():
    pool = [{"G": 0.0}, {"G": 1.0}, {"G": 2.0}, {"G": 3.0}]
    s = CategoriesScoring(["G"]).fit(pool)
    mu, sd = s.params()["G"]
    assert mu == pytest.approx(1.5) and sd == pytest.approx(math.sqrt(1.25))
    assert s.value({"G": 3.0}) == pytest.approx(1.5 / math.sqrt(1.25))
    assert s.value({"G": 1.5}) == pytest.approx(0.0)


def test_multiple_categories_sum_and_weights():
    pool = [{"G": 0.0, "A": 0.0}, {"G": 2.0, "A": 4.0}]   # mu G=1 sd 1; mu A=2 sd 2
    s = CategoriesScoring(["G", "A"], {"A": 0.5}).fit(pool)
    assert s.breakdown({"G": 2.0, "A": 4.0}) == pytest.approx({"G": 1.0, "A": 0.5})
    assert s.value({"G": 2.0, "A": 4.0}) == pytest.approx(1.5)
    assert s.value({"G": 2.0}) == pytest.approx(1.0)   # missing category contributes nothing


def test_skater_stats_standardized_among_skaters_only():
    pool = [{"G": 0.0}, {"G": 2.0}, {"SV": 25.0, "SA": 27.0}]
    s = CategoriesScoring(["G"]).fit(pool)
    assert s.params()["G"] == pytest.approx((1.0, 1.0))
    assert s.value({"SV": 25.0, "SA": 27.0}) == 0.0


def test_unfitted_returns_zero_and_lazy_fit_with_pool():
    s = CategoriesScoring(["G"])
    assert not s.fitted and s.value({"G": 2.0}) == 0.0
    pool = [{"G": 0.0}, {"G": 2.0}]
    assert s.value({"G": 2.0}, pool) == pytest.approx(1.0)
    assert s.fitted and s.value({"G": 0.0}) == pytest.approx(-1.0)   # reuses fitted stats


def test_negative_categories_flip_sign():
    pool = [{"GAA": 2.0, "GS": 1.0}, {"GAA": 3.0, "GS": 1.0}, {"GAA": 4.0, "GS": 1.0}]
    for directions in (None, {"GAA": 1}, {"GAA": -1}):
        s = CategoriesScoring(["GAA"], directions).fit(pool)
        assert s.value({"GAA": 2.0, "GS": 1.0}) == pytest.approx(1 / math.sqrt(2 / 3))
    pim_pool = [{"PIM": 0.0}, {"PIM": 2.0}]
    assert CategoriesScoring(["PIM"]).fit(pim_pool).value({"PIM": 2.0}) == pytest.approx(1.0)
    assert CategoriesScoring(["PIM"], {"PIM": -1}).fit(pim_pool).value({"PIM": 2.0}) == pytest.approx(-1.0)


def goalie(sa, pct, gs=1.0):
    return {"SA": sa, "SV": sa * pct, "SVPCT": pct, "GS": gs}


def test_svpct_volume_weighting_rewards_high_shot_goalie():
    pool = [goalie(30, 0.90), goalie(25, 0.89), goalie(28, 0.91), goalie(22, 0.90)]
    s = CategoriesScoring(["SVPCT"]).fit(pool)
    ref, sd = s.params()["SVPCT"]
    assert ref == pytest.approx(sum(p["SV"] for p in pool) / sum(p["SA"] for p in pool))
    busy, quiet = s.value(goalie(32, 0.915)), s.value(goalie(20, 0.915))
    assert busy > quiet > 0
    assert busy == pytest.approx((32 * 0.915 - ref * 32) / sd)
    # a below-reference SV% hurts more with more volume
    assert s.value(goalie(32, 0.88)) < s.value(goalie(20, 0.88)) < 0


def test_gaa_volume_weighted_by_starts():
    pool = [{"GAA": 2.5, "GS": 0.8}, {"GAA": 3.0, "GS": 0.6}, {"GAA": 3.2, "GS": 0.4}]
    s = CategoriesScoring(["GAA"]).fit(pool)
    ref, _ = s.params()["GAA"]
    assert ref == pytest.approx((2.5 * 0.8 + 3.0 * 0.6 + 3.2 * 0.4) / 1.8)
    assert s.value({"GAA": 2.4, "GS": 0.9}) > s.value({"GAA": 2.4, "GS": 0.3}) > 0


def test_from_config_passes_directions():
    s = from_config(ScoringConfig(kind="categories", categories=["G", "PIM"], weights={"PIM": -1}))
    assert isinstance(s, CategoriesScoring) and s.weight("PIM") == -1 and s.weight("G") == 1
    r = from_config(ScoringConfig(kind="roto", categories=["GAA"]))
    assert isinstance(r, RotoScoring) and r.weight("GAA") == -1


def test_team_totals_and_roto_balance():
    r = RotoScoring(["G", "SVPCT", "GAA"])
    tot = r.team_totals([{"G": 0.5}, {"G": 0.25}, goalie(30, 0.92), goalie(10, 0.88)])
    assert tot["G"] == pytest.approx(0.75)
    assert tot["SVPCT"] == pytest.approx((27.6 + 8.8) / 40)
    assert "GAA" not in tot
    me = {"G": 3.0, "GAA": 2.5}
    others = [{"G": 2.0, "GAA": 3.0}, {"G": 1.0, "GAA": 2.0}, {"G": 3.0, "GAA": 2.8}]
    pts = r.roto_points(me, others)
    assert pts == {"G": pytest.approx(3.5), "GAA": pytest.approx(3.0)}
    assert r.category_balance_penalty(me, others) == 0.0
    punted = {"G": 0.0, "GAA": 2.5}      # last in G: pts 1 vs mid 2.5 over 4 teams
    assert r.category_balance_penalty(punted, others) == pytest.approx((1.5 / 4) / 2)


def test_fit_to_context_uses_rostered_pool():
    def pl(cid, g, gp=10):
        return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=None, positions=["C"],
                      lines={"season": StatLine(split="season", gp=gp, stats={"G": g * gp, "GP": gp})})
    rostered = [pl("a", 0.0), pl("b", 2.0)]
    fa = pl("fa", 10.0)
    ctx = LeagueContext(provider="t", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="categories", categories=["G"]),
                        roster_shape={"C": 2},
                        teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True,
                                           slots=[RosterSlot(slot="C", player=p, starting=True) for p in rostered])],
                        free_agents=[fa], matchup_period=1, as_of=date(2026, 10, 1))
    assert pool_from_players(rostered) == [{"G": 0.0}, {"G": 2.0}]
    s = fit_to_context(from_config(ctx.scoring), ctx)
    assert s.value({"G": 2.0}) == pytest.approx(1.0)   # FA excluded from the reference pool
