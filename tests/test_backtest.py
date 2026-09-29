"""Backtest harness: data rows, models, metrics, fitting, archive. No network."""
import json
import math
from datetime import date
from pathlib import Path

import pytest
from rich.console import Console
from typer.testing import CliRunner

from fantasy_manager import cli_backtest
from fantasy_manager.backtest import archive as A
from fantasy_manager.backtest import data as D
from fantasy_manager.backtest import evaluate as E
from fantasy_manager.backtest import fit as F
from fantasy_manager.backtest import models as M
from fantasy_manager.backtest import pipeline as P
from fantasy_manager.backtest.scoring import PRESETS, fpg, load_scoring, scorer
from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot,
                                    ScoringConfig, StatLine)
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation import params as VP
from fantasy_manager.valuation.blend import K_BASELINE, K_INSEASON, RECENCY_WEIGHTS, multi_season_baseline

FIX = Path(__file__).parent / "fixtures" / "backtest"
S1, S2, S3, S4 = 20202021, 20212022, 20222023, 20232024
GOALS = PointsScoring({"G": 1.0})


def row(pid, season, g_pg, gp=80, group="F", age=None, birth=None, **extra):
    stats = {"G": g_pg * gp, **{k: v * gp for k, v in extra.items()}}
    pos = {"F": "C", "D": "D", "G": "G"}[group]
    return D.PlayerSeason(player_id=pid, season=season, name=f"P{pid}", group=group, position=pos, gp=gp,
                          stats=stats, birth_date=birth, age=age)


def table_of(*rows):
    return D.SeasonTable(rows)


# --------------------------------------------------------------------------- data

def test_seasons_and_ages():
    assert D.parse_seasons("20212022-20232024", []) == [S2, S3, S4]
    assert D.parse_seasons("2021,2023", []) == [S2, S4]
    assert D.parse_seasons(None, [S1]) == [S1]
    assert D.season_label(S2) == "2021-22"
    assert D.age_on_oct1("1997-01-13", 20152016) == pytest.approx(18.72, abs=0.01)
    assert D.position_group("LW") == "F" and D.position_group("D") == "D" and D.position_group("G") == "G"
    assert D.checkpoint_dates(S2) == [date(2021, 11, 1), date(2021, 12, 1), date(2022, 1, 1)]


def test_player_season_rows_roundtrip_and_player(tmp_path):
    r = row(1, S2, 0.5, gp=40, birth="2000-06-01", age=21.3, HIT=2.0)
    assert r.per_game() == {"G": 0.5, "HIT": 2.0}
    p = r.to_player("prior")
    assert p.lines["prior"].gp == 40 and p.positions == ["C"] and p.nhl_id == 1
    t = table_of(r, row(1, S1, 0.4), row(2, S2, 0.1, group="D"))
    D.save_table(t, tmp_path)
    t2 = D.load_table(tmp_path)
    assert t2.seasons == [S1, S2]
    assert [h.season for h in t2.history(1, S3)] == [S1, S2]
    assert t2.age(1, S3) == D.age_on_oct1("2000-06-01", S3)


def _fake_client(pages):
    """NhlClient whose fetcher serves report rows by (kind/report) from `pages`."""
    from fantasy_manager.providers.nhl import NhlClient

    def fetch(url, params=None):
        key = "/".join(url.rsplit("/", 2)[-2:])
        return {"data": pages.get(key, []), "total": len(pages.get(key, []))}
    return NhlClient(fetch_json=fetch)


