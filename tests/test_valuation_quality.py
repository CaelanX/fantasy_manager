"""Small-sample shrinkage of baselines, goalie workload, replacement fallback and dynasty
future-year values."""
from datetime import date

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.blend import K_BASELINE, K_PROJECTION, baseline_sample, shrink_baseline
from fantasy_manager.valuation.dynasty import apply_dynasty
from fantasy_manager.valuation.replacement import percentile, replacement_with_fallback
from fantasy_manager.valuation.schedule import start_share
from fantasy_manager.valuation.valuate import valuate_league

GOALIE_W = {"W": 3.0, "SV": 0.2, "GA": -1.0, "SO": 2.0}


def goalie(cid, gp, svpct, w_rate=0.5, split="prior", team="EDM", gs=None):
    sa = 30.0 * gp
    sv = svpct * sa
    stats = {"GS": gp if gs is None else gs, "SA": sa, "SV": sv, "GA": sa - sv, "W": w_rate * gp,
             "SO": 0.05 * gp, "SVPCT": svpct}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=["G"],
                  lines={split: StatLine(split=split, gp=gp, stats=stats)})


def dman(cid, gp, pts_pg, split="prior"):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=["D"],
                  lines={split: StatLine(split=split, gp=gp, stats={"PTS": pts_pg * gp, "GP": gp})})


def ctx_of(rostered, fas=(), shape=None, dynasty=False):
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in rostered]
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={}),
                         roster_shape=shape or {"D": 2, "G": 2, "BN": 10},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=list(fas), matchup_period=1, as_of=date(2026, 9, 28),
                         dynasty=dynasty)


def test_three_gp_hot_goalie_values_below_sixty_gp_starter():
    hot = goalie("hot", 3, 0.950, w_rate=1.0)
    starter = goalie("starter", 60, 0.915, w_rate=0.55)
    fillers = [goalie(f"f{i}", 40, 0.900, w_rate=0.45) for i in range(4)]
    ctx = ctx_of([starter, *fillers], fas=[hot])
    vals = valuate_league(ctx, PointsScoring(GOALIE_W))
    raw_hot = PointsScoring(GOALIE_W).value(hot.lines["prior"].per_game())
    raw_starter = PointsScoring(GOALIE_W).value(starter.lines["prior"].per_game())
    assert raw_hot > raw_starter                             # face value: the backup looks better
    assert vals["hot"].fpg < raw_hot                         # shrunk toward the G mean
    assert vals["hot"].fpg_season < vals["starter"].fpg_season
    assert vals["hot"].vorp < vals["starter"].vorp
    # workload: season value carries the (team-games) start share
    share_hot = (3 + 5) / (82 + 10)
    assert vals["hot"].start_share == pytest.approx(share_hot)
    assert vals["hot"].fpg_season == pytest.approx(vals["hot"].fpg * share_hot)
    codes = {r.code for r in vals["hot"].reasons}
    assert {"START_SHARE", "BASELINE"} <= codes


def test_five_gp_defenseman_values_below_seventy_gp_one():
    # k = 14 (fitted, was 20): a 5-GP line keeps 26% of its own rate, so a 5-GP hot streak at
    # 1.2 PPG no longer trails a 70-GP 0.6 PPG line; it still trails a 70-GP 0.8 one
    small = dman("small", 5, 1.2)
    big = dman("big", 70, 0.8)
    fillers = [dman(f"f{i}", 60, 0.3) for i in range(5)]
    ctx = ctx_of([big, *fillers], fas=[small])
    vals = valuate_league(ctx, PointsScoring({"PTS": 1.0}))
    assert vals["small"].fpg < vals["big"].fpg
    mean = (0.8 + 5 * 0.3) / 6
    k = K_BASELINE["D"]    # one prior season, no birth date (no age factor): the D baseline k
    assert vals["small"].fpg == pytest.approx((5 * 1.2 + k * mean) / (5 + k))
    assert vals["big"].fpg == pytest.approx((70 * 0.8 + k * mean) / (70 + k))


