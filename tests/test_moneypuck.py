"""MoneyPuck provider, xG enrichment, luck signals and the xG backtest helpers.

Fixtures are trimmed copies of MoneyPuck's public season summaries (moneypuck.com/data.htm,
free for non-commercial use with credit): the header plus the first rows of the 2025-26
skaters / goalies files, and the header-only 2026-27 skaters file as published before opening
night.
"""
import csv
import io
import math
from datetime import date
from pathlib import Path

import pytest

from fantasy_manager.backtest.data import PlayerSeason
from fantasy_manager.backtest.xg_backtest import (XgSeason, XgShrunk, adjust_row, arm_name, headline,
                                                  ProjRow, shooting_pairs, shooting_summary)
from fantasy_manager.cache import HttpCache
from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine,
                                    normalize_name)
from fantasy_manager.providers import moneypuck as mp
from fantasy_manager.providers.moneypuck import (CREDIT, MoneyPuckClient, MpSkater, cached_fetch_text,
                                                 onice_sh_norms, parse_goalies, parse_skaters, season_url,
                                                 ttl_for_url, xg_calibration)
from fantasy_manager.providers.xg_enrich import enrich_xg
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.regression import (DEFAULT_ONICE_SH_PCT, K_GOALS, LuckSignals, league_weights,
                                                  luck_signals, regressed_rates, shrink_goals, xg_sample)

FIX = Path(__file__).parent / "fixtures" / "moneypuck"
SKATERS_2025 = (FIX / "skaters_2025_head.csv").read_text(encoding="utf-8")
GOALIES_2025 = (FIX / "goalies_2025_head.csv").read_text(encoding="utf-8")
SKATERS_2026 = (FIX / "skaters_2026_empty.csv").read_text(encoding="utf-8")

LAFFERTY, MITCHELL, GUDBRANSON = 8478043, 8484262, 8475790


class FakeFetch:
    def __init__(self, files):
        self.files = files
        self.calls = []

    def __call__(self, url, params=None):
        self.calls.append(url)
        for key, text in self.files.items():
            if url.endswith(key):
                return text
        raise AssertionError(f"unexpected url {url}")


def with_season(text, season, gp=None):
    """The fixture rewritten as another season (optionally with every games_played = gp)."""
    rows = list(csv.DictReader(io.StringIO(text)))
    out = io.StringIO()
    w = csv.DictWriter(out, fieldnames=list(rows[0].keys()), lineterminator="\n")
    w.writeheader()
    for r in rows:
        r["season"] = str(season)
        if gp is not None:
            r["games_played"] = str(gp)
        w.writerow(r)
    return out.getvalue()


# --------------------------------------------------------------------------- parsing

def test_parse_all_situation():
    rows = parse_skaters(SKATERS_2025)
    assert [r.nhl_id for r in rows] == [LAFFERTY, MITCHELL, GUDBRANSON]
    assert {r.situation for r in rows} == {"all"}
    r = rows[0]
    assert (r.name, r.team, r.position, r.season, r.gp) == ("Sam Lafferty", "CHI", "C", 2025, 29)
    assert r.toi_min == pytest.approx(14959 / 60)
    assert (r.ixg, r.goals, r.sog, r.ixg_adj, r.points) == (0.85, 1.0, 9.0, 0.83, 2.0)
    assert r.assists == 1.0
    assert r.onice_xg_pct == 0.39
    assert r.onice_sh_pct == pytest.approx(7 / 78)
    assert r.onice_sv_pct == pytest.approx(1 - 9 / 94)
    assert r.goals_minus_ixg == pytest.approx(0.15)
    assert r.ixg_per_game == pytest.approx(0.85 / 29)
    assert r.pp_toi_min is None
    assert rows[1].position == "D" and rows[1].group == "D" and r.group == "F"