def test_fetch_season_parses_reports():
    pages = {
        "skater/summary": [{"playerId": 1, "skaterFullName": "A B", "positionCode": "L", "seasonId": S2,
                            "gamesPlayed": 10, "goals": 5, "assists": 3, "points": 8, "ppPoints": 2, "ppGoals": 1,
                            "shPoints": 0, "shGoals": 0, "shots": 30, "penaltyMinutes": 4, "teamAbbrevs": "EDM"},
                           {"playerId": 3, "skaterFullName": "No Realtime", "positionCode": "D", "seasonId": S2,
                            "gamesPlayed": 2, "goals": 0, "assists": 1, "points": 1, "shots": 2}],
        "skater/realtime": [{"playerId": 1, "hits": 12, "blockedShots": 7, "emptyNetGoals": 1}],
        "skater/bios": [{"playerId": 1, "birthDate": "2000-10-02"}],
        "goalie/summary": [{"playerId": 2, "goalieFullName": "G K", "seasonId": S2, "gamesPlayed": 5,
                            "gamesStarted": 5, "wins": 3, "losses": 1, "otLosses": 1, "goalsAgainst": 10,
                            "shotsAgainst": 150, "saves": 140, "shutouts": 1, "assists": 1}],
        "goalie/bios": [{"playerId": 2, "birthDate": "1995-01-01"}],
    }
    rows = {r.player_id: r for r in D.fetch_season(_fake_client(pages), S2)}
    a = rows[1]
    assert a.group == "F" and a.position == "LW" and a.gp == 10
    assert a.stats["HIT"] == 12 and a.stats["BLK"] == 7 and a.stats["PPA"] == 1 and a.stats["ENG"] == 1
    assert a.age == pytest.approx(20.99, abs=0.01)
    assert rows[3].stats["HIT"] == 0.0 and rows[3].group == "D"   # no realtime row -> zeros
    g = rows[2]
    assert g.group == "G" and g.stats["SV"] == 140 and g.stats["W"] == 3 and g.stats["A"] == 1


def test_counting_fetcher_ttl():
    f = D.CountingFetcher(cache=None, today=date(2026, 9, 28))
    assert f.ttl_for({"cayenneExp": "seasonId=20232024 and gameTypeId=2"}) == D.TTL_HISTORICAL
    assert f.ttl_for({"cayenneExp": "seasonId=20262027 and gameTypeId=2"}) == D.TTL_CURRENT


# --------------------------------------------------------------------------- scoring

def test_presets_match_live_fixtures():
    for name in ("espn", "fantrax"):
        fx = json.loads((FIX / f"scoring_{name}.json").read_text())["scoring"]
        cfg = PRESETS[name]
        assert cfg.weights == fx["weights"] and cfg.goalie_weights == fx["goalie_weights"]
    assert load_scoring("ESPN")[0] == "espn"
    with pytest.raises(ValueError):
        load_scoring("nope")


def test_fpg_goalie_weights_apply_to_goalie_lines():
    sc = scorer(PRESETS["fantrax"])
    assert fpg({"G": 1.0}, sc) == 4.0
    assert fpg({"G": 1.0, "GS": 1.0, "SV": 0.0}, sc) == 20.0


# --------------------------------------------------------------------------- models

def ctx_for(table, season, scoring=GOALS, fitted=None):
    return M.make_context(table, season, scoring, fitted)


def test_naive_uses_last_season():
    t = table_of(row(1, S1, 0.2), row(1, S2, 0.5))
    ctx = ctx_for(t, S3)
    assert M.project(M.NaiveLastSeason(), t.history(1, S3), 25, ctx) == pytest.approx(0.5)
    assert M.NaiveLastSeason().rates([], 25, ctx) is None


def test_marcel_weights_regression_and_age():
    # one player, three seasons; mean (of the only F in S3) = his S3 rate
    t = table_of(row(1, S1, 0.3, gp=40), row(1, S2, 0.6, gp=80), row(1, S3, 0.9, gp=60))
    ctx = ctx_for(t, S4)
    raw = (5 * 60 * 0.9 + 4 * 80 * 0.6 + 3 * 40 * 0.3) / (5 * 60 + 4 * 80 + 3 * 40)
    n = (5 * 60 + 4 * 80 + 3 * 40) / 5
    mean = 0.9
    expect = (n * raw + M.MARCEL_K * mean) / (n + M.MARCEL_K)
    got = M.project(M.Marcel(age=False), t.history(1, S4), None, ctx)
    assert got == pytest.approx(expect)
    young = M.project(M.Marcel(), t.history(1, S4), 22.0, ctx)
    old = M.project(M.Marcel(), t.history(1, S4), 33.0, ctx)
    assert young == pytest.approx(expect * (1 + 0.006 * 5)) and old == pytest.approx(expect * (1 - 0.003 * 6))


