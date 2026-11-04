"""Goalie save talent from MoneyPuck GSAx (regression.regressed_gsax -> xg_enrich -> valuate,
reason GSAX), the MoneyPuck header guard, START_ACTUAL on the projection path and the optional
signal warnings."""
import csv
import io
from datetime import date
from pathlib import Path

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers import xg_enrich
from fantasy_manager.providers.moneypuck import (GOALIE_GSAX_COLUMNS, MoneyPuckClient, MpGoalie, goalie_calibration,
                                                 missing_columns)
from fantasy_manager.providers.xg_enrich import enrich_xg, goalie_gsax, register_gsax
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation import valuate as V
from fantasy_manager.valuation.regression import (GSAX_K_SHOTS, GSAX_MIN_SHOTS, GoalieGsax, gsax_rates,
                                                  regressed_gsax)
from fantasy_manager.valuation.valuate import valuate_league

FIX = Path(__file__).parent / "fixtures" / "moneypuck"
GOALIES_2025 = (FIX / "goalies_2025_head.csv").read_text(encoding="utf-8")
SKATERS_2025 = (FIX / "skaters_2025_head.csv").read_text(encoding="utf-8")
GOALIES_HEADER = GOALIES_2025.splitlines()[0] + "\n"
SHESTERKIN = 8478048
AS_OF = date(2026, 10, 20)          # 2026-27 season
W = {"W": 3.0, "SV": 0.2, "GA": -1.0, "SO": 2.0}


@pytest.fixture(autouse=True)
def _clean_registry():
    xg_enrich.clear_registry()
    yield
    xg_enrich.clear_registry()


class FakeFetch:
    def __init__(self, files):
        self.files, self.calls = files, []

    def __call__(self, url, params=None):
        self.calls.append(url)
        for key, text in self.files.items():
            if url.endswith(key):
                return text
        raise RuntimeError(f"404 {url}")


def drop_column(text, col):
    rows = list(csv.DictReader(io.StringIO(text)))
    fields = [f for f in rows[0].keys() if f != col]
    out = io.StringIO()
    w = csv.DictWriter(out, fieldnames=fields, lineterminator="\n", extrasaction="ignore")
    w.writeheader()
    w.writerows(rows)
    return out.getvalue()


def goalie(cid, nhl=None, gp=60, svpct=0.905, split="prior", team="EDM", gs=None, projected=None):
    sa = 30.0 * gp
    sv = svpct * sa
    stats = {"GS": gp if gs is None else gs, "SA": sa, "SV": sv, "GA": sa - sv, "W": 0.5 * gp, "SO": 0.05 * gp,
             "SVPCT": svpct}
    lines = {split: StatLine(split=split, gp=gp, stats=stats)}
    if projected is not None:
        lines["projected"] = projected
    return Player(cid=cid, name=cid, name_norm=cid, ids={"nhl": str(nhl)} if nhl else {}, team=team,
                  positions=["G"], lines=lines)


def ctx_of(players, as_of=AS_OF, dynasty=False):
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in players]
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights=W), roster_shape={"G": 2, "BN": 10},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=[], matchup_period=1, as_of=as_of, dynasty=dynasty)


def est(nhl, per_100, shots=4000.0, xsv=0.900, league=0.900):
    return GoalieGsax(nhl_id=nhl, seasons=(2025,), gsax=per_100 / 100 * (shots + GSAX_K_SHOTS), shots=shots,
                      raw_gsax=per_100 / 100 * (shots + GSAX_K_SHOTS), raw_shots=shots, xsv_pct=xsv,
                      per_shot=per_100 / 100, shots_per_game=28.0, league_sv_pct=league)


def codes(pv):
    return {r.code: r for r in pv.reasons}


# --------------------------------------------------------------------------- the model

