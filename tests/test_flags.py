from datetime import date

import pytest

from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig,
                                    StatLine)
from fantasy_manager.recommend.flags import recommend_flags
from fantasy_manager.valuation.blend import K_INSEASON
from fantasy_manager.valuation.valuate import PlayerValue

WEIGHTS = {"G": 1.0, "A": 1.0}


def line(split, gp, g_pg, a_pg, sog_pg):
    return StatLine(split=split, gp=gp, stats={"G": g_pg * gp, "A": a_pg * gp, "SOG": sog_pg * gp,
                                                "GP": gp})


def skater(cid, l15=None, status="healthy", season_gp=30):
    """Baseline 1.0 FPG (0.4 G + 0.6 A), 4 SOG/GP -> 10% shooting in season and projection."""
    lines = {"season": line("season", season_gp, 0.4, 0.6, 4.0),
             "projected": line("projected", 82, 0.4, 0.6, 4.0)}
    if l15:
        lines["last15"] = line("last15", *l15)
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=["C"],
                  status=status, lines=lines)


def val(p, vorp=1.0):
    return PlayerValue(player=p, fpg=1.0, fpg_season=1.0, fpg_week=1.0, vorp=vorp, vorp_week=vorp,
                       horizon_values={"season": 1.0, "week": 1.0, "vorp_season": vorp,
                                       "vorp_week": vorp})


# L15 shapes: (gp, G/gp, A/gp, SOG/gp)
HOT_LUCKY = (6, 0.6, 0.9, 3.0)     # 1.5 FPG (ratio 1.5), 20% shooting vs 10% baseline
HOT_REAL = (6, 0.6, 0.9, 6.0)      # 1.5 FPG but 10% shooting on more shots
COLD_STABLE = (6, 0.1, 0.4, 4.0)   # 0.5 FPG, shots unchanged
COLD_SHOTS_DOWN = (6, 0.1, 0.4, 2.0)


def build(mine, theirs, fas=()):
    def team(tid, name, me, ps):
        return FantasyTeam(team_id=tid, name=name, owner_is_me=me,
                           slots=[RosterSlot(slot="C", player=p, starting=True) for p in ps])
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights=WEIGHTS),
                        roster_shape={"C": 4, "BN": 2},
                        teams=[team("1", "Mine", True, mine), team("2", "Rivals", False, theirs)],
                        free_agents=list(fas), matchup_period=1, as_of=date(2026, 11, 1))
    values = {p.cid: val(p) for p in ctx.all_players()}
    return ctx, values


def by_title(recs):
    return {(r.kind, (r.add or r.drop)[0].cid): r for r in recs}


def test_sell_high_fires_on_lucky_hot_streak():
    ctx, values = build([skater("lucky", HOT_LUCKY), skater("real", HOT_REAL)], [])
    recs = by_title(recommend_flags(ctx, values))
    assert set(recs) == {("sell_high", "lucky")}          # sustained (shot-driven) form is not flagged
    r = recs[("sell_high", "lucky")]
    reasons = {x.code: x for x in r.reasons}
    assert reasons["FORM_RATIO"].value == pytest.approx(1.5)
    assert reasons["SHOOTING_PCT"].value == pytest.approx(0.20)
    assert reasons["SHOOTING_PCT"].baseline == pytest.approx(0.10)
    assert reasons["GP_L15"].value == 6
    assert r.drop[0].cid == "lucky" and r.counterparty is None and r.score > 0


def test_sell_high_only_for_my_players():
    ctx, values = build([], [skater("theirs_lucky", HOT_LUCKY)])
    assert recommend_flags(ctx, values) == []


def test_buy_low_only_for_other_teams_players():
    ctx, values = build([skater("my_cold", COLD_STABLE)],
                        [skater("their_cold", COLD_STABLE), skater("their_shots_down", COLD_SHOTS_DOWN),
                         skater("their_hurt", COLD_STABLE, status="out")],
                        fas=[skater("fa_cold", COLD_STABLE)])
    recs = recommend_flags(ctx, values)
    assert [(r.kind, r.add[0].cid) for r in recs] == [("buy_low", "their_cold")]
    r = recs[0]
    assert r.counterparty == "Rivals"
    reasons = {x.code: x for x in r.reasons}
    assert reasons["FORM_RATIO"].value == pytest.approx(0.5)
    assert reasons["SOG_RATE"].value == pytest.approx(4.0) and reasons["SOG_RATE"].baseline == pytest.approx(4.0)
    assert {"GP_L15", "SHOOTING_PCT"} <= set(reasons)


def test_small_l15_sample_is_skipped():
    ctx, values = build([skater("lucky", (4, 0.6, 0.9, 3.0))], [skater("cold", (4, 0.1, 0.4, 4.0))])
    assert recommend_flags(ctx, values) == []


def test_buy_low_skips_replacement_level_players():
    ctx, values = build([], [skater("scrub", COLD_STABLE)])
    values["scrub"] = val(values["scrub"].player, vorp=-0.2)
    assert recommend_flags(ctx, values) == []


def test_form_ratio_uses_shrunk_season_baseline():
    # season is hot (1.5 FPG over 10 GP) but shrinks toward the 1.0 projection: base = 1.1667
    p = skater("x", (6, 0.9, 1.2, 3.0), season_gp=10)          # L15: 2.1 FPG, 30% shooting
    p.lines["season"] = line("season", 10, 0.6, 0.9, 4.0)      # 15% shooting baseline
    ctx, values = build([p], [])
    recs = recommend_flags(ctx, values)
    ratio = {x.code: x for x in recs[0].reasons}["FORM_RATIO"].value
    k = K_INSEASON["skater"]                                   # 25 (was 20)
    assert ratio == pytest.approx(2.1 / ((10 * 1.5 + k * 1.0) / (10 + k)))
    # the unshrunk ratio (2.1 / 1.5 = 1.4) would overstate how far ahead of baseline he is
    p2 = skater("y", HOT_LUCKY, season_gp=10)
    p2.lines["season"] = line("season", 10, 0.6, 0.9, 4.0)
    ctx2, values2 = build([p2], [])
    assert recommend_flags(ctx2, values2) == []                 # 1.5 / 1.167 = 1.29 < 1.35


def test_flag_predicted_gain_is_regression_to_baseline():
    ctx, values = build([skater("lucky", HOT_LUCKY)], [skater("cold", COLD_STABLE)])
    recs = by_title(recommend_flags(ctx, values))
    sell, buy = recs[("sell_high", "lucky")], recs[("buy_low", "cold")]
    assert sell.gain_units == buy.gain_units == "season_fpg"
    assert sell.horizon_days == buy.horizon_days == 28
    assert sell.predicted_gain < 0 < buy.predicted_gain           # hot form expected to fall, cold to recover
    base = next(x.text for x in sell.reasons if x.code == "FORM_RATIO")
    assert sell.predicted_gain == pytest.approx(1.0 - 1.5, abs=0.2) and "baseline" in base
    assert sell.strength is not None and sell.strength > 4.0      # |1.5 - 1| = 0.5 -> between 4 and 7