def test_fm_current_is_the_app_multi_season_baseline():
    t = table_of(row(1, S2, 0.8, gp=30), *(row(i, S2, 0.2, gp=80) for i in range(2, 6)))
    ctx = ctx_for(t, S3)
    mean = ctx.means["F"]
    f = VP.age_factor("F", 24.0)                  # age 25 on Oct 1 -> 24 last season
    expect = multi_season_baseline([t.get(1, S2).statline("prior")], "F", ctx.means)
    got = M.FmCurrent().rates(t.history(1, S3), 25, ctx)
    assert got == pytest.approx({k: v * f for k, v in expect.items()})
    assert got["G"] == pytest.approx(f * (30 * 0.8 + K_BASELINE["F"] * mean["G"]) / (30 + K_BASELINE["F"]))
    # a player who skipped last season still has a baseline from the season before (weight 4)
    assert M.FmCurrent().rates([row(9, S1, 0.5)], None, ctx)["G"] == pytest.approx(
        (80 * 4 / 5 * 0.5 + K_BASELINE["F"] * mean["G"]) / (80 * 4 / 5 + K_BASELINE["F"]))
    # ... but none without any of the last three seasons
    assert M.FmCurrent().rates([row(9, S1 - 20002, 0.5)], 25, ctx) is None


def test_fm_current_matches_fm_multi_with_the_packaged_params():
    t = table_of(row(1, S1, 0.3, gp=40), row(1, S2, 0.6, gp=80), row(1, S3, 0.9, gp=60),
                 row(2, S3, 0.1, gp=50, group="D"), row(2, S2, 0.2, gp=70, group="D"),
                 *(row(i, S3, 0.5, gp=80) for i in range(3, 6)), *(row(i, S3, 0.1, gp=80, group="D") for i in range(6, 9)))
    ctx = ctx_for(t, S4)
    packaged = M.FittedParams(k_multi=dict(K_BASELINE), age_yoy=VP.age_yoy(), age_groups=VP.age_groups())
    for pid, age in ((1, 21.4), (1, 33.0), (2, 27.9), (2, None)):
        assert M.FmCurrent().rates(t.history(pid, S4), age, ctx) == pytest.approx(
            M.FmMulti(packaged).rates(t.history(pid, S4), age, ctx))


def test_fm_fitted_matches_fm_current_at_app_k_and_applies_age():
    t = table_of(row(1, S2, 0.8, gp=30), *(row(i, S2, 0.2, gp=80) for i in range(2, 6)))
    ctx = ctx_for(t, S3)
    # one prior season: the 3-season baseline reduces to last season shrunk with K_BASELINE
    app_like = M.FmFitted(M.FittedParams(k=dict(K_BASELINE), age_yoy=VP.age_yoy()))
    assert M.project(app_like, t.history(1, S3), 25, ctx) == pytest.approx(M.project(M.FmCurrent(), t.history(1, S3), 25, ctx))
    same = M.FmFitted(M.FittedParams(k={"F": 20.0, "D": 20.0, "G": 12.0}))
    aged = M.FmFitted(M.FittedParams(k={"F": 20.0}, age_yoy={"F": {23: 1.1, 24: 1.1, 25: 0.9}}))
    base = M.project(same, t.history(1, S3), 25, ctx)
    assert M.project(aged, t.history(1, S3), 25.5, ctx) == pytest.approx(base * 1.1)   # age 24.5 last season
    assert M.project(aged, t.history(1, S3), 40.0, ctx) == pytest.approx(base * 0.9)   # clamps to oldest bin
    fp = M.FittedParams.from_dict(json.loads(json.dumps(aged.params.to_dict())))
    assert fp.age_factor("F", 24.2) == 1.1 and fp.age_factor("G", 24.2) == 1.0


def test_fm_multi_weights_three_seasons():
    t = table_of(row(1, S1, 0.3, gp=40), row(1, S2, 0.6, gp=80), row(1, S3, 0.9, gp=60),
                 *(row(i, S3, 0.5, gp=80) for i in range(2, 5)))
    ctx = ctx_for(t, S4)
    params = M.FittedParams(k_multi={"F": 0.0})
    raw = (5 * 60 * 0.9 + 4 * 80 * 0.6 + 3 * 40 * 0.3) / (5 * 60 + 4 * 80 + 3 * 40)
    assert M.project(M.FmMulti(params), t.history(1, S4), None, ctx) == pytest.approx(raw)
    with pytest.raises(ValueError):
        M.get_models(["bogus"])