def test_regressed_gsax_math():
    # current (1.0), prior (0.8), prior2 (0.6): (year, calibrated xGA, GA, shots, GP, league sv)
    seasons = [(2026, 30.0, 25.0, 300.0, 10, 0.90), (2025, 150.0, 130.0, 1500.0, 50, 0.90),
               (2024, 140.0, 140.0, 1400.0, 48, 0.91)]
    e = regressed_gsax(1, seasons)
    wg = 1.0 * 5 + 0.8 * 20 + 0.6 * 0
    ws = 300 + 0.8 * 1500 + 0.6 * 1400
    assert e.gsax == pytest.approx(wg) and e.shots == pytest.approx(ws)
    assert e.per_shot == pytest.approx(wg / (ws + 3000))
    assert e.raw_gsax == pytest.approx(25.0) and e.raw_shots == pytest.approx(3200.0)
    assert e.xsv_pct == pytest.approx(1 - (30 + 0.8 * 150 + 0.6 * 140) / ws)
    assert e.league_sv_pct == pytest.approx((300 * 0.90 + 0.8 * 1500 * 0.90 + 0.6 * 1400 * 0.91) / ws)
    assert e.seasons == (2026, 2025, 2024) and e.shots_per_game == pytest.approx(30.0)
    assert e.usable
    # the current season's slot is kept even when empty: last season still weighs 0.8
    e2 = regressed_gsax(1, [(2026, 0, 0, 0, 0), (2025, 150.0, 130.0, 1500.0, 50)])
    assert e2.per_shot == pytest.approx(0.8 * 20 / (0.8 * 1500 + 3000)) and e2.league_sv_pct is None
    assert regressed_gsax(1, [(2026, 0, 0, 0, 0)]) is None
    small = regressed_gsax(1, [(2026, 5.0, 3.0, 200.0, 7)])
    assert small.shots < GSAX_MIN_SHOTS and not small.usable


def test_sv_anchoring_and_rates():
    e = est(1, +0.5, xsv=0.902, league=0.896)
    # MoneyPuck scale: xsv + gsax; league scale: league + (xsv - mp league) + gsax
    assert e.sv_pct == pytest.approx(0.907)
    assert e.sv_pct_on(0.900) == pytest.approx(0.900 + 0.006 + 0.005)
    assert e.sv_pct_on(None) == e.sv_pct
    rates = {"SA": 30.0, "SV": 27.6, "GA": 2.4, "SVPCT": 0.92, "GAA": 2.4, "W": 0.5}
    new, before, after = gsax_rates(rates, e, 0.900)
    assert before == pytest.approx(0.92) and after == pytest.approx(0.911)
    assert new["SV"] == pytest.approx(30 * 0.911) and new["GA"] == pytest.approx(30 * 0.089)
    assert new["SVPCT"] == pytest.approx(0.911) and new["GAA"] == pytest.approx(2.4 * 0.089 / 0.08)
    assert new["W"] == 0.5 and "SO" not in new
    # 91.1-style percentages stay on their scale; no save stat -> unchanged
    assert gsax_rates({"SA": 30.0, "SVPCT": 92.0}, e, 0.9)[0]["SVPCT"] == pytest.approx(91.1)
    assert gsax_rates({"W": 0.5}, e)[1] is None


def test_goalie_calibration():
    rows = [MpGoalie(nhl_id=i, season=2025, name="g", team="T", situation="all", gp=50, toi_min=3000.0,
                     xga=150.0, ga=135.0, sa=1500.0) for i in range(20)]
    assert goalie_calibration(rows) == pytest.approx(0.9)          # 135/150, at the lower clamp
    assert goalie_calibration(rows[:3]) == 1.0                       # too few shots to calibrate
    rows[0].ga = 0.0
    assert goalie_calibration(rows) == pytest.approx(0.9)


# --------------------------------------------------------------------------- enrichment

def test_enrich_registers_goalie_gsax():
    g = goalie("g", SHESTERKIN)
    ctx = ctx_of([g])
    fetch = FakeFetch({"/2026/regular/goalies.csv": GOALIES_HEADER, "/2025/regular/goalies.csv": GOALIES_2025,
                       "/2026/regular/skaters.csv": SKATERS_2025, "/2025/regular/skaters.csv": SKATERS_2025})
    res = enrich_xg(ctx, client=MoneyPuckClient(fetch))
    e = res.goalies["g"]
    assert goalie_gsax(g, 2026) is e and goalie_gsax(g, 2025) is None
    # one goalie in the file: no calibration (1.0); 2025 weighted 0.8
    assert e.seasons == (2025,)
    assert e.gsax == pytest.approx(0.8 * 21.25) and e.shots == pytest.approx(0.8 * 1425)
    assert e.per_shot == pytest.approx(0.8 * 21.25 / (0.8 * 1425 + 3000))
    assert e.league_sv_pct == pytest.approx(1 - 126 / 1425)
    assert any("goalies 2024-25" in w for w in ctx.warnings)       # missing season: a warning, not a crash
    assert any("goalie GSAx for 1 goalies" in n for n in ctx.source_notes)
    # a second run replaces the season's registry
    enrich_xg(ctx_of([goalie("other", 1)]), client=MoneyPuckClient(fetch))
    assert goalie_gsax(g, 2026) is None


