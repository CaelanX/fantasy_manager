"""harness.matchup_calib: daily matchup snapshots, final scores and win-probability calibration."""
import math
from datetime import date, timedelta
from types import SimpleNamespace

import pytest

from fantasy_manager.harness import Ledger
from fantasy_manager.harness.matchup_calib import (MIN_MATCHUPS, attach_results, brier, calibration_report,
                                                   capture_matchup, fetch_final_scores, fit_sd_scale, log_loss,
                                                   pending_periods, record_snapshot, reliability, result_of,
                                                   summary_line)

D = date
START, END = D(2026, 11, 2), D(2026, 11, 8)


@pytest.fixture
def led(tmp_path):
    with Ledger(tmp_path) as L:
        yield L


def mu(period=1, start=START, end=END, p=0.6, mine=(50.0, 30.0), theirs=(45.0, 30.0), sds=(3.0, 4.0),
       days_left=5, opponent="Opp"):
    return SimpleNamespace(period=period, start=start, end=end, opponent_team=opponent, opponent_id="t2",
                           days_left=days_left, playoffs=False, my_points_so_far=mine[0],
                           my_projected_remaining=mine[1], their_points_so_far=theirs[0],
                           their_projected_remaining=theirs[1], my_sd=sds[0], their_sd=sds[1], win_probability=p,
                           source="test")


# --------------------------------------------------------------------------- recording

def test_snapshot_is_recorded_once_per_league_and_day(led):
    assert record_snapshot(led, "espn", D(2026, 11, 4), mu()) == "recorded"
    assert record_snapshot(led, "espn", D(2026, 11, 4), mu(p=0.9)) == "already recorded today"
    assert record_snapshot(led, "fantrax", D(2026, 11, 4), mu()) == "recorded"
    rows = led.query("SELECT * FROM matchup_snapshots WHERE league='espn'")
    assert len(rows) == 1
    r = rows[0]
    assert r["win_prob"] == 0.6                                   # the first capture of the day wins
    assert r["my_proj_total"] == 80.0 and r["their_proj_total"] == 75.0 and r["margin"] == 5.0
    assert r["margin_sd"] == pytest.approx(5.0)                   # sqrt(3^2 + 4^2)
    assert r["period_end"] == "2026-11-08" and r["skater_sd_k"] is not None


def test_nothing_recorded_without_an_opponent(led):
    assert record_snapshot(led, "espn", D(2026, 11, 4), mu(opponent=None)).startswith("nothing to record")
    assert record_snapshot(led, "espn", D(2026, 11, 4), mu(p=None)).startswith("nothing to record")
    assert led.count("matchup_snapshots") == 0


def test_results_attach_after_the_period_ends_and_are_rechecked_briefly(led):
    record_snapshot(led, "espn", D(2026, 11, 3), mu())
    assert pending_periods(led, "espn", END) == []                # last day still being played
    pend = pending_periods(led, "espn", END + timedelta(days=1))
    assert [p["period"] for p in pend] == [1]
    assert attach_results(led, "espn", {1: {"mine": 101.5, "theirs": 99.0, "source": "x"}}, pend) == 1
    r = led.query("SELECT * FROM matchup_results")[0]
    assert (r["result"], r["my_final"], r["their_final"]) == ("win", 101.5, 99.0)
    # a stat correction within the re-check window replaces it; later it is left alone
    pend = pending_periods(led, "espn", END + timedelta(days=2))
    attach_results(led, "espn", {1: {"mine": 98.0, "theirs": 99.0}}, pend)
    assert led.query("SELECT result FROM matchup_results")[0]["result"] == "loss"
    assert led.count("matchup_results") == 1
    assert pending_periods(led, "espn", END + timedelta(days=10)) == []
    assert attach_results(led, "espn", {}, pending_periods(led, "espn", END + timedelta(days=2))) == 0


def test_result_of():
    assert result_of(10, 9) == "win" and result_of(9, 10) == "loss" and result_of(10.001, 10) == "tie"