# --------------------------------------------------------------------------- metrics

def test_rank_and_spearman():
    assert E.rankdata([10, 20, 20, 5]) == [2.0, 3.5, 3.5, 1.0]
    assert E.spearman([1, 2, 3, 4], [10, 20, 30, 40]) == pytest.approx(1.0)
    assert E.spearman([1, 2, 3, 4], [4, 3, 2, 1]) == pytest.approx(-1.0)


def test_metrics_and_top_n():
    m = E.metrics([1.0, 2.0, 3.0], [1.0, 1.0, 5.0], top=False)
    assert m.mae == pytest.approx(1.0) and m.rmse == pytest.approx(math.sqrt(5 / 3)) and m.bias == pytest.approx(-1 / 3)
    pred = list(range(100))
    act = list(range(100))
    act[99], act[0] = act[0], act[99]      # swap best and worst
    assert E.top_n_precision(pred, act, 10) == pytest.approx(0.9)
    assert E.top_n_precision(pred[:5], act[:5], 10) is None


def test_evaluate_preseason_on_synthetic_history():
    rows = []
    for pid in range(1, 61):
        base = 0.1 + pid * 0.01
        for s in (S1, S2, S3, S4):
            rows.append(row(pid, s, base, gp=70, age=20.0 + pid % 15 + (s - S1) / 10001))
    t = table_of(*rows)
    res, preds = E.evaluate_preseason(t, GOALS, [S3, S4])
    by = {(r.season, r.model): r for r in res if r.pool == "skaters"}
    assert by[(S4, "naive")].mae == pytest.approx(0.0) and by[(S4, "naive")].spearman == pytest.approx(1.0)
    # flat careers: fm_current (3-season baseline + the fitted age factor) is off only by aging
    assert by[(S4, "fm_current")].n == 60 and by[(S4, "fm_current")].spearman > 0.99
    assert by[(S4, "fm_current")].mae < 0.05
    assert {r.model for r in res} == set(M.DEFAULT_MODELS)
    wins, total = E.head_to_head(res, "naive", "fm_current")
    assert total == 2


# --------------------------------------------------------------------------- fitting

def _pair(pid, age, prev, nxt, gp=80, mean=0.5, group="F"):
    return F.Pair(pid, S2, group, age, gp, prev, mean, gp, nxt)


def test_age_curve_delta_method_recovers_trajectory():
    pairs = []
    pid = 0
    for age in range(18, 41):
        ratio = 1.10 if age < 25 else 0.90
        for j in range(30):
            pid += 1
            prev = 0.5 + 0.01 * j
            pairs.append(_pair(pid, age + 0.5, prev, prev * ratio))
    c = F.fit_age_curve(pairs, "F", boot=20, window=0, min_pooled=5)
    assert c.peak_age() == 25 and c.level[25] == pytest.approx(1.0)
    assert c.level[24] == pytest.approx(1 / 1.10) and c.level[26] == pytest.approx(0.90)
    assert c.level[20] == pytest.approx(1.10 ** -5)
    assert c.lo[26] <= c.level[26] <= c.hi[26] and c.n[30] == 30
    assert F.dynasty_anchors(c, (25, 26)) == [[25, 1.0], [26, 0.9]]


def test_age_curve_sparse_ages_are_flat():
    pairs = [_pair(i, 27.2, 1.0, 0.8) for i in range(40)]
    c = F.fit_age_curve(pairs, "F", boot=0, window=0, min_pooled=25)
    assert c.yoy[27] == pytest.approx(0.8) and c.yoy[19] == 1.0 and c.yoy[35] == 1.0


def test_shrinkage_k_grid():
    persistent = [_pair(i, 25, 0.2 + 0.01 * i, 0.2 + 0.01 * i) for i in range(50)]
    assert F.fit_shrinkage_k(persistent)[0] == 0.0
    pure_noise = [_pair(i, 25, 0.5 + (0.3 if i % 2 else -0.3), 0.5) for i in range(50)]
    best, curve = F.fit_shrinkage_k(pure_noise)
    assert best == max(F.K_GRID) and curve[best] < curve[0.0]