def test_goalies_not_fetched_without_goalies():
    fetch = FakeFetch({"/2026/regular/skaters.csv": SKATERS_2025, "/2025/regular/skaters.csv": SKATERS_2025})
    enrich_xg(ctx_of([goalie("g")]), client=MoneyPuckClient(fetch))       # no NHL id
    assert not any("goalies" in u for u in fetch.calls)


# --------------------------------------------------------------------------- header guard

def test_missing_columns_helper():
    assert missing_columns(GOALIES_2025, GOALIE_GSAX_COLUMNS) == []
    assert missing_columns(drop_column(GOALIES_2025, "xGoals"), GOALIE_GSAX_COLUMNS) == ["xGoals"]
    assert missing_columns("", GOALIE_GSAX_COLUMNS) == []
    assert missing_columns(GOALIES_HEADER, GOALIE_GSAX_COLUMNS) == []


def test_header_guard_skips_signals_with_a_warning():
    from fantasy_manager.models import normalize_name

    laf = Player(cid="1", name="Sam Lafferty", name_norm=normalize_name("Sam Lafferty"), ids={"nhl": "8478043"},
                 team=None, positions=["C"])
    g = goalie("g", SHESTERKIN)
    ctx = ctx_of([laf, g])
    fetch = FakeFetch({"/2026/regular/skaters.csv": "",
                       "/2025/regular/skaters.csv": drop_column(SKATERS_2025, "I_F_xGoals"),
                       "/2026/regular/goalies.csv": "", "/2025/regular/goalies.csv": drop_column(GOALIES_2025, "xGoals"),
                       "/2024/regular/goalies.csv": ""})
    res = enrich_xg(ctx, client=MoneyPuckClient(fetch))
    assert laf.ixg_per_game is None and laf.goals_minus_ixg is None      # not read as 0
    assert laf.onice_sh_pct is not None                                  # the on-ice signal still works
    assert res.filled == {"1": 2025}
    assert "g" not in res.goalies and goalie_gsax(g, 2026) is None
    warn = " | ".join(ctx.warnings)
    assert "2025-26 skaters.csv is missing column(s) I_F_xGoals" in warn and "XG_SHRINK" in warn
    assert "2025-26 goalies.csv is missing column(s) xGoals" in warn and "goalie GSAx" in warn
    assert len(res.missing) == 2


# --------------------------------------------------------------------------- valuation

def test_gsax_replaces_the_raw_save_component():
    good, bad, plain = goalie("good", 1), goalie("bad", 2), goalie("plain", 3)
    fillers = [goalie(f"f{i}", 10 + i) for i in range(3)]
    register_gsax(2026, est(1, +0.6))
    register_gsax(2026, est(2, -0.6))
    vals = valuate_league(ctx_of([good, bad, plain, *fillers]), PointsScoring(W))
    g, b, p = vals["good"], vals["bad"], vals["plain"]
    assert "GSAX" in codes(g) and "GSAX" in codes(b) and "GSAX" not in codes(p)
    assert g.rates["GA"] < p.rates["GA"] < b.rates["GA"]
    assert g.fpg > p.fpg > b.fpg
    r = codes(g)["GSAX"]
    assert r.value == pytest.approx(0.006) and r.baseline == pytest.approx(0.905, abs=1e-3)
    assert "MoneyPuck.com" in r.text and "+0.60 per 100 shots" in r.text
    # league save % 0.905 (every goalie's prior line) + 0 shot quality + 0.006
    assert g.rates["SVPCT"] == pytest.approx(0.911)
    assert g.rates["SV"] + g.rates["GA"] == pytest.approx(30.0)
    assert g.rates["W"] == p.rates["W"]                                  # wins untouched


def test_gsax_does_not_double_count_a_hot_raw_save_pct():
    # a .940 raw save % with average GSAx is valued as average: the raw component is replaced
    hot = goalie("hot", 1, svpct=0.940)
    fillers = [goalie(f"f{i}", 10 + i, svpct=0.900) for i in range(4)]
    register_gsax(2026, est(1, 0.0))
    v = valuate_league(ctx_of([hot, *fillers]), PointsScoring(W))["hot"]
    mean_sv = (0.940 + 4 * 0.900) / 5
    assert v.rates["SVPCT"] == pytest.approx(mean_sv)
    assert codes(v)["GSAX"].baseline > v.rates["SVPCT"]