def test_capture_step_records_and_attaches_and_never_raises(led):
    ctx = SimpleNamespace(provider="espn")
    record_snapshot(led, "espn", D(2026, 11, 3), mu())
    out = capture_matchup(led, "espn", ctx, object(), {}, D(2026, 11, 10),
                          matchup_fn=lambda *a, **k: mu(period=2, start=D(2026, 11, 9), end=D(2026, 11, 15)),
                          finals_fn=lambda c, p, periods: {int(x): {"mine": 80, "theirs": 90} for x in periods})
    assert out["snapshot"] == "recorded" and out["results"] == 1 and out["period"] == 2 and not out["warnings"]

    def boom(*a, **k):
        raise RuntimeError("no provider")
    out = capture_matchup(led, "espn", ctx, object(), {}, D(2026, 11, 11), matchup_fn=boom, finals_fn=boom)
    assert out["snapshot"] == "failed" and any("no provider" in w for w in out["warnings"])


# --------------------------------------------------------------------------- provider final scores

def test_espn_final_scores_skip_undecided_periods():
    sched = [{"matchupPeriodId": 1, "winner": "HOME", "home": {"teamId": 13, "totalPoints": 120.5},
              "away": {"teamId": 4, "totalPoints": 110.0}},
             {"matchupPeriodId": 2, "winner": "UNDECIDED", "home": {"teamId": 4, "totalPoints": 50},
              "away": {"teamId": 13, "totalPoints": 60}},
             {"matchupPeriodId": 1, "winner": "AWAY", "home": {"teamId": 1}, "away": {"teamId": 2}}]
    league = SimpleNamespace(espn_request=SimpleNamespace(league_get=lambda params: {"schedule": sched}))
    prov = SimpleNamespace(_league_cached=lambda: league)
    ctx = SimpleNamespace(provider="espn", my_team=SimpleNamespace(team_id="13"))
    f = fetch_final_scores(ctx, prov, [1, 2])
    assert f == {1: {"mine": 120.5, "theirs": 110.0, "opponent_id": "4", "source": "ESPN scoreboard"}}


def test_fantrax_final_scores_from_schedule_tables():
    data = {"tableList": [{"caption": "Scoring Period 3", "subCaption": "(Mon Nov 16, 2026 - Sun Nov 22, 2026)",
                           "rows": [{"cells": [{"teamId": "a"}, {"content": "1,001.5"}, {"teamId": "me"},
                                               {"content": "990"}]}]}]}
    client = SimpleNamespace(call=lambda *req: [data])
    prov = SimpleNamespace(_get_client=lambda: client)
    ctx = SimpleNamespace(provider="fantrax", my_team=SimpleNamespace(team_id="me"))
    f = fetch_final_scores(ctx, prov, [3])
    assert f[3]["mine"] == 990.0 and f[3]["theirs"] == 1001.5 and f[3]["opponent_id"] == "a"
    assert fetch_final_scores(ctx, prov, []) == {}


# --------------------------------------------------------------------------- math

def test_brier_and_log_loss():
    assert brier([0.8, 0.3], [1, 0]) == pytest.approx((0.04 + 0.09) / 2)
    assert log_loss([0.8, 0.3], [1, 0]) == pytest.approx(-(math.log(0.8) + math.log(0.7)) / 2)
    assert log_loss([1.0], [0]) == pytest.approx(-math.log(1e-3))             # clipped, finite
    assert log_loss([0.5], [0.5]) == pytest.approx(math.log(2))               # a tie is y = 0.5
    assert brier([], []) is None and log_loss([], []) is None


def test_reliability_bins():
    bins = reliability([0.1, 0.15, 0.5, 0.95, 1.0], [0, 1, 1, 1, 1])
    assert [b["n"] for b in bins] == [2, 0, 1, 0, 2]
    assert bins[0]["mean_pred"] == pytest.approx(0.125) and bins[0]["observed"] == 0.5
    assert bins[1]["mean_pred"] is None and bins[4]["observed"] == 1.0


def test_sd_scale_weights_each_matchup_equally():
    assert fit_sd_scale({"a": [2.0, 2.0, 2.0, 2.0], "b": [0.0]}) == pytest.approx(math.sqrt(2.0))
    assert fit_sd_scale({"a": [1.0, -1.0]}) == pytest.approx(1.0)
    assert fit_sd_scale({}) is None


# --------------------------------------------------------------------------- the report