def test_projection_counts_as_full_season_unless_tiny():
    full = StatLine(split="projected", gp=60, stats={"PTS": 30.0})
    tiny = StatLine(split="projected", gp=8, stats={"PTS": 8.0})
    prior = StatLine(split="prior", gp=60, stats={"PTS": 30.0})
    assert baseline_sample(full) == 82 and baseline_sample(tiny) == 8 and baseline_sample(prior) == 60
    mean = {"PTS": 0.3}
    r_full, _ = shrink_baseline(full, mean, False)
    r_tiny, _ = shrink_baseline(tiny, mean, False)
    assert r_full["PTS"] == pytest.approx((82 * 0.5 + 20 * 0.3) / 102)
    assert r_tiny["PTS"] == pytest.approx((8 * 1.0 + 20 * 0.3) / 28)
    assert shrink_baseline(full, {}, False)[0] == pytest.approx({"PTS": 0.5})   # no mean: untouched
    g = StatLine(split="prior", gp=6, stats={"W": 6.0})
    kg = K_PROJECTION["goalie"]
    assert shrink_baseline(g, {"W": 0.5}, True)[0]["W"] == pytest.approx((6 + kg * 0.5) / (6 + kg))


def test_season_only_small_sample_shrinks_toward_mean():
    """A player with only a tiny current-season line (no baseline) is no longer taken at face value."""
    lucky = goalie("lucky", 1, 0.970, w_rate=1.0, split="season")
    fillers = [goalie(f"f{i}", 40, 0.905) for i in range(3)]
    vals = valuate_league(ctx_of(fillers, fas=[lucky]), PointsScoring(GOALIE_W))
    raw = PointsScoring(GOALIE_W).value(lucky.lines["season"].per_game())
    assert vals["lucky"].fpg < 0.5 * raw + 0.5 * vals["f0"].fpg


def test_start_share_uses_team_games():
    g = goalie("g", 20, 0.91, gs=18)
    assert start_share(g) == pytest.approx((18 + 5) / (20 + 10))                      # legacy GS/GP
    assert start_share(g, team_games={"prior": 82}) == pytest.approx((18 + 5) / (82 + 10))
    no_gs = Player(cid="n", name="n", name_norm="n", ids={}, team="EDM", positions=["G"],
                   lines={"season": StatLine(split="season", gp=4, stats={"W": 2.0})})
    assert start_share(no_gs, team_games={"season": 10}) == pytest.approx((4 + 5) / (10 + 10))


def test_percentile_and_rostered_fallback():
    assert percentile([], 0.1) == 0.0
    assert percentile([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0], 0.1) == pytest.approx(2.0)
    players = {c: Player(cid=c, name=c, name_norm=c, ids={}, team=None, positions=pos)
               for c, pos in [("fa", ["C"]), ("g1", ["G"]), ("g2", ["G"]), ("g3", ["G"])]}
    levels, fb = replacement_with_fallback({"fa": 2.0}, {"g1": 1.0, "g2": 2.0, "g3": 3.0}, players, ["C", "G"])
    assert fb == {"G"} and levels["G"] == pytest.approx(1.2) and levels["C"] == pytest.approx(2.0 / 3)


def test_valuation_marks_fallback_replacement():
    gs = [goalie(f"g{i}", 50, 0.90 + i * 0.003) for i in range(5)]
    fa = dman("fa", 60, 0.3)
    vals = valuate_league(ctx_of([*gs, dman("d", 60, 0.5)], fas=[fa]), PointsScoring(GOALIE_W | {"PTS": 1.0}))
    codes = {r.code: r for r in vals["g4"].reasons}
    assert "REPL_FALLBACK" in codes and codes["REPL_FALLBACK"].value > 0
    assert "REPL_FALLBACK" not in {r.code for r in vals["d"].reasons}


def test_dynasty_does_not_carry_injury_into_future_years():
    hurt = dman("hurt", 70, 0.8)
    hurt.status = "ir"
    fine = dman("fine", 70, 0.8)
    ctx = ctx_of([hurt, fine], dynasty=True)
    vals = valuate_league(ctx, PointsScoring({"PTS": 1.0}))
    dyn = apply_dynasty(vals, ctx)
    assert vals["hurt"].fpg_season < vals["fine"].fpg_season
    # only year 0 is discounted: the 3-year value loses exactly that year's shortfall
    gap = vals["fine"].fpg_season - vals["hurt"].fpg_season
    assert dyn["fine"].value - dyn["hurt"].value == pytest.approx(gap)


def test_player_without_any_stats_is_not_given_the_mean():
    blank = Player(cid="blank", name="blank", name_norm="blank", ids={}, team="EDM", positions=["G"])
    fillers = [goalie(f"f{i}", 40, 0.905) for i in range(3)]
    vals = valuate_league(ctx_of(fillers, fas=[blank]), PointsScoring(GOALIE_W))
    assert vals["blank"].fpg == 0.0 and "NO_DATA" in {r.code for r in vals["blank"].reasons}