def test_gsax_needs_enough_shots_and_applies_preseason_projection_value():
    small = goalie("small", 5)
    register_gsax(2026, est(5, +1.0, shots=300.0))
    assert "GSAX" not in codes(valuate_league(ctx_of([small]), PointsScoring(W))["small"])
    # a projected goalie before his season: the projection's FPts per team game carries the change
    proj = StatLine(split="projected", gp=60, stats={"GP": 60, "GS": 58, "SA": 1700, "SV": 1550, "GA": 150,
                                                    "W": 32, "SO": 3, "FPTS": 400})
    g = goalie("g", 6, projected=proj)
    base = valuate_league(ctx_of([g]), PointsScoring(W))["g"]
    register_gsax(2026, est(6, +0.6))
    up = valuate_league(ctx_of([g]), PointsScoring(W))["g"]
    assert "GSAX" in codes(up)
    shift = up.fpg - base.fpg
    assert shift > 0
    assert up.fpg_season == pytest.approx(base.fpg_season + shift * base.start_share)


# --------------------------------------------------------------------------- START_ACTUAL (projection path)

def test_ledger_starts_blend_into_a_projected_share():
    proj = StatLine(split="projected", gp=60, stats={"GP": 60, "GS": 60, "SA": 1700, "SV": 1550, "GA": 150, "W": 32})
    g = goalie("g", gp=8, split="season", gs=8, projected=proj)
    ctx = ctx_of([g])
    plain = valuate_league(ctx, PointsScoring(W))["g"]
    assert plain.share_source == "projection" and "START_ACTUAL" not in codes(plain)
    ctx.goalie_actual_starts = {"g": (2, 10)}               # ledger: 2 starts in 10 team games
    v = valuate_league(ctx, PointsScoring(W))["g"]
    expect = (2 + 10 * (60 / 82)) / (10 + 10)
    assert v.start_share == pytest.approx(expect)
    assert v.share_source == "actual" and "START_ACTUAL" in codes(v) and "START_SHARE" not in codes(v)
    assert v.start_share < plain.start_share
    assert v.fpg_season == pytest.approx(v.fpg * expect)
    assert V.goalie_base_share(g, ctx, V.team_games_played(ctx)) == (pytest.approx(expect), True)
    ctx.goalie_actual_starts = {"g": (2, 4)}                # < 5 ledger games: ignored
    assert "START_ACTUAL" not in codes(valuate_league(ctx, PointsScoring(W))["g"])


def test_ledger_starts_scale_a_preseason_projection_value():
    proj = StatLine(split="projected", gp=60, stats={"GP": 60, "GS": 41, "W": 25, "FPTS": 328})
    g = Player(cid="g", name="g", name_norm="g", ids={}, team="EDM", positions=["G"], lines={"projected": proj})
    ctx = ctx_of([g])
    ctx.goalie_actual_starts = {"g": (0, 6)}                # backup who has not started yet
    v = valuate_league(ctx, PointsScoring(W))["g"]
    expect = (0 + 10 * 0.5) / (6 + 10)
    assert v.start_share == pytest.approx(expect)
    assert v.fpg_season == pytest.approx(328 / 82 * expect / 0.5)


# --------------------------------------------------------------------------- silent failures -> warnings

def test_failed_optional_signals_warn_once(monkeypatch):
    def boom(*a, **k):
        raise RuntimeError("kaput")

    monkeypatch.setattr(V, "teammate_shares", boom)
    monkeypatch.setattr("fantasy_manager.providers.preseason_enrich.preseason_lines", boom)
    monkeypatch.setattr("fantasy_manager.providers.rookie_enrich.signals_for", boom)
    monkeypatch.setattr("fantasy_manager.providers.xg_enrich.goalie_gsax", boom)
    ctx = ctx_of([goalie("a", 1), goalie("b", 2)])
    vals = valuate_league(ctx, PointsScoring(W))
    assert set(vals) == {"a", "b"}                          # values still computed
    for sig in ("teammate goalie shares", "preseason lines", "news role signals", "goalie GSAx"):
        hits = [w for w in ctx.warnings if w.startswith(f"Valuation: {sig} unavailable")]
        assert len(hits) == 1, sig                          # one per signal, not per player
        assert "RuntimeError: kaput" in hits[0]
    valuate_league(ctx, PointsScoring(W))
    assert len([w for w in ctx.warnings if "teammate goalie shares" in w]) == 1


def test_dynasty_map_failure_warns(monkeypatch):
    from fantasy_manager.recommend.base import dynasty_map

    def boom(*a, **k):
        raise ValueError("no ages")

    monkeypatch.setattr("fantasy_manager.valuation.dynasty.apply_dynasty", boom)
    ctx = ctx_of([goalie("a", 1)], dynasty=True)
    assert dynasty_map(ctx, {}) is None
    assert dynasty_map(ctx, {}) is None
    hits = [w for w in ctx.warnings if w.startswith("Dynasty values unavailable")]
    assert len(hits) == 1 and "ValueError: no ages" in hits[0]