def _inseason_fixture():
    t = table_of(row(1, S1, 0.5, gp=80), row(2, S1, 0.3, gp=80), row(3, S1, 0.1, gp=80),
                 row(1, S2, 0.6, gp=80), row(2, S2, 0.2, gp=80), row(3, S2, 0.2, gp=80), row(4, S2, 0.4, gp=60))
    W = D.WindowLine
    day = "2021-12-01"
    wins = {"to_date": {1: W(20, {"G": 14.0}), 2: W(20, {"G": 3.0}), 4: W(18, {"G": 8.0})},
            "last30": {1: W(13, {"G": 10.0}), 2: W(10, {"G": 1.0}), 4: W(12, {"G": 6.0})},
            "last15": {1: W(6, {"G": 6.0}), 4: W(7, {"G": 2.0})},
            "last7": {1: W(3, {"G": 0.0})},
            "rest": {1: W(60, {"G": 36.0}), 2: W(55, {"G": 11.0}), 4: W(9, {"G": 4.0})}}
    return t, {S2: {day: wins}}


def test_inseason_scalar_matches_app_blend():
    t, windows = _inseason_fixture()
    obs = E.inseason_observations(t, windows, GOALS, [S2])
    assert sorted(o.player_id for o in obs) == [1, 2]            # player 4 has < 10 GP after
    for o in obs:
        assert F.inseason_projection(o, K_INSEASON["skater"], dict(RECENCY_WEIGHTS)) == pytest.approx(o.fpg_app)
        comps = F._components([o], K_INSEASON["skater"])
        assert F._mae_components(comps, dict(RECENCY_WEIGHTS)) == pytest.approx(abs(o.fpg_app - o.fpg_rest))
    fit = F.fit_inseason(obs)
    assert fit["mae_fitted"] <= fit["mae_app"] + 1e-12
    assert abs(sum(fit["recency_weights"].values()) - 1.0) < 1e-9
    rows, fits = E.evaluate_inseason(obs)
    assert {r.model for r in rows} == set(E.INSEASON_MODELS)


def test_recency_weights_for_matches_app():
    from fantasy_manager.valuation.blend import recency_weights
    assert F.recency_weights_for(dict(RECENCY_WEIGHTS), {"last30": 5, "last15": 2, "last7": 4}) == \
        pytest.approx(recency_weights(5, 2, 4))


# --------------------------------------------------------------------------- archive

def _ctx(proj_g=0.5, as_of=date(2026, 10, 1)):
    p = Player(cid="espn:1", name="Connor Test", name_norm="connor test", ids={"espn": "1", "nhl": "8478402"},
               team="EDM", positions=["C"], pct_owned=99.0,
               lines={"projected": StatLine(split="projected", gp=80, stats={"G": proj_g * 80, "GP": 80})})
    fa = Player(cid="espn:2", name="Free Agent", name_norm="free agent", ids={"espn": "2"}, team="SJS",
                positions=["D"], lines={"projected": StatLine(split="projected", gp=70, stats={"G": 7.0, "GP": 70})})
    team = FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=[RosterSlot(slot="C", player=p, starting=True)])
    return LeagueContext(provider="espn", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 2.0}), roster_shape={"C": 1},
                         teams=[team], free_agents=[fa], matchup_period=1, as_of=as_of)


class _PV:
    def __init__(self, fpg, rates):
        self.fpg, self.rates = fpg, rates


def test_archive_projections_dedupes_same_day(tmp_path):
    ctx = _ctx()
    values = {"espn:1": _PV(1.1, {"G": 0.55})}
    path, status = A.archive_projections(ctx, tmp_path, values)
    assert status == "created" and path.name == "projections-espn-2026-10-01.json"
    assert A.archive_projections(ctx, tmp_path, values)[1] == "unchanged"
    assert A.archive_projections(_ctx(proj_g=0.6), tmp_path, values)[1] == "updated"
    assert len(list((tmp_path / "archive").glob("projections-*.json"))) == 1
    A.archive_projections(_ctx(as_of=date(2026, 10, 2)), tmp_path, values)
    assert len(A.list_snapshots(tmp_path, "espn")) == 2
    snap = json.loads(path.read_text())
    rec = {r["cid"]: r for r in snap["players"]}
    assert rec["espn:1"]["projected"]["gp"] == 80 and rec["espn:1"]["pct_owned"] == 99.0
    assert rec["espn:1"]["fm"]["rates"] == {"G": 0.55} and rec["espn:2"]["fm"] is None
    assert A.pick_snapshot(tmp_path, "espn", "first").name.endswith("2026-10-01.json")
    assert A.pick_snapshot(tmp_path, "espn", "last").name.endswith("2026-10-02.json")


