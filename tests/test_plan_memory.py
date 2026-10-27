"""Plan memory (recommend/plan_memory.py): done steps, data-driven change reasons, stability and
long-term options, compared with the last plan built for the same matchup period."""
from fantasy_manager.models import FantasyTeam, RosterSlot
from fantasy_manager.recommend.plan import build_week_plan, plan_summary
from fantasy_manager.recommend.plan_memory import since_line
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.valuate import valuate_league

from .test_plan import DAYS, MON, NO_GOALIES, ctx_for, matchup, mk


def plan_mem(ctx, data_dir, stance="even"):
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    return build_week_plan(ctx, values, recs=[], matchup=matchup(stance), goalies=NO_GOALIES, data_dir=data_dir)


SCHED = {"MON": [DAYS[0]], "TW": DAYS[1:3], "FS": DAYS[4:7], "TT": DAYS[1:4], "NONE": []}


def test_executed_step_is_done_and_the_rest_carries_on(tmp_path):
    first = plan_mem(ctx_for([("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))],
                             [mk("x", "TW", 2.0), mk("y", "FS", 2.0)], SCHED), tmp_path)
    assert [(m.add.cid, m.drop.cid) for m in first.moves] == [("x", "weak"), ("y", "x")]
    assert first.since is None                                   # nothing to compare with yet
    # the user made the first move: x replaced weak; one move left
    ctx = ctx_for([("C", mk("my_c", "MON", 2.0)), ("BN", mk("x", "TW", 2.0))], [mk("y", "FS", 2.0)], SCHED, moves=1)
    p = plan_mem(ctx, tmp_path)
    assert p.since is not None and p.since.done == ["Done: added x, dropped weak (Tue)"]
    assert [(m.add.cid, m.drop.cid) for m in p.moves] == [("y", "x")]
    assert p.moves[0].status != "new"                            # same step as before
    assert not p.since.changed
    # the block stays while nothing changes
    again = plan_mem(ctx, tmp_path)
    assert again.since is not None and again.since.done == p.since.done


def test_removed_step_gets_the_reason_from_the_data(tmp_path):
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    plan_mem(ctx_for(mine, [mk("x", "TW", 2.0), mk("y", "FS", 2.0)], SCHED), tmp_path)
    ctx = ctx_for(mine, [mk("x", "TW", 2.0)], SCHED)
    ctx.teams.append(FantasyTeam(team_id="2", name="rival", owner_is_me=False,
                                 slots=[RosterSlot(slot="BN", player=mk("y", "FS", 2.0), starting=False)]))
    p = plan_mem(ctx, tmp_path)
    assert any(c.startswith("y for x (+") and c.endswith("y was picked up by rival") for c in p.since.changed)


def test_stability_keeps_an_earlier_step_unless_clearly_beaten(tmp_path):
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    first = plan_mem(ctx_for(mine, [mk("a", "TT", 2.0)], SCHED, moves=1), tmp_path)
    assert [m.add.cid for m in first.moves] == ["a"]
    # b is a little better (+0.6 pts over three games): the plan keeps a
    p = plan_mem(ctx_for(mine, [mk("a", "TT", 2.0), mk("b", "TT", 2.2)], SCHED, moves=1), tmp_path)
    assert [m.add.cid for m in p.moves] == ["a"] and p.moves[0].status == "kept"
    assert p.since is not None and p.since.kept and "under the 1.5-pt bar" in p.since.kept[0]
    # c is much better: it replaces a, with both gains in the reason
    q = plan_mem(ctx_for(mine, [mk("a", "TT", 2.0), mk("c", "TT", 5.0)], SCHED, moves=1), tmp_path)
    assert [m.add.cid for m in q.moves] == ["c"] and q.moves[0].status == "new"
    assert any("replaced by c for weak (+" in x and "with 1 move left, the best one is c" in x for x in q.since.changed)


def test_long_term_option_for_a_dropped_keeper(tmp_path):
    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    plan_mem(ctx_for(mine, [mk("keeper", "FS", 3.0)], SCHED, moves=1), tmp_path)
    p = plan_mem(ctx_for(mine, [mk("keeper", "FS", 3.0), mk("s", "TT", 5.0)], SCHED, moves=1), tmp_path)
    assert [m.add.cid for m in p.moves] == ["s"]
    lt = [w for w in p.watch if w.startswith("Long-term option: keeper is a better long-term C than weak")]
    assert lt and "Mon Oct 12 when moves reset" in lt[0]
    assert since_line(p).startswith("Since your last plan")
    assert any(x.startswith("Since your last plan") for x in plan_summary(p, MON))


def test_plan_page_shows_since_your_last_plan(tmp_path, monkeypatch):
    import pytest

    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    import fantasy_manager.recommend.plan as plan_mod
    from fantasy_manager.web.app import create_app
    from tests.test_web import make_result

    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    plan_mem(ctx_for(mine, [mk("keeper", "FS", 3.0)], SCHED, moves=1), tmp_path)
    p = plan_mem(ctx_for(mine, [mk("keeper", "FS", 3.0), mk("s", "TT", 5.0)], SCHED, moves=1), tmp_path)
    monkeypatch.setattr(plan_mod, "build_week_plan", lambda *a, **k: p)
    html = TestClient(create_app(lambda league: make_result(league), home="plan")).get("/?league=espn").text
    assert "Since your last plan" in html and "keeper for weak (+" in html and 'class="tag tag-new"' in html


def test_open_spot_after_an_ir_move_yields_a_no_drop_add(tmp_path):
    # the user put "hurt" on IR: the bench spot is open, so the plan adds without a drop
    mine = [("C", mk("my_c", "MON", 2.0)), ("IR", mk("hurt", "NONE", 2.0, status="ir"))]
    p = plan_mem(ctx_for(mine, [mk("x", "TW", 2.0)], SCHED), tmp_path)
    assert [(m.add.cid, m.drop, m.to_ir) for m in p.moves] == [("x", None, None)]
    assert "open roster spot, no drop" in p.moves[0].headline


def test_moved_banner_seen_and_not_seen(tmp_path):
    from fantasy_manager.recommend.plan_memory import latest_snapshot, moved_banner

    before = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("hurt", "NONE", 2.0, status="ir"))]
    plan_mem(ctx_for(before, [mk("x", "TW", 2.0)], SCHED), tmp_path)
    since = latest_snapshot(tmp_path, "test")["created"]
    # not on the league yet: same roster
    text, warn, again = moved_banner(tmp_path, "test", since, ctx_for(before, [], SCHED), "hurt to IR")
    assert text.startswith("Not showing yet (hurt to IR): test can take a minute") and warn and again
    text, warn, again = moved_banner(tmp_path, "test", since, ctx_for(before, [], SCHED), retry=True)
    assert text.startswith("Still not showing") and warn and not again
    # seen: hurt is on IR and the bench spot is open
    after = [("C", mk("my_c", "MON", 2.0)), ("IR", mk("hurt", "NONE", 2.0, status="ir"))]
    text, warn, again = moved_banner(tmp_path, "test", since, ctx_for(after, [], SCHED))
    assert text.startswith("✓ Seen: hurt → IR · 1 open roster spot") and not warn and not again


