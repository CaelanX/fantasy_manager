"""harness.refit / params_store: replay, bounded search, the gate, calendar, apply / rollback,
champion-challenger shadow scoring. Synthetic ledgers in tmp dirs; hypothesis property tests for
the guardrails."""
import json
import os
import tempfile
from datetime import date, timedelta
from pathlib import Path

import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

from fantasy_manager.backtest import archive as A
from fantasy_manager.backtest.fit import InseasonObs
from fantasy_manager.harness import Ledger
from fantasy_manager.harness import refit as R
from fantasy_manager.harness.ingest import ingest_projections
from fantasy_manager.harness.metrics import grade_week
from fantasy_manager.harness.outcomes import store_scoring
from fantasy_manager.harness.params_store import PACKAGED, ParamsStore
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation import params as VP
from fantasy_manager.valuation.valuate import valuate_league

D = date
SC = ScoringConfig(kind="points", weights={"G": 1.0, "A": 0.5, "W": 2.0, "SV": 0.1, "GA": -1.0})
FIRST_SNAP = D(2026, 10, 5)
N_WEEKS = 6
N_PLAYERS = 80
AS_OF = D(2026, 12, 9)          # every 28-day window of the six weekly snapshots has matured


def _set_env(d):
    old = {k: os.environ.get(k) for k in (VP.OVERRIDE_ENV, VP.PARAMS_DIR_ENV)}
    os.environ[VP.OVERRIDE_ENV] = "1"
    os.environ[VP.PARAMS_DIR_ENV] = str(d)
    VP.reload()
    return old


def _restore_env(old):
    for k, v in old.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    VP.reload()


@pytest.fixture
def pdir(tmp_path):
    d = tmp_path / "harness" / "params"
    old = _set_env(d)
    yield d
    _restore_env(old)


# --------------------------------------------------------------------------- synthetic ledger

def _rate(i):
    return 0.2 + 0.02 * i                     # true goals per game, 0.20 .. 1.78


