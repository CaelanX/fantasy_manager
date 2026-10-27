"""Track record shown on cards / CLI / overview (harness.track): read-only over graded outcomes."""
from datetime import date

import pytest

from fantasy_manager.harness import track as tk
from fantasy_manager.harness.ledger import DB_NAME, Ledger
from fantasy_manager.models import Recommendation

TODAY = date(2026, 12, 1)


def _seed(tmp_path, rows):
    """rows: (kind, title, gain_units, window, hit, gain, origin, window_end)."""
    with Ledger(tmp_path) as led:
        for i, (kind, title, units, window, hit, gain, origin, end) in enumerate(rows):
            ep = f"ep{i}"
            led.db.execute("INSERT INTO rec_episodes(episode_id, league, rec_key, kind, title, first_seen, last_seen, "
                           "n_days) VALUES (?, 'espn', ?, ?, ?, '2026-11-01', '2026-11-01', 1)", (ep, ep, kind, title))
            led.db.execute("INSERT INTO outcomes(outcome_id, league, episode_id, window, basis, realized_gain, "
                           "gain_units, hit, kind, origin, complete, window_end) VALUES (?, 'espn', ?, ?, 'first_seen', "
                           "?, ?, ?, ?, ?, 1, ?)", (f"o{i}", ep, window, gain, units, hit, kind, origin, end))
        led.db.commit()


def _streams(n, hits, end="2026-11-20"):
    return [("waiver", "Add X, drop Y for this matchup", "matchup_pts", "7d", int(i < hits), 1.0 if i < hits else -1.0,
             "ignored", end) for i in range(n)]


def test_no_ledger_is_not_created_and_says_why(tmp_path):
    book = tk.TrackBook.load(tmp_path, "espn", TODAY)
    assert not (tmp_path / DB_NAME).exists() and not book.available
    text = tk.track_text(book, Recommendation(kind="waiver", score=1, title="Add A, drop B"))
    assert text.startswith("Track record: starts once the daily harness run")


def test_rate_by_subkind_with_ci(tmp_path):
    _seed(tmp_path, _streams(25, 15))
    tr = tk.track_record("waiver", "stream", data_dir=tmp_path, league="espn", today=TODAY)
    assert tr["n"] == 25 and tr["hits"] == 15 and tr["enough"] and tr["subkind"] == "stream"
    assert tr["hit_rate"] == pytest.approx(0.6) and tr["avg_gain"] == pytest.approx(0.2)
    lo, hi = tr["ci"]
    assert lo < 0.6 < hi
    assert tk.record_text(tr).startswith("Similar moves (this-matchup pickups): 60% gained points (n=25, last 60 days")


def test_below_min_n_is_honest(tmp_path):
    _seed(tmp_path, _streams(3, 2))
    book = tk.TrackBook.load(tmp_path, "espn", TODAY)
    rec = Recommendation(kind="waiver", score=1, title="Add A, drop B for this matchup", gain_units="matchup_pts")
    assert tk.track_text(book, rec) == ("Track record: not enough history yet (n=3 graded this-matchup pickups, "
                                        f"shown from {tk.MIN_N})")


def test_thin_subkind_falls_back_to_kind(tmp_path):
    season = [("waiver", "Add A, drop B", "season_fpg", "28d", 1, 0.5, "followed", "2026-11-20")] * 22
    _seed(tmp_path, season + _streams(2, 1))
    book = tk.TrackBook.load(tmp_path, "espn", TODAY)
    tr = book.record("waiver", "stream")
    assert tr["subkind"] is None and tr["n"] == 24 and tr["enough"]


def test_old_wrong_window_and_user_moves_are_excluded(tmp_path):
    rows = (_streams(2, 2, end="2026-09-01")                                      # older than 60 days
            + [("waiver", "Add A for this matchup", "matchup_pts", "28d", 1, 1.0, "ignored", "2026-11-20")]   # not primary
            + [("waiver", "my own add", None, "28d", 1, 1.0, "user_only", "2026-11-20")])
    _seed(tmp_path, rows)
    assert tk.TrackBook.load(tmp_path, "espn", TODAY).record("waiver")["n"] == 0


def test_subkinds():
    assert tk.subkind_of("waiver", "Add A, drop B for this matchup") == "stream"
    assert tk.subkind_of("waiver", "Add A, drop B", "season_fpg") == "season"
    assert tk.subkind_of("lineup", "Swap IR slots: activate A, move B to IR") == "ir"
    assert tk.subkind_of("lineup", "Start Kirill Kaprizov over Ivan Irwin today") == "swap"
    assert tk.subkind_of("injury", "Hold Brad Marchand through his injury") == "hold_cut"
    assert tk.subkind_of("trade", "Trade A for B") is None


def test_dashboard_card_and_overview_strip(monkeypatch, tmp_path):
    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    from fantasy_manager.config import get_settings
    from fantasy_manager.web import views
    from fantasy_manager.web.app import create_app
    from tests.test_web import make_result

    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    get_settings.cache_clear()
    views._TRACK_BOOKS.clear()
    try:
        _seed(tmp_path, [("injury", "Move Jake Sanderson to IR", None, "28d", 1, 1.0, "followed", date.today().isoformat())])
        h = TestClient(create_app(lambda league: make_result())).get("/").text
        assert "Model track record" in h and "Building history" in h
        assert "not enough history yet (n=1 graded IR moves" in h          # on the IR card
    finally:
        get_settings.cache_clear()
        views._TRACK_BOOKS.clear()
