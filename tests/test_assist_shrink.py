"""In-season assist-luck shrink (valuate.assist_shrink, reason ASSIST_SHRINK)."""
from datetime import date

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers import xg_enrich
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.regression import (ASSIST_SHRINK_CAP, ASSIST_SHRINK_SHARE, DEFAULT_ONICE_SH_PCT,
                                                  assist_shrink_rates, luck_signals)
from fantasy_manager.valuation.valuate import assist_shrink, valuate_league

W = {"G": 2.0, "A": 1.0, "SOG": 0.2}
AS_OF = date(2026, 11, 20)


@pytest.fixture(autouse=True)
def _clean_registry():
    xg_enrich.clear_registry()
    yield
    xg_enrich.clear_registry()


def skater(cid, gp, a, onice, split="season", prior=True):
    lines = {}
    if gp:
        lines["season"] = StatLine(split="season", gp=gp, stats={"GP": gp, "G": 0.3 * gp, "A": a, "SOG": 2.5 * gp})
    if prior:
        lines["prior"] = StatLine(split="prior", gp=82, stats={"GP": 82, "G": 25.0, "A": 30.0, "SOG": 200.0})
    return Player(cid=cid, name=cid, name_norm=cid, ids={"nhl": "1"}, team="EDM", positions=["C"], lines=lines,
                  ixg_per_game=0.3, goals_minus_ixg=0.0, onice_sh_pct=onice, xg_split=split)


def ctx_of(players):
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in players]
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights=W), roster_shape={"C": 2, "BN": 10},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=[], matchup_period=1, as_of=AS_OF)


def value(p):
    return valuate_league(ctx_of([p]), PointsScoring(W))[p.cid]


def codes(pv):
    return {r.code: r for r in pv.reasons}


def test_assist_shrink_rates_math():
    p = skater("x", 20, 16.0, 0.12)
    sig = luck_signals(p, {"F": 0.10})
    luck = 0.8 * (1 - 0.10 / 0.12)                          # A/GP x clamp(1 - norm / on-ice)
    assert sig.assist_luck_fpg == pytest.approx(luck) and sig.confidence == pytest.approx(20 / 40)
    new, delta = assist_shrink_rates({"A": 0.7, "PTS": 1.0}, sig)
    assert delta == pytest.approx(-ASSIST_SHRINK_SHARE * luck * 0.5)
    assert new["A"] == pytest.approx(0.7 + delta) and new["PTS"] == pytest.approx(1.0 + delta)
    assert assist_shrink_rates({"A": 0.7}, None) == ({"A": 0.7}, 0.0)


def test_hot_on_ice_luck_lowers_assists_cold_raises_them():
    lucky = value(skater("lucky", 20, 16.0, 0.13))
    unlucky = value(skater("unlucky", 20, 16.0, 0.075))
    plain = value(skater("plain", 20, 16.0, None))           # no on-ice data: nothing to do
    assert "ASSIST_SHRINK" not in codes(plain)
    assert lucky.rates["A"] < plain.rates["A"] < unlucky.rates["A"]
    r = codes(lucky)["ASSIST_SHRINK"]
    assert r.value < 0 and r.baseline > 0 and "MoneyPuck" in r.text
    # the change in rate space: -0.35 x luck x confidence (default 5v5 norm without enrich norms)
    sig = luck_signals(skater("lucky", 20, 16.0, 0.13), None)
    assert r.value == pytest.approx(-0.35 * sig.assist_luck_fpg * sig.confidence, abs=1e-4)
    assert lucky.fpg == pytest.approx(plain.fpg + r.value * W["A"], abs=1e-4)


def test_never_preseason_before_ten_gp_or_from_last_season():
    for p in (skater("pre", 0, 0.0, 0.13), skater("nine", 9, 8.0, 0.13), skater("prior", 20, 16.0, 0.13, "prior")):
        assert "ASSIST_SHRINK" not in codes(value(p)), p.cid
    assert "ASSIST_SHRINK" in codes(value(skater("ten", 10, 8.0, 0.13)))


def test_cap_at_share_of_fpg():
    # a pure playmaker on a huge on-ice shooting %: the raw shrink would exceed the cap
    p = skater("x", 82, 120.0, 0.30, prior=False)
    p.lines["season"].stats.update({"G": 0.0, "SOG": 0.0})
    rates = {"A": 1.4}
    scoring = PointsScoring({"A": 1.0})
    new, why = assist_shrink(p, rates, scoring)
    raw = assist_shrink_rates(rates, luck_signals(p))[1]
    assert abs(raw) > ASSIST_SHRINK_CAP * 1.4                # the cap binds
    assert new["A"] == pytest.approx(1.4 * (1 - ASSIST_SHRINK_CAP))
    assert "capped" in why.text


def test_uses_registered_onice_norms():
    p = skater("x", 20, 16.0, 0.11)
    xg_enrich._NORMS[2026] = {"F": 0.11}                     # on-ice exactly at the season norm
    assert "ASSIST_SHRINK" not in codes(value(p))
    xg_enrich.clear_registry()
    assert DEFAULT_ONICE_SH_PCT["F"] < 0.11
    assert "ASSIST_SHRINK" in codes(value(p))