def build_ledger(L, n_players=N_PLAYERS, n_weeks=N_WEEKS, proj_bias=0.6):
    """Weekly snapshots of skaters whose season-to-date rate is their true rate and whose league
    projection is off by ``proj_bias`` toward 1.0 goals per game: less in-season shrinkage (a
    lower k_inseason) is better for every player. Each plays every other day at exactly his rate."""
    store_scoring(L, "espn", SC)
    rows = []
    means = {"F": {"G": 1.0, "A": 0.0}}
    for w in range(n_weeks):
        snap = FIRST_SNAP + timedelta(days=7 * w)
        L.execute("INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                  (f"means:espn:{snap.isoformat()}", json.dumps(means)))
        gp = 5 + 3 * w
        for i in range(n_players):
            r = _rate(i)
            proj = r + proj_bias * (1.0 - r)
            inputs = {"season": {"gp": gp, "stats": {"G": r * gp, "A": 0.0, "GP": gp}, "zeros": True},
                      "history": None, "projection": {"gp": 82, "stats": {"G": proj * 82, "GP": 82}},
                      "status": "healthy", "games_next7": 4, "offnight_next7": 0, "start_share": None,
                      "positions": ["C"]}
            rows.append({"league": "espn", "as_of": snap.isoformat(), "cid": f"p{i}", "name": f"p{i}",
                         "nhl_id": 1000 + i, "positions": "C", "status": "healthy", "fpg": None, "proj_week": None,
                         "inputs_json": json.dumps(inputs), "archive_version": 2})
    L.upsert("projections", rows, ("league", "as_of", "cid"))
    last = FIRST_SNAP + timedelta(days=7 * (n_weeks - 1) + 40)
    days = [FIRST_SNAP + timedelta(days=k) for k in range(1, (last - FIRST_SNAP).days + 1)]
    L.upsert("realized_pulls", [{"game_date": d.isoformat(), "n_players": n_players} for d in days], ("game_date",))
    L.upsert("realized_daily", [{"nhl_id": 1000 + i, "game_date": d.isoformat(), "gp": 1,
                                 "stats_json": json.dumps({"G": _rate(i)})}
                                for i in range(n_players) for k, d in enumerate(days) if k % 2 == 0],
             ("nhl_id", "game_date"))
    return L


def hist_flat(n=200):
    """Historical checkpoints the in-season k barely matters for (gp_td 0: baseline only)."""
    return [InseasonObs(season=2024, day="11-01", player_id=i, group="F", gp_td=0, fpg_td=0.0, fpg_base=1.0,
                        has_base=True, splits={}, gp_rest=40, fpg_rest=1.0 + (0.1 if i % 2 else -0.1))
            for i in range(n)]


def hist_prefers_high_k(n=200):
    """Checkpoints where the baseline is right and to-date is noisy: a lower k is worse."""
    return [InseasonObs(season=2024, day="11-01", player_id=i, group="F", gp_td=10, fpg_td=1.0 + (0.5 if i % 2 else -0.5),
                        fpg_base=1.0, has_base=True, splits={}, gp_rest=40, fpg_rest=1.0) for i in range(n)]


@pytest.fixture
def led(tmp_path):
    with Ledger(tmp_path) as L:
        yield L


# --------------------------------------------------------------------------- replay == live valuation

def _league():
    def line(split, gp, g, a=0.0, extra=None):
        return StatLine(split=split, gp=gp, stats={"G": g * gp, "A": a * gp, "PIM": 0.0, "GP": gp, **(extra or {})})

    star = Player(cid="e:1", name="Star", name_norm="star", ids={"nhl": "1"}, team="EDM", positions=["C"],
                  birth_date=D(1997, 1, 13), status="dtd",
                  lines={"projected": line("projected", 80, 0.6, 0.7), "prior": line("prior", 82, 0.55, 0.8),
                         "prior2": line("prior2", 70, 0.5), "season": line("season", 12, 0.8, 0.0),
                         "last30": line("last30", 12, 0.8), "last15": line("last15", 6, 1.0), "last7": line("last7", 3, 0.0)})
    vet = Player(cid="e:2", name="Vet", name_norm="vet", ids={"nhl": "2"}, team="TOR", positions=["D"],
                 birth_date=D(1990, 3, 1),
                 lines={"prior": line("prior", 60, 0.1, 0.4), "prior2": line("prior2", 75, 0.12, 0.3),
                        "season": line("season", 10, 0.0, 0.6), "last15": line("last15", 5, 0.0, 1.0)})
    rookie = Player(cid="e:3", name="Rook", name_norm="rook", ids={"nhl": "3"}, team="EDM", positions=["LW"],
                    lines={"projected": line("projected", 30, 0.3), "season": line("season", 4, 0.5)})
    walkon = Player(cid="e:4", name="Walk", name_norm="walk", ids={"nhl": "4"}, team="TOR", positions=["C"],
                    lines={"season": line("season", 3, 0.3)})
    goalie = Player(cid="e:5", name="Tender", name_norm="tender", ids={"nhl": "5"}, team="TOR", positions=["G"],
                    lines={"prior": StatLine(split="prior", gp=50, stats={"W": 25, "SV": 1300, "GA": 130, "GS": 48, "GP": 50}),
                           "season": StatLine(split="season", gp=8, stats={"W": 5, "SV": 200, "GA": 20, "GS": 8, "GP": 8})})
    extras = [Player(cid=f"e:{10 + i}", name=f"X{i}", name_norm=f"x{i}", ids={"nhl": str(10 + i)}, team="EDM",
                     positions=["C" if i % 2 else "D"], lines={"prior": line("prior", 40 + i, 0.2 + 0.03 * i, 0.3)})
              for i in range(6)]
    team = FantasyTeam(team_id="1", name="me", owner_is_me=True,
                       slots=[RosterSlot(slot="C", player=p, starting=True) for p in (star, vet, rookie, goalie)])
    as_of = D(2026, 11, 2)
    sched = {t: [as_of + timedelta(days=k) for k in (0, 2, 3, 5)] for t in ("EDM", "TOR")}
    return LeagueContext(provider="espn", league_id="1", season=2027, name="T", scoring=SC,
                         roster_shape={"C": 1, "D": 1, "G": 1}, teams=[team], free_agents=[walkon, *extras],
                         matchup_period=1, as_of=as_of, schedule=sched,
                         games_per_day={as_of + timedelta(days=k): 5 for k in range(7)})


def _replayed(ctx, values, tmp_path):
    path, _ = A.archive_projections(ctx, tmp_path, values)
    snap = A.load_snapshot(path)
    sc = PointsScoring(SC.weights)
    return {rec["cid"]: R.obs_from_inputs("espn", ctx.as_of, {**rec, "nhl_id": rec["nhl_id"]}, rec["inputs"],
                                          snap["position_means"], sc) for rec in snap["players"]}


def test_replay_reproduces_the_live_valuation(tmp_path):
    ctx = _league()
    values = valuate_league(ctx, PointsScoring(SC.weights))
    obs = _replayed(ctx, values, tmp_path)
    assert len(obs) == len(values)
    for cid, pv in values.items():
        o = obs[cid]
        assert o.exact
        assert R.inseason_projection(o) == pytest.approx(pv.fpg, abs=1e-4), cid
        if pv.proj_week is not None:
            assert R.week_projection(o) == pytest.approx(pv.proj_week, abs=1e-3), cid
    # the FPG-space view carries the live fields
    iv = obs["e:1"].to_inseason()
    assert iv.gp_td == 12 and iv.status == "dtd" and iv.fpg_proj is not None and iv.fpg_hist is not None


def test_replay_with_candidate_params_equals_the_live_run_under_that_version(tmp_path, pdir):
    ctx = _league()
    sc = PointsScoring(SC.weights)
    obs = _replayed(ctx, valuate_league(ctx, sc), tmp_path)
    knobs = {**R.current_knobs(), "k_inseason.skater": 20.0, "k_inseason.goalie": 32.0, "recency.last30": 0.1,
             "recency.season": 0.85, "recency.last15": 0.05, "projection_weight": 0.7, "k_projection.skater": 24.0,
             "availability.dtd.week": 0.65, "start_share.k": 12.0, "start_share.prior": 0.45, "offnight_bonus": 0.06}
    cand = R.params_for(knobs)
    replay = {cid: (R.inseason_projection(o, cand), R.week_projection(o, cand)) for cid, o in obs.items()}
    store = ParamsStore(pdir)
    rec = store.propose(R.knobs_override(knobs), list(knobs))
    store.apply(rec["version"])
    live = valuate_league(_league(), sc)                         # the app, with the version active
    for cid, pv in live.items():
        assert replay[cid][0] == pytest.approx(pv.fpg, abs=1e-4), cid
        if pv.proj_week is not None:
            assert replay[cid][1] == pytest.approx(pv.proj_week, abs=1e-3), cid


def test_archive_header_means_reach_the_ledger(tmp_path, led):
    ctx = _league()
    path, _ = A.archive_projections(ctx, tmp_path, valuate_league(ctx, PointsScoring(SC.weights)))
    snap = A.load_snapshot(path)
    assert set(snap["position_means"]) == {"F", "D", "G"}
    ingest_projections(led, "espn", ctx.as_of.isoformat(), snap)
    assert R._means(led, "espn", ctx.as_of.isoformat()) == snap["position_means"]
    goalie = next(r for r in snap["players"] if r["cid"] == "e:5")["inputs"]
    assert goalie["share"]["source"] == "history" and goalie["share"]["team_games"] > 0


# --------------------------------------------------------------------------- end to end

def test_replay_table_targets_and_weeks(led):
    build_ledger(led)
    obs, info = R.replay_table(led, AS_OF)
    assert info["leagues"] == {"espn": N_PLAYERS * N_WEEKS}
    o = next(x for x in obs if x.player == 1010 and x.snap == FIRST_SNAP)
    assert o.gp28 == 14 and o.real_fpg28 == pytest.approx(_rate(10)) and o.gp7 == 4
    assert o.real_pts7 == pytest.approx(4 * _rate(10))
    assert len({x.week for x in obs}) == N_WEEKS
    early, _ = R.replay_table(led, FIRST_SNAP + timedelta(days=29))       # only the first 28-day window matured
    assert {x.snap for x in early if x.real_fpg28 is not None} == {FIRST_SNAP}


def test_refit_applies_a_better_k_within_bounds_and_rollback_restores_the_hash(led, pdir):
    build_ledger(led)
    h0 = VP.params_hash()
    store = ParamsStore(pdir, led)
    res = R.run_refit(led, AS_OF, mode="apply", store=store, hist={"espn": hist_flat()})
    assert res.gate.passed, res.gate.reasons
    assert res.action == "applied" and res.version == "v0001"
    assert "k_inseason" in res.groups_selected and len(res.groups_selected) <= 2
    k_new = res.candidate["k_inseason.skater"]
    assert k_new == pytest.approx(20.0)                                  # the -20% bound
    assert VP.k_inseason("skater") == pytest.approx(20.0)                # live now
    fo = res.objectives["fpg"]
    assert fo["gain"] >= 0.01 and fo["ci_lo"] > 0 and fo["hist_change"] <= 0.005
    rec = store.get("v0001")
    assert rec["status"] == "active" and rec["parent"] == PACKAGED and rec["hash"] == VP.params_hash()
    assert rec["params"]["inseason"]["k_skater"] == pytest.approx(20.0)
    assert set(rec["metrics"]) >= {"holdout_before", "holdout_after", "hist_before", "hist_after", "n_live"}
    assert rec["metrics"]["n_live"] == N_PLAYERS * N_WEEKS
    assert json.loads((pdir / "active.json").read_text())["version"] == "v0001"
    rows = led.query("SELECT version, status, params_hash FROM param_versions")
    assert rows == [{"version": "v0001", "status": "active", "params_hash": rec["hash"]}]
    assert {r["param"] for r in res.rows} >= {"k_inseason.skater", "k_inseason.goalie"}
    goalie_row = next(r for r in res.rows if r["param"] == "k_inseason.goalie")
    assert not goalie_row["changed"]                                    # < 150 goalie obs: frozen
    out = store.rollback()
    assert out == {"from": "v0001", "to": PACKAGED, "hash": h0} and VP.params_hash() == h0
    assert store.get("v0001")["status"] == "rolled_back"
    assert led.query("SELECT status FROM param_versions")[0]["status"] == "rolled_back"


def test_dry_run_and_propose_write_nothing_active(led, pdir):
    build_ledger(led)
    store = ParamsStore(pdir, led)
    res = R.run_refit(led, AS_OF, mode="dry-run", store=store, hist={"espn": hist_flat()})
    assert res.gate.passed and res.action == "dry-run" and store.versions() == []
    res = R.run_refit(led, AS_OF, mode="propose", store=store, hist={"espn": hist_flat()})
    assert res.action == "proposed" and store.get(res.version)["status"] == "proposed"
    assert store.active_name() == PACKAGED and VP.k_inseason("skater") == 25.0


def test_candidate_that_wins_holdout_but_worsens_history_is_rejected(led, pdir, monkeypatch):
    build_ledger(led)
    monkeypatch.setattr(R, "HIST_POOL_K", 10)          # let the live data dominate the search ...
    res = R.run_refit(led, AS_OF, mode="apply", store=ParamsStore(pdir, led),
                      hist={"espn": hist_prefers_high_k()}, only=["k_inseason"])
    fo = res.objectives["fpg"]
    assert fo["gain"] >= 0.01 and fo["ci_lo"] > 0                       # ... it wins the holdout
    assert fo["hist_change"] > 0.005                                    # ... but history gets worse
    assert not res.gate.passed and res.action == "none"
    assert any("historical MAE" in r for r in res.gate.reasons)
    assert VP.k_inseason("skater") == 25.0


def test_gate_rejects_history_regression_directly():
    cur = R.current_knobs(VP.load_packaged())
    cand = {**cur, "k_inseason.skater": 21.0}
    ok = R.ObjectiveStats("fpg", 200, 0.30, 0.28, 0.02, 0.1, 0.25, 0.2505, 1000)
    stats = R.GateStats(n_live=500, n_goalie=0, weeks=5, n_week=500, objectives={"fpg": ok})
    assert R.gate(cand, cur, stats).passed
    bad = R.ObjectiveStats("fpg", 200, 0.30, 0.28, 0.02, 0.1, 0.25, 0.2515, 1000)     # +0.6% history
    res = R.gate(cand, cur, R.GateStats(500, 0, 5, 500, {"fpg": bad}))
    assert not res.passed and not res.checks["statistics"]
    weak = R.ObjectiveStats("fpg", 200, 0.30, 0.298, -0.01, 0.05, 0.25, 0.25, 1000)   # < 1%, CI spans 0
    reasons = R.gate(cand, cur, R.GateStats(500, 0, 5, 500, {"fpg": weak})).reasons
    assert any("needs >= 1% better" in r for r in reasons) and any("does not exclude 0" in r for r in reasons)
    goalie = {**cur, "k_inseason.goalie": 36.0}
    res = R.gate(goalie, cur, R.GateStats(500, 100, 5, 500, {"fpg": ok}))
    assert not res.passed and any("goalie" in r and "50 more" in r for r in res.reasons)


# --------------------------------------------------------------------------- calendar

def test_calendar_lock_and_force(led, pdir):
    build_ledger(led)
    res = R.run_refit(led, D(2026, 10, 20), mode="dry-run", store=ParamsStore(pdir, led))
    assert res.locked and "locked until 2026-11-02" in res.lock_reason and res.gate is None
    res = R.run_refit(led, D(2026, 10, 20), mode="dry-run", force=True, store=ParamsStore(pdir, led))
    assert not res.locked and not res.gate.passed                      # force never bypasses the gate
    assert res.n_live == 0 and any("need 400 (400 more)" in r for r in res.gate.reasons)
    # after the lock the same data is refit without --force
    assert not R.run_refit(led, AS_OF, mode="dry-run", store=ParamsStore(pdir, led),
                           hist={"espn": hist_flat()}).locked


def test_refit_days_and_auto_apply_policy():
    assert not R.is_refit_day(D(2026, 10, 19)) and R.is_refit_day(D(2026, 11, 2))
    assert R.is_refit_day(D(2026, 11, 16)) and not R.is_refit_day(D(2026, 11, 9)) and R.is_refit_day(D(2026, 11, 30))
    assert R.next_refit_day(D(2026, 9, 29)) == D(2026, 11, 2) and R.next_refit_day(D(2026, 11, 3)) == D(2026, 11, 16)
    assert R.daily_refit_mode(D(2026, 11, 2), True) == "propose"         # refit, but no auto-apply before 11-16
    assert R.daily_refit_mode(D(2026, 11, 16), True) == "auto"
    assert R.daily_refit_mode(D(2026, 11, 16), False) == "propose"       # prefs.harness_auto_apply off
    assert R.daily_refit_mode(D(2026, 11, 9), True) is None
    assert R.allowed_groups("auto", D(2027, 1, 11)) == R.TIER_A          # auto: Tier A only, always
    assert R.allowed_groups("apply", D(2026, 11, 16)) == R.TIER_A        # Tier B proposals only until Dec 1
    assert set(R.allowed_groups("apply", D(2026, 12, 1))) == set(R.GROUPS)
    assert set(R.allowed_groups("propose", D(2026, 11, 2))) == set(R.GROUPS)


def test_auto_mode_applies_only_tier_a(led, pdir):
    build_ledger(led)
    store = ParamsStore(pdir, led)
    res = R.run_refit(led, AS_OF, mode="auto", store=store, hist={"espn": hist_flat()},
                      only=["availability", "offnight_bonus"])
    assert res.groups_searched == [] and res.action == "none" and store.versions() == []
    assert any("Tier B" in n for n in res.notes)
    res = R.run_refit(led, AS_OF, mode="auto", store=store, hist={"espn": hist_flat()})
    assert res.action == "applied" and set(res.groups_selected) <= set(R.TIER_A)
    assert store.get(res.version)["applied_by"] == "auto"


def test_prefs_flag_defaults_on(tmp_path):
    from fantasy_manager.prefs import harness_auto_apply, set_pref

    assert harness_auto_apply(tmp_path) is True
    set_pref("harness_auto_apply", False, tmp_path)
    assert harness_auto_apply(tmp_path) is False


def test_cli_refit_locked_and_forced(tmp_path, monkeypatch):
    from types import SimpleNamespace

    from rich.console import Console
    from typer.testing import CliRunner

    from fantasy_manager import cli_harness

    monkeypatch.setattr(cli_harness, "_settings", lambda: SimpleNamespace(fm_data_dir=tmp_path, fm_offline=True))
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    old = _set_env(tmp_path / "harness" / "params")
    try:
        runner = CliRunner()
        res = runner.invoke(cli_harness.harness_app, ["refit", "--dry-run", "--as-of", "2026-09-29"])
        assert res.exit_code == 0 and "locked until 2026-11-02" in res.output
        res = runner.invoke(cli_harness.harness_app, ["refit", "--dry-run", "--force", "--as-of", "2026-09-29"])
        assert res.exit_code == 0, res.output
        assert "Gate: FAILED" in res.output and "insufficient live obs: 0" in res.output and "400 more" in res.output
        res = runner.invoke(cli_harness.harness_app, ["refit", "--dry-run", "--force", "--json"])
        assert json.loads(res.output)["gate"]["passed"] is False
        res = runner.invoke(cli_harness.harness_app, ["refit", "--only", "age_yoy", "--force"])
        assert res.exit_code == 2 and "not eligible" in res.output
        res = runner.invoke(cli_harness.harness_app, ["params"])
        assert res.exit_code == 0 and "Active params: packaged" in res.output and "k_inseason.skater" in res.output
        res = runner.invoke(cli_harness.harness_app, ["rollback"])
        assert res.exit_code == 1 and "nothing to roll back" in res.output
    finally:
        _restore_env(old)


# --------------------------------------------------------------------------- champion / challenger

def _promote_bad_version(store, k=60.0, since=D(2026, 10, 1)):
    knobs = {**R.current_knobs(), "k_inseason.skater": k}
    rec = store.propose(R.knobs_override(knobs, ["k_inseason.skater"]), ["k_inseason.skater"])
    store.apply(rec["version"], as_of=since)
    return rec


def test_shadow_scoring_rolls_back_after_two_losing_weeks(led, pdir):
    build_ledger(led)
    h0 = VP.params_hash()
    store = ParamsStore(pdir, led)
    _promote_bad_version(store)                       # k 60: shrinks the true to-date rates toward bad projections
    assert VP.k_inseason("skater") == 60.0
    info = grade_week(led, "espn", D(2026, 11, 9))    # snapshot 10-05 matured: first losing week
    sh = info["shadow"]
    assert sh["winner"] == "shadow" and sh["n"] == N_PLAYERS and "rolled_back" not in sh
    assert store.active_name() == "v0001"
    grade_week(led, "espn", D(2026, 11, 9))           # regrading the same week does not count twice
    assert store.active_name() == "v0001"
    info = grade_week(led, "espn", D(2026, 11, 16))   # second straight losing week -> automatic rollback
    assert info["shadow"]["rolled_back"]["to"] == PACKAGED
    assert store.active_name() == PACKAGED and VP.params_hash() == h0
    rec = store.get("v0001")
    assert rec["status"] == "rolled_back" and any(e["by"] == "auto-rollback" for e in rec["changelog"])
    assert led.query("SELECT command FROM runs WHERE command='auto-rollback'")


def test_shadow_scoring_keeps_a_winning_version(led, pdir):
    build_ledger(led)
    store = ParamsStore(pdir, led)
    _promote_bad_version(store, k=20.0)               # a better k: the active version wins
    for wk in (D(2026, 11, 9), D(2026, 11, 16), D(2026, 11, 23)):
        assert grade_week(led, "espn", wk)["shadow"]["winner"] == "active"
    assert store.active_name() == "v0001" and len(store.get("v0001")["shadow"]["weeks"]) == 3


def test_apply_shadows_the_previous_version_and_retires_older_ones(pdir):
    store = ParamsStore(pdir)
    cur = R.current_knobs()
    v1 = store.propose(R.knobs_override({**cur, "k_inseason.skater": 22.0}, ["k_inseason.skater"]), ["x"])
    store.apply(v1["version"])
    v2 = store.propose(VP.deep_merge(store.active_params(),
                                     R.knobs_override({**cur, "projection_weight": 0.65}, ["projection_weight"])),
                       ["projection_weight"])
    assert v2["parent"] == "v0001" and v2["params"]["inseason"]["k_skater"] == 22.0      # cumulative
    store.apply(v2["version"])
    assert store.get("v0001")["status"] == "shadow" and store.get("v0002")["shadow"]["version"] == "v0001"
    v3 = store.propose(store.active_params(), ["none"])
    store.apply(v3["version"])
    assert [store.get(v)["status"] for v in ("v0001", "v0002", "v0003")] == ["retired", "shadow", "active"]
    assert store.rollback(to="v0001")["to"] == "v0001" and VP.k_inseason("skater") == 22.0
    assert VP.projection_weight() == 0.6
    with pytest.raises(ValueError):
        store.rollback(to="v0042")


# --------------------------------------------------------------------------- property tests

KNOB_NAMES = [k.name for k in R.KNOBS]
CURRENT = R.current_knobs(VP.load_packaged())


def _passing_stats():
    ok = {obj: R.ObjectiveStats(obj, 500, 1.0, 0.9, 0.05, 0.15, 0.3 if obj == "fpg" else None,
                                0.3 if obj == "fpg" else None, 1000) for obj in ("fpg", "week")}
    return R.GateStats(n_live=1000, n_goalie=400, weeks=8, n_week=1000, objectives=ok)


@settings(max_examples=300, deadline=None)
@given(changes=st.dictionaries(st.sampled_from(KNOB_NAMES), st.floats(-2.0, 2.0, allow_nan=False), max_size=8),
       rebalance=st.booleans())
def test_gated_candidates_respect_bounds_and_group_limit(changes, rebalance):
    cand = dict(CURRENT)
    for name, rel in changes.items():
        base = cand[name]
        cand[name] = base * (1 + rel) if R.KNOB[name].relative else base + rel * 0.2
    if rebalance:     # the search keeps recency summing to 1 via the season weight
        cand["recency.season"] = 1.0 - sum(cand[f"recency.{s}"] for s in R.RECENCY_SPLITS)
    res = R.gate(cand, CURRENT, _passing_stats())
    if res.passed:
        changed = R.changed_knobs(cand, CURRENT)
        assert 0 < len(R.changed_groups(cand, CURRENT)) <= R.MAX_GROUPS
        for name in changed:
            lo, hi = R.KNOB[name].bounds(CURRENT[name])
            assert lo - 1e-9 <= cand[name] <= hi + 1e-9
        assert abs(sum(cand[f"recency.{s}"] for s in ("season", *R.RECENCY_SPLITS)) - 1.0) <= 1e-9
    else:
        assert res.reasons


@settings(max_examples=200, deadline=None)
@given(w30=st.floats(0, 0.5), w15=st.floats(0, 0.3), w7=st.floats(0, 0.2),
       at30=st.floats(-0.05, 0.05), at15=st.floats(-0.05, 0.05))
def test_search_moves_stay_in_bounds_and_recency_sums_to_one(w30, w15, w7, at30, at15):
    cur = {**CURRENT, "recency.last30": w30, "recency.last15": w15, "recency.last7": w7,
           "recency.season": 1.0 - w30 - w15 - w7}
    at = dict(cur)
    at["recency.last30"] = max(0.0, w30 + at30)
    at["recency.last15"] = max(0.0, w15 + at15)
    at["recency.season"] = 1.0 - at["recency.last30"] - at["recency.last15"] - w7
    for cand in R.recency_moves(cur, at):
        full = {**cur, **cand}
        assert abs(sum(full[f"recency.{s}"] for s in ("season", *R.RECENCY_SPLITS)) - 1.0) <= 1e-9
        for s in R.RECENCY_SPLITS:
            lo, hi = R.KNOB[f"recency.{s}"].bounds(cur[f"recency.{s}"])
            assert lo - 1e-9 <= full[f"recency.{s}"] <= hi + 1e-9 and full[f"recency.{s}"] >= 0
        lo, hi = R.KNOB["recency.season"].bounds(cur["recency.season"])
        assert lo - 1e-9 <= full["recency.season"] <= hi + 1e-9
        assert R.gate(full, cur, _passing_stats()).checks["bounds"]
    for k in R.KNOBS:
        v0 = cur[k.name] if k.group == "recency_weights" else CURRENT[k.name] * (1 + at30)
        for v in R.knob_grid(k, v0):
            lo, hi = k.bounds(v0)
            assert lo - 1e-12 <= v <= hi + 1e-12


@settings(max_examples=25, deadline=None, suppress_health_check=[HealthCheck.too_slow])
@given(ks=st.lists(st.floats(20.0, 30.0, allow_nan=False), min_size=1, max_size=4),
       pws=st.lists(st.floats(0.5, 0.7, allow_nan=False), min_size=4, max_size=4))
def test_rollback_restores_the_exact_hash(ks, pws):
    with tempfile.TemporaryDirectory() as tmp:
        old = _set_env(Path(tmp))
        try:
            store = ParamsStore(Path(tmp))
            hashes = [VP.params_hash()]
            for k, pw in zip(ks, pws):
                knobs = {**R.current_knobs(), "k_inseason.skater": k, "projection_weight": pw}
                over = VP.deep_merge(store.active_params(),
                                     R.knobs_override(knobs, ["k_inseason.skater", "projection_weight"]))
                rec = store.propose(over, ["k_inseason.skater", "projection_weight"])
                store.apply(rec["version"])
                assert VP.params_hash() == rec["hash"]
                hashes.append(VP.params_hash())
            for expected in reversed(hashes[:-1]):
                store.rollback()
                assert VP.params_hash() == expected
            assert store.active_name() == PACKAGED
        finally:
            _restore_env(old)