def test_situation_filtering_and_pp():
    assert len(parse_skaters(SKATERS_2025, None)) == 15
    five = parse_skaters(SKATERS_2025, "5on5")
    assert {r.situation for r in five} == {"5on5"} and len(five) == 3
    assert five[2].onice_sh_pct == pytest.approx(19 / 203)
    pk = parse_skaters(SKATERS_2025, "4on5")
    assert pk[2].toi_min == pytest.approx(100.267, abs=1e-3)
    rows = parse_skaters(SKATERS_2025, "all", with_pp=True)
    assert rows[0].pp_toi_min == pytest.approx(0.3)
    assert rows[0].pp_ixg == 0.0 and rows[0].pp_goals == 0.0


def test_goalies():
    gs = parse_goalies(GOALIES_2025)
    assert len(gs) == 1
    g = gs[0]
    assert (g.name, g.team, g.gp, g.xga, g.ga, g.sa) == ("Igor Shesterkin", "NYR", 51, 147.25, 126.0, 1425.0)
    assert g.sv_pct == pytest.approx(1 - 126 / 1425)
    assert g.gsax == pytest.approx(21.25)
    assert g.xsv_pct < g.sv_pct
    assert [x.situation for x in parse_goalies(GOALIES_2025, None)] == ["other", "all", "5on5", "4on5", "5on4"]


def test_header_only_file_is_empty():
    assert parse_skaters(SKATERS_2026) == []
    assert parse_skaters(SKATERS_2026, None) == []


def test_client_urls_memo_and_situation_check():
    fetch = FakeFetch({"/2025/regular/skaters.csv": SKATERS_2025, "/2025/regular/goalies.csv": GOALIES_2025})
    c = MoneyPuckClient(fetch_text=fetch)
    assert len(c.skaters(2025)) == 3
    assert len(c.skaters(20252026, "5on5")) == 3          # NHL season ids accepted
    assert fetch.calls == ["https://moneypuck.com/moneypuck/playerData/seasonSummary/2025/regular/skaters.csv"]
    assert c.goalies(2025)[0].nhl_id == 8478048
    assert set(c.skaters_by_id(2025)) == {LAFFERTY, MITCHELL, GUDBRANSON}
    with pytest.raises(ValueError):
        c.skaters(2025, "5v5")


def test_ttl_and_cached_fetch(tmp_path):
    today = date(2026, 9, 29)
    assert ttl_for_url(season_url(2026), today) == mp.TTL_CURRENT
    assert ttl_for_url(season_url(2025), today) == mp.TTL_PAST
    assert ttl_for_url(season_url(2025), date(2026, 3, 1)) == mp.TTL_CURRENT

    class FakeCache:
        def __init__(self):
            self.calls = []

        def get_text(self, url, params=None, ttl=0):
            self.calls.append((url, ttl))
            return SKATERS_2025

    cache = FakeCache()
    MoneyPuckClient(cached_fetch_text(cache, today)).skaters(2018)
    assert cache.calls == [(season_url(2018), mp.TTL_PAST)]


def test_norms_and_calibration():
    rows = parse_skaters(SKATERS_2025, "5on5")
    n = onice_sh_norms(rows, min_toi_min=0)
    assert n["F"] == pytest.approx(7 / 78)
    assert n["D"] == pytest.approx((3 + 19) / (31 + 203))
    assert onice_sh_norms(rows, min_toi_min=200) == {"F": pytest.approx(7 / 78), "D": pytest.approx(19 / 203)}
    cal = xg_calibration(parse_skaters(SKATERS_2025))
    assert cal["F"] == pytest.approx(1 / 0.85)
    assert cal["D"] == pytest.approx(2 / (0.38 + 1.11))


# --------------------------------------------------------------------------- enrichment

def pl(cid, name, pos, nhl=None, lines=None):
    ids = {"espn": cid}
    if nhl is not None:
        ids["nhl"] = str(nhl)
    return Player(cid=cid, name=name, name_norm=normalize_name(name), ids=ids, team=None, positions=pos,
                  lines=lines or {})


def make_ctx(players, as_of=date(2026, 9, 29)):
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in players]
    return LeagueContext(provider="test", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 2.0, "A": 1.0}),
                         roster_shape={"BN": 5},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=[], matchup_period=1, as_of=as_of)