def _season(led, n, err=20.0, margin_sd=10.0):
    """n one-week matchups, two snapshots each; projected margin +5, actual margin 5 +/- err."""
    for i in range(n):
        start = D(2026, 10, 5) + timedelta(days=7 * i)
        end = start + timedelta(days=6)
        sds = (margin_sd / math.sqrt(2), margin_sd / math.sqrt(2))
        for k in range(2):
            record_snapshot(led, "espn", start + timedelta(days=k), mu(period=i + 1, start=start, end=end, p=0.7,
                                                                       sds=sds, days_left=7 - k))
        actual = 5.0 + (err if i % 2 == 0 else -err)
        pend = pending_periods(led, "espn", end + timedelta(days=1))
        attach_results(led, "espn", {i + 1: {"mine": 100.0 + actual, "theirs": 100.0}}, pend)


def test_report_fits_the_scale_with_enough_matchups(led):
    _season(led, 10)
    rep = calibration_report(led, "espn")
    assert rep["matchups"] == 10 and rep["graded_snapshots"] == 20 and rep["trust"] == "provisional"
    # outcomes alternate win (+25) / loss (-15) at p = 0.7
    assert rep["brier"] == pytest.approx((0.3 ** 2 + 0.7 ** 2) / 2)
    assert rep["record"] == {"win": 5, "loss": 5, "tie": 0}
    sc = rep["scale"]
    assert sc["s"] == pytest.approx(2.0) and sc["verdict"] == "overconfident" and sc["applied"] is False
    assert sc["recommended_skater_sd_k"] == pytest.approx(2 * sc["skater_sd_k"])
    assert rep["chase_win_rate"]["n"] == 0 and rep["protect_win_rate"]["n"] == 20
    assert "fitted SD scale 2.00 (overconfident)" in summary_line(rep)


def test_small_sample_guard(led):
    _season(led, MIN_MATCHUPS - 1)
    rep = calibration_report(led, "espn")
    assert rep["scale"] is None and rep["trust"] == "hidden"
    assert rep["status"].startswith("insufficient data (7 of 8")
    assert rep["brier"] is not None                              # the scores are still shown, labelled hidden
    assert "insufficient data" in summary_line(rep)


def test_ended_period_snapshots_are_not_graded(led):
    _season(led, 1)
    record_snapshot(led, "espn", END + timedelta(days=30), mu(period=1, days_left=0))   # nothing left to play
    assert calibration_report(led, "espn")["graded_snapshots"] == 2


def test_empty_report_and_surfaces(led):
    rep = calibration_report(led, "espn")
    assert rep["status"] == "no completed matchups yet" and rep["scale"] is None
    assert "no completed matchup yet" in summary_line(rep)
    from fantasy_manager.harness.health import capture
    from fantasy_manager.harness.metrics import status_report

    _season(led, 10)
    L = status_report(led, ["espn"])["leagues"]["espn"]
    assert L["matchup_calibration"]["scale"]["verdict"] == "overconfident"
    assert any("overconfident" in w for w in capture(led, "espn")["warnings"])


def test_cli_daily_records_the_matchup_and_status_shows_it(tmp_path, monkeypatch):
    import json

    from rich.console import Console
    from typer.testing import CliRunner

    from fantasy_manager import cli_backtest, cli_harness
    from fantasy_manager.analysis import matchup as am
    from fantasy_manager.harness import realized as R
    from tests.test_harness_ledger import _ctx

    monkeypatch.setattr(cli_harness, "_settings", lambda: SimpleNamespace(fm_data_dir=tmp_path, fm_offline=True))
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    ctx = _ctx()

    class Prov:
        warnings: list = []

        def activity(self, since=None):
            return []

        def lineup_snapshot(self, day):
            return []
    monkeypatch.setattr(cli_backtest, "_load_league_full", lambda lg, s, c, with_recs: (ctx, {}, [], [], Prov()))
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    monkeypatch.setattr(am, "current_matchup", lambda *a, **k: mu(p=0.42))
    runner = CliRunner()
    for _ in range(2):
        res = runner.invoke(cli_harness.harness_app, ["daily", "--league", "fantrax", "--json"])
        assert res.exit_code == 0, res.output
    out = json.loads(res.output)
    assert out["leagues"][0]["matchup"]["snapshot"] == "already recorded today"
    with Ledger(tmp_path) as L:
        assert L.count("matchup_snapshots") == 1
    res = runner.invoke(cli_harness.harness_app, ["status", "--league", "fantrax"])
    assert res.exit_code == 0, res.output
    assert "matchup win probability: 1 snapshot(s)" in res.output