def test_archive_recommendations_dedupes(tmp_path):
    ctx = _ctx()
    rec = Recommendation(kind="waiver", score=0.7, title="Add Free Agent", add=ctx.free_agents,
                         reasons=[Reason(code="VORP", text="x", value=1.0)])
    path, status = A.archive_recommendations([rec], "espn", tmp_path, as_of=date(2026, 10, 1))
    assert status == "created" and path.name == "recs-espn-2026-10-01.json"
    assert A.archive_recommendations([rec], "espn", tmp_path, as_of=date(2026, 10, 1))[1] == "unchanged"
    data = json.loads(path.read_text())
    assert data["recommendations"][0]["add"][0]["name"] == "Free Agent"


def test_score_archive_matches_by_nhl_id_and_name(tmp_path):
    ctx = _ctx()
    A.archive_projections(ctx, tmp_path, {"espn:1": _PV(1.0, {"G": 0.45}), "espn:2": _PV(0.2, {"G": 0.1})})
    actuals = [D.PlayerSeason(8478402, 20262027, "Connor Test", "F", "C", 80, {"G": 40.0}),
               D.PlayerSeason(99, 20262027, "Free Agent", "D", "D", 60, {"G": 6.0})]
    out = A.score_archive(actuals, tmp_path, providers=("espn", "fantrax"))
    assert set(out) == {"espn"}
    sk = out["espn"]["pools"]["skaters"]
    assert sk["provider"]["n"] == 2                                  # second matched by name
    assert sk["provider"]["mae"] == pytest.approx((abs(1.0 - 1.0) + abs(0.2 - 0.2)) / 2)
    assert sk["fm"]["mae"] == pytest.approx((abs(0.9 - 1.0) + abs(0.2 - 0.2)) / 2)


# --------------------------------------------------------------------------- pipeline / CLI

def _synthetic_store(tmp_path):
    rows = []
    for pid in range(1, 41):
        for i, s in enumerate((S1, S2, S3, S4)):
            g = 0.2 + 0.005 * pid + 0.02 * ((pid + i) % 3)
            rows.append(row(pid, s, g, gp=70, group="D" if pid % 4 == 0 else "F",
                            birth=f"{1990 + pid % 12}-03-01", age=D.age_on_oct1(f"{1990 + pid % 12}-03-01", s),
                            SOG=2.0))
    D.save_table(table_of(*rows), tmp_path)


def test_run_fit_and_evaluation_write_files(tmp_path):
    _synthetic_store(tmp_path)
    fitted = P.run_fit(tmp_path, "espn", boot=5)
    assert Path(fitted["path"]).name == "fitted_params.json"
    assert set(fitted["age_curve_report"]) == {"F", "D", "G"} and fitted["inseason"] is None
    out = P.run_evaluation(tmp_path, "espn", seasons=[S3, S4], inseason=False, today=date(2026, 9, 28),
                           log=lambda m: None)
    report = Path(out["report"]).read_text()
    assert Path(out["report"]).name == "report-2026-09-28.md"
    assert "## Verdict" in report and "fm_current" in report and "## Fitted parameters" in report
    assert P.latest_report(tmp_path) == Path(out["report"])


def test_cli_run_and_report(monkeypatch, tmp_path):
    from fantasy_manager.config import get_settings

    _synthetic_store(tmp_path)
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("FM_OFFLINE", "1")
    get_settings.cache_clear()
    monkeypatch.setattr(cli_backtest, "console", Console(width=200))
    runner = CliRunner()
    try:
        res = runner.invoke(cli_backtest.backtest_app, ["run", "--seasons", "20222023-20232024", "--no-inseason",
                                                         "--models", "naive,fm_current"])
        assert res.exit_code == 0, res.output
        res = runner.invoke(cli_backtest.backtest_app, ["report"])
        assert res.exit_code == 0 and "Projection backtest" in res.output
    finally:
        get_settings.cache_clear()