def players():
    return [pl("1", "Sam Lafferty", ["C"], LAFFERTY), pl("2", "Travis Mitchell", ["D"], MITCHELL),
            pl("3", "Erik Gudbranson", ["D"], GUDBRANSON), pl("4", "No Id", ["C"]),
            pl("5", "Igor Shesterkin", ["G"], 8478048)]


def test_enrich_prior_fallback_before_opening_night():
    ps = players()
    ctx = make_ctx(ps)
    fetch = FakeFetch({"/2026/regular/skaters.csv": SKATERS_2026, "/2025/regular/skaters.csv": SKATERS_2025})
    res = enrich_xg(ctx, client=MoneyPuckClient(fetch))
    assert res.seasons == (2026, 2025)
    assert res.rows == {2026: 0, 2025: 3}
    assert res.filled == {"1": 2025, "2": 2025, "3": 2025}
    assert res.from_prior == {"1", "2", "3"}
    laf = ps[0]
    assert laf.ixg_per_game == pytest.approx(0.85 / 29, abs=1e-4)
    assert laf.goals_minus_ixg == pytest.approx(0.15)
    assert laf.onice_sh_pct == pytest.approx(7 / 78, abs=1e-4)      # 5on5
    assert laf.onice_xg_pct == 0.4                                    # 5on5, not the all-situation 0.39
    assert ps[3].ixg_per_game is None and ps[4].ixg_per_game is None  # no NHL id / goalie
    assert any(CREDIT in n and "2025-26 for 3" in n for n in ctx.source_notes)
    assert res.onice_sh_norms


def test_enrich_current_season_and_small_sample():
    ps = players()
    ctx = make_ctx(ps)
    cur = with_season(SKATERS_2025, 2026)
    # Mitchell has 9 GP in the "current" file too; make him a 3-GP player to hit the fallback
    rows = list(csv.DictReader(io.StringIO(cur)))
    for r in rows:
        if r["playerId"] == str(MITCHELL):
            r["games_played"] = "3"
    out = io.StringIO()
    w = csv.DictWriter(out, fieldnames=list(rows[0].keys()), lineterminator="\n")
    w.writeheader()
    w.writerows(rows)
    prior = "\n".join(l for l in SKATERS_2025.splitlines() if not l.startswith(str(MITCHELL)))
    fetch = FakeFetch({"/2026/regular/skaters.csv": out.getvalue(), "/2025/regular/skaters.csv": prior})
    res = enrich_xg(ctx, client=MoneyPuckClient(fetch), seasons=(20262027, 20252026))
    assert res.filled == {"1": 2026, "2": 2026, "3": 2026}
    assert res.from_prior == set()
    assert res.small_sample == {"2"}          # < 5 GP now and no prior line
    assert ps[1].ixg_per_game == pytest.approx(0.38 / 3, abs=1e-4)


def test_enrich_failure_is_a_warning():
    ctx = make_ctx(players())

    def boom(url, params=None):
        raise RuntimeError("offline")

    res = enrich_xg(ctx, client=MoneyPuckClient(boom))
    assert res.count == 0 and len(res.errors) == 2
    assert any("MoneyPuck" in w for w in ctx.warnings)


def test_enrich_through_http_cache_offline(tmp_path):
    cache = HttpCache(tmp_path, offline=True)
    try:
        ctx = make_ctx(players())
        res = enrich_xg(ctx, cache)             # nothing cached + offline -> warnings, no crash
        assert res.count == 0 and ctx.warnings
    finally:
        cache.close()


# --------------------------------------------------------------------------- luck signals

def skater(pos, gp, g, a, ixg_pg, onice, prior=None, season=True):
    lines = {}
    if season:
        lines["season"] = StatLine(split="season", gp=gp, stats={"G": g, "A": a, "GP": gp})
    if prior:
        lines["prior"] = StatLine(split="prior", gp=prior[0], stats={"G": prior[1], "A": prior[2], "GP": prior[0]})
    p = pl("x", "Synthetic", pos, 1, lines)
    p.ixg_per_game = ixg_pg
    ref_gp = gp if season and gp >= 5 else (prior[0] if prior else gp)
    ref_g = g if season and gp >= 5 else (prior[1] if prior else g)
    p.goals_minus_ixg = ref_g - ixg_pg * ref_gp
    p.onice_sh_pct = onice
    return p