def test_dynasty_upside_ignores_small_samples():
    from fantasy_manager.valuation.dynasty import production_rate

    assert production_rate(dman("tiny", 3, 1.5)) is None
    assert production_rate(dman("real", 30, 0.5)) == pytest.approx(0.5)


def test_goalie_workload_comes_from_the_projection():
    """Blackwood (42 projected GP) must out-value Skinner (25 projected GP) even though
    Skinner started more games last season."""
    def proj_goalie(cid, prior_gp, proj_gp, fpts):
        g = goalie(cid, prior_gp, 0.905, w_rate=0.5)
        g.lines["projected"] = StatLine(split="projected", gp=proj_gp,
                                        stats={"GP": proj_gp, "W": 0.5 * proj_gp, "SV": 25.0 * proj_gp,
                                               "GA": 2.8 * proj_gp, "FPTS": fpts})
        return g
    blackwood = proj_goalie("blackwood", 39, 42, 42 * 6.5)
    skinner = proj_goalie("skinner", 50, 25, 25 * 6.2)
    fillers = [goalie(f"f{i}", 40, 0.900) for i in range(3)]
    vals = valuate_league(ctx_of([blackwood, *fillers], fas=[skinner]), PointsScoring(GOALIE_W))
    b, s = vals["blackwood"], vals["skinner"]
    assert b.start_share == pytest.approx(42 / 82) and s.start_share == pytest.approx(25 / 82)
    assert b.fpg_season == pytest.approx(42 * 6.5 / 82) and s.fpg_season == pytest.approx(25 * 6.2 / 82)
    assert b.fpg_season > s.fpg_season and b.vorp > s.vorp
    share = next(r for r in b.reasons if r.code == "START_SHARE")
    assert "42 projected starts" in share.text
    # without a projection the prior-season GS / team GP share is still used
    plain = goalie("plain", 50, 0.905)
    vals = valuate_league(ctx_of([plain, *fillers]), PointsScoring(GOALIE_W))
    assert vals["plain"].start_share == pytest.approx((50 + 5) / (82 + 10))


def test_goalie_projection_share_blends_with_this_seasons_starts():
    from fantasy_manager.valuation.valuate import PROJ_SHARE_K, projected_workload

    g = goalie("g", 10, 0.91, split="season", gs=10)
    g.lines["projected"] = StatLine(split="projected", gp=41, stats={"GP": 41})
    share, per_game, _ = projected_workload(g, {"EDM": 12})
    assert per_game is None
    assert share == pytest.approx((10 + PROJ_SHARE_K * 0.5) / (12 + PROJ_SHARE_K))
    assert projected_workload(dman("d", 50, 0.5)) is None


# -- multi-season baseline + league projection blend ------------------------------------------

from fantasy_manager.valuation import params as VP
from fantasy_manager.valuation.valuate import baseline_rates, season_age


def fwd(cid, lines, birth=None):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=["C"], birth_date=birth,
                  lines={s: StatLine(split=s, gp=gp, stats={"PTS": pg * gp, "GP": gp}) for s, (gp, pg) in lines.items()})


MEANS = {"F": {"PTS": 0.5}}


def test_history_only_baseline_is_three_seasons_weighted_and_shrunk():
    p = fwd("h", {"prior": (60, 0.9), "prior2": (80, 0.6), "prior3": (40, 0.3)})
    rates, line, n, src = baseline_rates(p, MEANS)
    wg = 5 * 60 + 4 * 80 + 3 * 40
    raw = (5 * 60 * 0.9 + 4 * 80 * 0.6 + 3 * 40 * 0.3) / wg
    k = K_BASELINE["F"]
    assert rates["PTS"] == pytest.approx((wg / 5 * raw + k * 0.5) / (wg / 5 + k))
    assert line.split == "prior" and n == pytest.approx(wg / 5)
    assert src.startswith("NHL history (180 GP over 3 seasons weighted 5/4/3")


