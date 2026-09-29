import pytest

from fantasy_manager.models import StatLine
from fantasy_manager.valuation.blend import (K_BASELINE, K_INSEASON, blend_rates, blend_recency,
                                             multi_season_baseline, multi_season_sample, projection_blend_weight,
                                             recency_weights, shrunk_rates)

K_SKATER, K_GOALIE = K_INSEASON["skater"], K_INSEASON["goalie"]

PROJ = StatLine(split="projected", gp=82, stats={"G": 41.0, "A": 41.0, "GP": 82})


def cur(gp, g_per_game):
    return StatLine(split="season", gp=gp, stats={"G": g_per_game * gp, "A": 0.5 * gp, "GP": gp})


def test_gp_zero_returns_prior():
    assert shrunk_rates(cur(0, 1.0), PROJ, False) == pytest.approx(PROJ.per_game())
    assert shrunk_rates(None, PROJ, False) == pytest.approx({"G": 0.5, "A": 0.5})


def test_no_prior_returns_current():
    assert shrunk_rates(cur(10, 1.0), None, False) == pytest.approx({"G": 1.0, "A": 0.5})


def test_shrink_amounts_gp5_vs_gp40():
    # current 1.0 G/GP vs prior 0.5 G/GP, in-season k = 25
    r5 = shrunk_rates(cur(5, 1.0), PROJ, False)["G"]
    r40 = shrunk_rates(cur(40, 1.0), PROJ, False)["G"]
    assert r5 == pytest.approx((5 * 1.0 + K_SKATER * 0.5) / (5 + K_SKATER))  # 0.583
    assert r40 == pytest.approx((40 * 1.0 + K_SKATER * 0.5) / (40 + K_SKATER))  # 0.808
    assert r5 < r40 < 1.0
    # A is identical in both lines, so it never moves
    assert shrunk_rates(cur(5, 1.0), PROJ, False)["A"] == pytest.approx(0.5)


def test_inseason_k_is_fitted_and_goalies_shrink_more():
    assert (K_SKATER, K_GOALIE) == (25.0, 40.0)
    assert K_BASELINE == {"F": 8.0, "D": 14.0, "G": 120.0}


def test_goalie_uses_goalie_k():
    prior = StatLine(split="prior", gp=50, stats={"W": 25.0, "GP": 50})
    now = StatLine(split="season", gp=6, stats={"W": 6.0, "GP": 6})
    assert shrunk_rates(now, prior, True)["W"] == pytest.approx((6 * 1.0 + K_GOALIE * 0.5) / (6 + K_GOALIE))


def test_stat_missing_on_one_side_borrows_other():
    now = StatLine(split="season", gp=10, stats={"G": 5.0, "HIT": 30.0, "GP": 10})
    r = shrunk_rates(now, PROJ, False)
    assert r["HIT"] == pytest.approx(3.0)


@pytest.mark.parametrize("gps", [(0, 0, 0), (13, 7, 4), (5, 3, 1), (30, 15, 7), (2, 2, 2)])
def test_recency_weights_sum_to_one(gps):
    w = recency_weights(*gps)
    assert sum(w.values()) == pytest.approx(1.0)
    assert all(v >= 0 for v in w.values())


def test_recency_reallocation():
    assert recency_weights(0, 0, 0) == pytest.approx({"season": 1.0, "last30": 0, "last15": 0, "last7": 0})
    full = recency_weights(13, 7, 4)
    assert full == pytest.approx({"season": 0.85, "last30": 0.15, "last15": 0.0, "last7": 0.0})
    half = recency_weights(6.5, 6.5, 3.3)
    assert half["last30"] == pytest.approx(0.075)
    assert half["season"] == pytest.approx(0.85 + 0.075)


def test_blend_recency_values():
    season = {"G": 0.5}
    l30 = StatLine(split="last30", gp=13, stats={"G": 13.0, "GP": 13})  # 1.0 G/GP
    out = blend_recency(season, l30, None, None)
    assert out["G"] == pytest.approx(0.85 * 0.5 + 0.15 * 1.0)
    assert blend_recency(season, None, None, None) == pytest.approx(season)


def _line(split, gp, g):
    return StatLine(split=split, gp=gp, stats={"G": g * gp, "GP": gp})


def test_multi_season_sample_weights_5_4_3_by_gp():
    lines = [_line("prior", 60, 0.9), _line("prior2", 80, 0.6), _line("prior3", 40, 0.3)]
    rates, n, gp = multi_season_sample(lines)
    wg = 5 * 60 + 4 * 80 + 3 * 40
    assert rates["G"] == pytest.approx((5 * 60 * 0.9 + 4 * 80 * 0.6 + 3 * 40 * 0.3) / wg)
    assert n == pytest.approx(wg / 5) and gp == 180
    assert multi_season_sample([None, None, None]) is None
    # a skipped season is simply missing
    r2, n2, _ = multi_season_sample([None, _line("prior2", 80, 0.6), None])
    assert r2["G"] == pytest.approx(0.6) and n2 == pytest.approx(80 * 4 / 5)


def test_multi_season_stat_only_some_seasons_carry_is_not_diluted():
    a = StatLine(split="prior", gp=50, stats={"G": 10.0, "HAT": 1.0})
    b = StatLine(split="prior2", gp=80, stats={"G": 40.0})
    rates, _, _ = multi_season_sample([a, b])
    assert rates["HAT"] == pytest.approx(1.0 / 50)


def test_multi_season_baseline_shrinks_by_group_k():
    lines = [_line("prior", 30, 0.8)]
    means = {"F": {"G": 0.2}, "D": {"G": 0.2}, "G": {"G": 0.0}}
    assert multi_season_baseline(lines, "F", means)["G"] == pytest.approx((30 * 0.8 + 8 * 0.2) / 38)
    assert multi_season_baseline(lines, "D", means)["G"] == pytest.approx((30 * 0.8 + 14 * 0.2) / 44)
    assert multi_season_baseline(lines, "F", means, k=0)["G"] == pytest.approx(0.8)
    assert multi_season_baseline(lines, "F", {})["G"] == pytest.approx(0.8)          # no mean: raw
    assert multi_season_baseline([None], "F", means) == {}


def test_projection_blend_weight_ramps_to_60_40():
    assert projection_blend_weight(0) == 1.0                 # rookie: projection only
    assert projection_blend_weight(40) == pytest.approx(0.6)
    assert projection_blend_weight(300) == pytest.approx(0.6)
    assert projection_blend_weight(20) == pytest.approx(0.8)
    assert blend_rates({"G": 1.0, "HIT": 2.0}, {"G": 0.5}, 0.6) == pytest.approx({"G": 0.8, "HIT": 2.0})