def test_moved_endpoint_redirects_to_the_plan_with_a_banner(monkeypatch, tmp_path):
    import pytest

    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    import fantasy_manager.recommend.plan as plan_mod
    import fantasy_manager.recommend.plan_memory as pm
    from fantasy_manager.web.app import create_app
    from tests.test_web import make_result

    mine = [("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))]
    p = plan_mem(ctx_for(mine, [mk("x", "TW", 2.0)], SCHED), tmp_path)
    monkeypatch.setattr(plan_mod, "build_week_plan", lambda *a, **k: p)
    c = TestClient(create_app(lambda league: make_result(league), home="plan"))
    r = c.post("/moved", data={"league": "espn", "since": "2026-10-26T19:25:26+00:00", "label": "Add x"},
               follow_redirects=False)
    assert r.status_code == 303 and r.headers["location"].startswith("/plan?")
    assert "msg=moved" in r.headers["location"] and "label=Add+x" in r.headers["location"]
    monkeypatch.setattr(pm, "moved_banner", lambda *a, **k: ("✓ Seen: Hellebuyck → IR · 1 open roster spot", False, False))
    seen = c.get(r.headers["location"]).text
    assert "✓ Seen: Hellebuyck → IR · 1 open roster spot" in seen and 'id="moved-retry"' not in seen
    assert 'action="/moved"' in seen and "I made a move" in seen
    monkeypatch.setattr(pm, "moved_banner", lambda *a, **k: ("Not showing yet: ESPN can take a minute", True, True))
    later = c.get(r.headers["location"]).text
    assert "Not showing yet" in later and 'id="moved-retry"' in later and "/static/moved.js" in later


def test_snapshots_are_appended_only_on_change(tmp_path):
    ctx = ctx_for([("C", mk("my_c", "MON", 2.0)), ("BN", mk("weak", "NONE", 0.1))], [mk("x", "TW", 2.0)], SCHED)
    for _ in range(3):
        plan_mem(ctx, tmp_path)
    files = list((tmp_path / "plans").glob("*.json"))
    assert len(files) == 1 and files[0].name == f"test-{MON.isoformat()}.json"
    import json

    assert len(json.loads(files[0].read_text(encoding="utf-8"))) == 1