def test_age_factor_applies_to_the_history_baseline():
    p = fwd("h", {"prior": (80, 0.6)}, birth=date(2005, 1, 1))
    as_of = date(2026, 9, 28)
    age = season_age(p, as_of)                                   # 21.75 on Oct 1 2026
    young, _, _, src = baseline_rates(p, MEANS, age)
    flat, _, _, _ = baseline_rates(p, MEANS)
    f = VP.age_factor("F", age - 1.0)                              # the season played at 20 -> 21
    assert f > 1.0 and young["PTS"] == pytest.approx(flat["PTS"] * f)
    assert f"x{f:.3f}" in src
    old = fwd("o", {"prior": (80, 0.6)}, birth=date(1992, 1, 1))
    assert baseline_rates(old, MEANS, season_age(old, as_of))[0]["PTS"] < flat["PTS"]
    g = Player(cid="g", name="g", name_norm="g", ids={}, team="EDM", positions=["G"], birth_date=date(1990, 1, 1),
               lines={"prior": StatLine(split="prior", gp=50, stats={"W": 25.0, "GP": 50})})
    assert baseline_rates(g, {"G": {"W": 0.5}}, 36.0)[0] == baseline_rates(g, {"G": {"W": 0.5}})[0]   # no goalie aging


def test_projection_blends_60_40_with_history_rookie_projection_only():
    proj_rates = lambda p: shrink_baseline(p.lines["projected"], MEANS["F"], False)[0]["PTS"]
    vet = fwd("v", {"projected": (80, 1.0), "prior": (70, 0.6), "prior2": (10, 0.6)})
    hist = baseline_rates(fwd("v2", {"prior": (70, 0.6), "prior2": (10, 0.6)}), MEANS)[0]["PTS"]
    rates, line, _, src = baseline_rates(vet, MEANS)
    assert line.split == "projected"
    assert rates["PTS"] == pytest.approx(0.6 * proj_rates(vet) + 0.4 * hist)
    assert src.startswith("60% projection") and "40% NHL history" in src
    rookie = fwd("r", {"projected": (70, 0.7)})
    assert baseline_rates(rookie, MEANS)[0]["PTS"] == pytest.approx(proj_rates(rookie))
    # 20 prior NHL GP: halfway along the ramp (0.8 projection / 0.2 history)
    part = fwd("p", {"projected": (70, 0.7), "prior": (20, 0.3)})
    hist_p = baseline_rates(fwd("p2", {"prior": (20, 0.3)}), MEANS)[0]["PTS"]
    assert baseline_rates(part, MEANS)[0]["PTS"] == pytest.approx(0.8 * proj_rates(part) + 0.2 * hist_p)


def test_packaged_params_load_and_fall_back(tmp_path):
    assert VP.PARAMS_FILE.exists()
    assert VP.load_params()["scoring"] == "espn"
    assert VP.k_baseline() == {"F": 8.0, "D": 14.0, "G": 120.0}
    assert VP.k_inseason() == {"skater": 25.0, "goalie": 40.0}
    assert VP.recency_weights() == {"season": 0.85, "last30": 0.15, "last15": 0.0, "last7": 0.0}
    missing = VP.load_params(str(tmp_path / "nope.json"))
    assert missing == {}
    assert VP.k_baseline(missing) == VP.FALLBACK_K_BASELINE and VP.k_inseason(missing) == VP.FALLBACK_K_INSEASON
    assert VP.recency_weights(missing) == VP.FALLBACK_RECENCY
    assert VP.age_yoy(missing) == VP.FALLBACK_AGE_YOY and VP.age_groups(missing) == ("F", "D")
    assert VP.age_factor("F", 20.4, VP.FALLBACK_AGE_YOY) == pytest.approx(1.070)
    assert VP.age_factor("F", 50.0, VP.FALLBACK_AGE_YOY) == pytest.approx(1.0)   # clamps to the last bin
    assert VP.age_factor("G", 20.0) == 1.0 and VP.age_factor("F", None) == 1.0
    # the packaged file and the fallbacks agree (fallbacks are the same fit, rounded)
    for g in ("F", "D"):
        for a, v in VP.age_yoy()[g].items():
            assert v == pytest.approx(VP.FALLBACK_AGE_YOY[g][a], abs=6e-4)
    bad = {"preseason": {"k_multi": {"F": "x", "D": -1, "G": 50}}, "inseason": {"recency_weights": {"season": 2}}}
    assert VP.k_baseline(bad) == {"F": 8.0, "D": 14.0, "G": 50.0}
    assert VP.recency_weights(bad) == VP.FALLBACK_RECENCY


def test_params_file_is_package_data():
    import tomllib
    from pathlib import Path
    cfg = tomllib.loads((Path(__file__).parents[1] / "pyproject.toml").read_text(encoding="utf-8"))
    assert "valuation/fitted_params.json" in cfg["tool"]["setuptools"]["package-data"]["fantasy_manager"]