def test_luck_signal_math():
    # 40 GP, 20 G on 12 ixG, 20 A, on-ice 12% vs F norm 10%, ESPN-like weights G 2 / A 1
    p = skater(["C"], 40, 20.0, 20.0, 0.3, 0.12)
    s = luck_signals(p, {"F": 0.10}, PointsScoring({"G": 2.0, "A": 1.0}))
    assert isinstance(s, LuckSignals)
    assert s.gp == 40 and not s.from_prior
    assert s.goals_minus_ixg == pytest.approx(8.0)
    assert s.goals_per_game == pytest.approx(0.5)
    assert s.sh_pct_vs_xg_ratio == pytest.approx(20 / 12)
    assert s.goal_luck_fpg == pytest.approx(8 / 40 * 2)
    assert s.onice_sh_pct_delta == pytest.approx(0.02)
    assert s.assist_luck_fpg == pytest.approx(0.5 * (1 - 0.10 / 0.12) * 1.0)
    assert s.expected_regression_fpg == pytest.approx(s.goal_luck_fpg + s.assist_luck_fpg)
    assert s.confidence == pytest.approx(40 / 60)
    assert 0 < s.next_season_drop_fpg < s.expected_regression_fpg


def test_luck_signal_cold_player_and_weights():
    p = skater(["D"], 60, 3.0, 12.0, 0.1, 0.07)
    s = luck_signals(p, {"D": 0.09})
    assert s.goal_luck_fpg == pytest.approx(-3 / 60)          # category league: goals / game
    assert s.assist_luck_fpg < 0 and s.expected_regression_fpg < 0
    # clamp keeps a tiny on-ice sh% from exploding
    q = skater(["D"], 60, 3.0, 12.0, 0.1, 0.01)
    assert luck_signals(q, {"D": 0.09}).assist_luck_fpg == pytest.approx(0.2 * -1.0)
    assert league_weights({"G": 3.0, "A": 2.0, "PTS": 1.0}) == (4.0, 3.0)
    assert league_weights(ScoringConfig(kind="categories", categories=["G"])) == (1.0, 1.0)
    assert league_weights(None) == (1.0, 1.0)


def test_luck_signal_prior_season_and_missing():
    p = skater(["LW"], 2, 1.0, 0.0, 0.25, None, prior=(80, 30.0, 25.0))
    assert xg_sample(p) == (80, True, "prior")
    s = luck_signals(p, None)
    assert s.from_prior and s.gp == 80
    assert s.goals_minus_ixg == pytest.approx(10.0)
    assert s.onice_sh_pct_delta is None and s.assist_luck_fpg == 0.0
    assert s.confidence == pytest.approx(80 / 100 * 0.5)
    none = pl("y", "Nobody", ["C"], 2)
    assert luck_signals(none) is None
    g = pl("g", "Goalie", ["G"], 3)
    g.ixg_per_game, g.goals_minus_ixg = 0.0, 0.0
    assert luck_signals(g) is None
    # default norms are used without a mapping
    q = skater(["C"], 40, 20.0, 20.0, 0.3, 0.12)
    assert luck_signals(q).onice_sh_pct_delta == pytest.approx(0.12 - DEFAULT_ONICE_SH_PCT["F"])


def test_regressed_rates_monotone_in_gp():
    rates = {"G": 0.5, "A": 0.5, "PTS": 1.0, "SOG": 3.0}
    prev = None
    for gp in (1, 5, 10, 20, 40, 82, 500):
        s = LuckSignals(gp=gp, from_prior=False, ixg_per_game=0.3, goals_per_game=0.5, goals_minus_ixg=0.2 * gp,
                        sh_pct_vs_xg_ratio=5 / 3, onice_sh_pct_delta=None, goal_luck_fpg=0.2,
                        assist_luck_fpg=0.0, expected_regression_fpg=0.2, confidence=0.5)
        r = regressed_rates(rates, s)
        assert 0.3 < r["G"] < 0.5
        assert r["PTS"] == pytest.approx(1.0 + (r["G"] - 0.5))
        assert r["A"] == 0.5 and r["SOG"] == 3.0
        if prev is not None:
            assert r["G"] > prev                  # observed rate weighs more with games
        prev = r["G"]
    assert regressed_rates(rates, None) == rates
    assert shrink_goals(rates, 0.3, 15)["G"] == pytest.approx(0.4)   # k = 15: half way at 15 GP
    assert K_GOALS == 15.0
    assert shrink_goals(rates, 0.3, 15, math.inf)["G"] == 0.3
    assert shrink_goals({"A": 1.0}, 0.3, 15) == {"A": 1.0}
    # larger k shrinks more
    assert shrink_goals(rates, 0.3, 40, 60)["G"] < shrink_goals(rates, 0.3, 40, 15)["G"]


# --------------------------------------------------------------------------- backtest helpers

def mps(pid, season, gp, goals, ixg, sog, pos="C"):
    return MpSkater(nhl_id=pid, season=season, name="x", team="T", position=pos, situation="all", gp=gp,
                    toi_min=1000.0, ixg=ixg, goals=goals, sog=sog, ixg_adj=ixg)


def test_adjust_row_and_wrapper():
    row = PlayerSeason(player_id=7, season=20242025, name="x", group="F", position="C", gp=80,
                       stats={"G": 40.0, "A": 30.0, "PTS": 70.0, "SOG": 250.0})
    xg = {20242025: XgSeason({7: mps(7, 2024, 80, 40.0, 24.0, 250.0)}, {}, {"F": 1.1})}
    adj = adjust_row(row, xg, 20.0)
    assert adj.stats["G"] == pytest.approx((80 * 0.5 + 20 * 0.3) / 100 * 80)
    assert adj.stats["PTS"] == pytest.approx(70 - (40 - adj.stats["G"]))
    assert adj.stats["A"] == 30.0 and row.stats["G"] == 40.0          # original untouched
    cal = adjust_row(row, xg, 20.0, calibrate=True)
    assert cal.stats["G"] > adj.stats["G"]
    other = PlayerSeason(player_id=8, season=20242025, name="y", group="F", position="C", gp=80, stats={"G": 5.0})
    assert adjust_row(other, xg, 20.0) is other                           # no ixG row
    old = PlayerSeason(player_id=7, season=20102011, name="x", group="F", position="C", gp=80, stats={"G": 5.0})
    assert adjust_row(old, xg, 20.0) is old

    class Base:
        name = "naive"

        def rates(self, history, age, ctx):
            return history[-1].per_game()

    w = XgShrunk(Base(), xg, 20.0)
    assert w.name == "naive+xg20" == arm_name("naive", 20.0)
    assert w.rates([row], None, None)["G"] == pytest.approx(adj.stats["G"] / 80)
    assert arm_name("fm_current", math.inf, True) == "fm_current+xgcalixG"


def test_shooting_pairs_and_headline():
    xg = {20232024: XgSeason({1: mps(1, 2023, 80, 30, 20, 200), 2: mps(2, 2023, 80, 10, 18, 200),
                              3: mps(3, 2023, 80, 20, 20, 30)}, {}),
          20242025: XgSeason({1: mps(1, 2024, 80, 22, 20, 200), 2: mps(2, 2024, 80, 17, 18, 200),
                              3: mps(3, 2024, 80, 20, 20, 200)}, {})}
    pairs = shooting_pairs(xg, min_sog=50)
    assert len(pairs) == 2                                 # player 3 had < 50 SOG in N-1
    summ = shooting_summary(pairs)
    assert summ["n"] == 2 and summ["corr_gmx_vs_dsh"] == pytest.approx(-1.0)
    rows = [ProjRow("espn", 20242025, "skaters", "fm_current", 100, 0.20, 0.80, 0.0),
            ProjRow("espn", 20242025, "skaters", "fm_current+xg15", 100, 0.19, 0.81, 0.0)]
    h = headline(rows, "espn")
    assert h["d_mae"] == pytest.approx(-0.01) and h["mae_wins"] == 1 and h["spearman_wins"] == 1
