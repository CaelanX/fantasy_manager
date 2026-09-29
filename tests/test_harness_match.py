"""Matching rules of harness.match (pure functions over ledger rows) plus one end-to-end run."""
import json
from datetime import date, datetime


from fantasy_manager.harness import Ledger, ingest_archive, match_episodes, pull_transactions
from fantasy_manager.harness.match import (EpisodeRow, LineupRow, TxRow, first_lockable_day, match_rows,
                                           window_end)
from fantasy_manager.models import ActivityItem

ME = "1"
D = date


def ep(kind, first, last=None, adds=(), drops=(), subjects=(), title="", eid="e1"):
    return EpisodeRow(episode_id=eid, kind=kind, first_seen=first, last_seen=last or first, title=title,
                      adds=set(adds), drops=set(drops), subjects=set(subjects))


def tx(action, cid, day, group="g1", team=ME, other=None):
    return TxRow(tx_id=group, action=action, cid=cid, team_id=team, day=day, group_id=group, counterparty_id=other)


def run(episodes, txs=(), lineups=(), today=D(2026, 10, 20), provider="espn"):
    res = match_rows(episodes, txs, lineups, ME, provider, today, "espn")
    return {e["episode_id"]: e for e in res.episodes}, res.decisions


def test_exact_waiver_is_followed():
    eps, dec = run([ep("waiver", D(2026, 10, 5), adds=["a"], drops=["d"])],
                   [tx("ADD", "a", D(2026, 10, 6)), tx("DROP", "d", D(2026, 10, 6))])
    assert eps["e1"]["status"] == "followed" and eps["e1"]["acted_on"] == "2026-10-06"
    assert [d["origin"] for d in dec] == ["followed"]                 # the group is consumed: no user_only


def test_waiver_with_a_different_drop_is_partial():
    eps, dec = run([ep("waiver", D(2026, 10, 5), adds=["a"], drops=["d"])],
                   [tx("ADD", "a", D(2026, 10, 5)), tx("DROP", "x", D(2026, 10, 5))])
    assert eps["e1"]["status"] == "partial"
    assert json.loads(eps["e1"]["match_json"])["dropped"] == ["x"]
    assert [d["origin"] for d in dec] == ["partial"]


def test_add_outside_window_is_user_only_and_episode_expires():
    e = ep("waiver", D(2026, 10, 5), adds=["a"], drops=["d"])
    assert window_end(e, "espn") == D(2026, 10, 8)
    eps, dec = run([e], [tx("ADD", "a", D(2026, 10, 10)), tx("DROP", "d", D(2026, 10, 10))])
    assert eps["e1"]["status"] == "expired"
    assert [(d["origin"], d["kind"], json.loads(d["adds_json"])) for d in dec] == [("user_only", "waiver", ["a"])]
    # before the window closes the unmatched episode stays open
    eps, _ = run([e], [], today=D(2026, 10, 8))
    assert eps["e1"]["status"] == "open"


def test_other_teams_moves_are_not_mine():
    eps, dec = run([ep("waiver", D(2026, 10, 5), adds=["a"])], [tx("ADD", "a", D(2026, 10, 6), team="2")])
    assert eps["e1"]["status"] == "expired" and dec == []


def test_two_for_one_trade_subset_is_partial_and_exact_is_followed():
    e = ep("trade", D(2026, 10, 5), adds=["x", "y"], drops=["m"])
    assert window_end(e, "espn") == D(2026, 10, 12)
    part = [tx("TRADE_IN", "x", D(2026, 10, 9), "T"), tx("TRADE_OUT", "m", D(2026, 10, 9), "T")]
    eps, dec = run([e], part)
    assert eps["e1"]["status"] == "partial" and dec[0]["origin"] == "partial"
    full = part + [tx("TRADE_IN", "y", D(2026, 10, 9), "T")]
    assert run([e], full)[0]["e1"]["status"] == "followed"
    # an unrelated trade sharing only a get is neither
    other = [tx("TRADE_IN", "x", D(2026, 10, 9), "U"), tx("TRADE_OUT", "z", D(2026, 10, 9), "U")]
    eps, dec = run([e], other)
    assert eps["e1"]["status"] == "expired" and dec[0]["origin"] == "user_only" and dec[0]["kind"] == "trade"


def test_trade_proposal_only_is_proposed():
    e = ep("trade", D(2026, 10, 5), adds=["x"], drops=["m"])
    props = [TxRow("P", "PROPOSED", "x", ME, D(2026, 10, 6), "P", "2"),       # I receive x from team 2
             TxRow("P", "PROPOSED", "m", "2", D(2026, 10, 6), "P", ME)]       # I give m to team 2
    eps, dec = run([e], props, today=D(2026, 10, 7))
    assert eps["e1"]["status"] == "proposed" and dec[0]["origin"] == "proposed"


def test_fantrax_lineup_rec_issued_tuesday_is_graded_the_following_monday():
    tue, mon = D(2026, 10, 6), D(2026, 10, 12)
    assert tue.weekday() == 1 and first_lockable_day("fantrax", tue) == mon
    assert first_lockable_day("fantrax", mon) == mon and first_lockable_day("espn", tue) == tue
    e = ep("lineup", tue, adds=["in"], drops=["out"], title="Start in over out for this week's lineup (locks Monday)")
    tue2 = D(2026, 10, 13)
    before = [LineupRow(ME, D(2026, 10, 7), "in", "BN", False), LineupRow(ME, D(2026, 10, 7), "out", "C", True),
              LineupRow(ME, mon, "in", "BN", False), LineupRow(ME, mon, "out", "C", True)]   # Monday 8am: pre-lock
    after = [LineupRow(ME, tue2, "in", "C", True), LineupRow(ME, tue2, "out", "BN", False)]
    eps, _ = run([e], lineups=before + after, today=D(2026, 10, 14), provider="fantrax")
    assert eps["e1"]["status"] == "followed" and eps["e1"]["acted_on"] == "2026-10-12"
    assert json.loads(eps["e1"]["match_json"])["graded_day"] == "2026-10-13"
    # before a post-lock snapshot exists the episode stays open (the Wednesday / Monday ones don't count)
    eps, _ = run([e], lineups=before, today=D(2026, 10, 12), provider="fantrax")
    assert eps["e1"]["status"] == "open"
    # post-lock snapshot unchanged -> expired (graded as ignored)
    same = [LineupRow(ME, tue2, "in", "BN", False), LineupRow(ME, tue2, "out", "C", True)]
    eps, _ = run([e], lineups=same, today=D(2026, 10, 14), provider="fantrax")
    assert eps["e1"]["status"] == "expired"
    # a rec issued on the Monday itself is graded against that Monday's lock
    eps, _ = run([ep("lineup", mon, adds=["in"], drops=["out"])], lineups=before, today=mon, provider="fantrax")
    assert eps["e1"]["status"] == "open"


def test_espn_lineup_partial_when_only_one_side_changes():
    e = ep("lineup", D(2026, 10, 10), adds=["in"], drops=["out"])
    rows = [LineupRow(ME, D(2026, 10, 10), "in", "C", True), LineupRow(ME, D(2026, 10, 10), "out", "UTIL", True)]
    assert run([e], lineups=rows)[0]["e1"]["status"] == "partial"


def test_injury_ir_within_two_days():
    e = ep("injury", D(2026, 10, 6), adds=["fa"], subjects=["hurt"], title="Move hurt to IR and add fa")
    ok = [LineupRow(ME, D(2026, 10, 8), "hurt", "IR", False)]
    eps, dec = run([e], [tx("ADD", "fa", D(2026, 10, 7))], ok)
    assert eps["e1"]["status"] == "followed" and dec[0]["origin"] == "followed"
    late = [LineupRow(ME, D(2026, 10, 9), "hurt", "IR", False)]            # 3 days later: outside the window
    eps, _ = run([e], [], late)
    assert eps["e1"]["status"] == "expired"
    eps, _ = run([e], [tx("IR", "hurt", D(2026, 10, 6))])                  # IR move only, no add
    assert eps["e1"]["status"] == "partial"


def test_activation_rec_followed_when_player_leaves_ir():
    e = ep("injury", D(2026, 10, 6), adds=["back"], title="Activate back from IR")
    rows = [LineupRow(ME, D(2026, 10, 6), "back", "IR", False), LineupRow(ME, D(2026, 10, 7), "back", "BN", False)]
    assert run([e], lineups=rows)[0]["e1"]["status"] == "followed"


def test_flags_trade_within_14_days():
    sell = ep("sell_high", D(2026, 10, 1), drops=["hot"], eid="s")
    buy = ep("buy_low", D(2026, 10, 1), adds=["cold"], eid="b")
    eps, _ = run([sell, buy], [tx("TRADE_OUT", "hot", D(2026, 10, 14), "T"),
                               tx("TRADE_IN", "cold", D(2026, 10, 20), "U")], today=D(2026, 10, 30))
    assert eps["s"]["status"] == "followed" and eps["b"]["status"] == "expired"


# --------------------------------------------------------------------------- end to end

def _write_recs(tmp_path, day, recs, league="espn"):
    d = tmp_path / "archive"
    d.mkdir(exist_ok=True)
    (d / f"recs-{league}-{day}.json").write_text(json.dumps(
        {"version": 2, "provider": league, "as_of": day, "recommendations": recs}), encoding="utf-8")


def _rec(kind, title, add=(), drop=(), subjects=(), counterparty=None, gain=0.5):
    ref = lambda c: {"cid": c, "name": c, "nhl_id": None, "team": "EDM", "positions": ["C"]}  # noqa: E731
    return {"kind": kind, "score": 1.0, "title": title, "add": [ref(c) for c in add], "drop": [ref(c) for c in drop],
            "subjects": [ref(c) for c in subjects], "counterparty": counterparty, "predicted_gain": gain,
            "gain_units": "season_fpg", "horizon_days": None, "strength": 5.0, "reasons": []}


class FakeProvider:
    def __init__(self, items):
        self.items = items

    def activity(self, since=None):
        return [i for i in self.items if since is None or i.ts.date() >= since]


class FakeCtx:
    def __init__(self, team_id="1"):
        self.my_team = type("T", (), {"team_id": team_id})()

    def all_players(self):
        return []


def test_end_to_end_ingest_pull_match_is_idempotent(tmp_path):
    for day in ("2026-10-05", "2026-10-06", "2026-10-07"):
        _write_recs(tmp_path, day, [_rec("waiver", "Add a, drop d", add=["espn:a"], drop=["espn:d"])])
    ledger = Ledger(tmp_path)
    ingest_archive(ledger, tmp_path)
    items = [ActivityItem(source="espn", tx_id="t1", ts=datetime(2026, 10, 8, 9), team_id="1", action="ADD",
                          cid="espn:a", group_id="t1"),
             ActivityItem(source="espn", tx_id="t1", ts=datetime(2026, 10, 8, 9), team_id="1", action="DROP",
                          cid="espn:d", group_id="t1"),
             ActivityItem(source="espn", tx_id="t2", ts=datetime(2026, 10, 8, 10), team_id="1", action="ADD",
                          cid="espn:z", group_id="t2"),
             ActivityItem(source="espn", tx_id="t3", ts=datetime(2026, 10, 8, 11), team_id="4", action="ADD",
                          cid="espn:q", group_id="t3")]
    assert pull_transactions(ledger, FakeProvider(items), "espn", date(2026, 10, 1), FakeCtx()) == 4
    s1 = match_episodes(ledger, "espn", today=date(2026, 10, 20))
    assert s1.by_status == {"followed": 1} and s1.decisions == {"followed": 1, "user_only": 1}
    ep_row = ledger.query("SELECT * FROM rec_episodes")[0]
    assert (ep_row["first_seen"], ep_row["last_seen"], ep_row["n_days"]) == ("2026-10-05", "2026-10-07", 3)
    assert ep_row["acted_on"] == "2026-10-08"                        # last_seen + 3 window
    assert ledger.count("transactions", "WHERE is_me=1") == 3
    snapshot = [ledger.query(f"SELECT * FROM {t} ORDER BY 1, 2") for t in ("rec_episodes", "decisions",
                                                                            "transactions")]
    ingest_archive(ledger, tmp_path)
    pull_transactions(ledger, FakeProvider(items), "espn", date(2026, 10, 1), FakeCtx())
    s2 = match_episodes(ledger, "espn", today=date(2026, 10, 20))
    again = [ledger.query(f"SELECT * FROM {t} ORDER BY 1, 2") for t in ("rec_episodes", "decisions",
                                                                        "transactions")]
    strip = lambda rows: [{k: v for k, v in r.items() if k not in ("updated_at", "created_at", "pulled_at")}  # noqa
                          for r in rows]
    assert [strip(x) for x in again] == [strip(x) for x in snapshot]
    assert s2.by_status == s1.by_status
    ledger.close()


def test_proposal_keeps_its_first_seen_time(tmp_path):
    def prop(day):
        return ActivityItem(source="fantrax", tx_id="P", ts=datetime(2026, 10, day), team_id="1", action="PROPOSED",
                            cid="fantrax:x", group_id="P", counterparty_id="2")
    with Ledger(tmp_path) as led:
        pull_transactions(led, FakeProvider([prop(5)]), "fantrax", None, FakeCtx())
        pull_transactions(led, FakeProvider([prop(9)]), "fantrax", None, FakeCtx())
        assert led.query("SELECT day, is_me FROM transactions") == [{"day": "2026-10-05", "is_me": 1}]


# --------------------------------------------------------------------------- alerts

def alert(first, subjects, title, counterparty=None, negative=False, eid="a1", last=None):
    e = ep("alert", first, last, subjects=subjects, title=title, eid=eid)
    e.counterparty, e.negative = counterparty, negative
    return e


def test_free_agent_alert_followed_by_my_add_is_followed_like_a_waiver():
    e = alert(D(2026, 10, 5), ["misa"], "Preseason standout: Michael Misa (4 GP, 1.25 PTS/GP; free agent)", "FA")
    assert window_end(e, "fantrax") == D(2026, 10, 8)
    eps, dec = run([e], [tx("ADD", "misa", D(2026, 10, 7)), tx("DROP", "old", D(2026, 10, 7))], provider="fantrax")
    assert eps["a1"]["status"] == "followed" and eps["a1"]["acted_on"] == "2026-10-07"
    m = json.loads(eps["a1"]["match_json"])
    assert m["added"] == ["misa"] and m["dropped"] == ["old"] and m["alert"] == "fa"
    assert [(d["origin"], d["kind"], json.loads(d["adds_json"]), json.loads(d["drops_json"])) for d in dec] == \
        [("followed", "alert", ["misa"], ["old"])]                      # the group is consumed: no user_only


def test_free_agent_alert_add_after_the_window_expires():
    e = alert(D(2026, 10, 5), ["x"], "X (free agent) promoted to PP1 (PP2 -> PP1): waiver watch")
    eps, dec = run([e], [tx("ADD", "x", D(2026, 10, 9))])
    assert eps["a1"]["status"] == "expired"
    assert [(d["origin"], d["kind"]) for d in dec] == [("user_only", "waiver")]


def test_alert_on_a_player_on_no_roster_counts_as_free_agent():
    e = alert(D(2026, 10, 5), ["x"], "Rookie role signal: X - skating on the top line (RotoWire, 2026-10-05)")
    rostered = [LineupRow("2", D(2026, 10, 5), "y", "C", True)]
    eps, _ = run([e], [tx("ADD", "x", D(2026, 10, 6))], rostered)
    assert eps["a1"]["status"] == "followed"
    # on another team's roster that day: informational, my later add does not follow it
    eps, _ = run([e], [tx("ADD", "x", D(2026, 10, 6))], rostered + [LineupRow("2", D(2026, 10, 5), "x", "C", True)])
    assert eps["a1"]["status"] == "expired"


def test_negative_alert_on_my_player_followed_by_drop_or_trade():
    e = alert(D(2026, 10, 5), ["m"], "Role loss: M (TOI -3.1, off PP1; my team)", "Me")
    assert window_end(e, "espn") == D(2026, 10, 12)
    eps, dec = run([e], [tx("DROP", "m", D(2026, 10, 11)), tx("ADD", "n", D(2026, 10, 11))])
    assert eps["a1"]["status"] == "followed"
    m = json.loads(eps["a1"]["match_json"])
    assert (m["via"], m["dropped"], m["added"]) == ("drop", ["m"], ["n"])
    assert [d["origin"] for d in dec] == ["followed"]
    eps, _ = run([e], [tx("TRADE_OUT", "m", D(2026, 10, 9), "T"), tx("TRADE_IN", "t", D(2026, 10, 9), "T")])
    assert eps["a1"]["status"] == "followed" and json.loads(eps["a1"]["match_json"])["via"] == "trade"
    # scratched / off PP1 titles and negative rookie news count as negative too
    assert run([alert(D(2026, 10, 5), ["m"], "M out of the lineup (scratched): bench caution")],
               [tx("DROP", "m", D(2026, 10, 6))])[0]["a1"]["status"] == "followed"
    assert run([alert(D(2026, 10, 5), ["m"], "Rookie role signal: M - sent down", negative=True)],
               [tx("DROP", "m", D(2026, 10, 6))])[0]["a1"]["status"] == "followed"


def test_positive_alert_on_my_player_then_drop_is_not_followed():
    e = alert(D(2026, 10, 5), ["m"], "M promoted to PP1 (PP2 -> PP1): start him / hold")
    eps, dec = run([e], [tx("DROP", "m", D(2026, 10, 6))])
    assert eps["a1"]["status"] == "expired"
    assert [d["origin"] for d in dec] == ["user_only"]


def test_followed_alert_is_graded_and_ignored_alerts_are_not(tmp_path):
    from fantasy_manager.harness.outcomes import load_subjects

    _write_recs(tmp_path, "2026-10-05", [
        _rec("alert", "Preseason standout: M (4 GP; free agent)", subjects=["espn:m"], counterparty="FA", gain=None),
        _rec("alert", "Rising: Q (+6.0% rostered this week)", subjects=["espn:q"], counterparty="FA", gain=None)])
    with Ledger(tmp_path) as ledger:
        ingest_archive(ledger, tmp_path)
        items = [ActivityItem(source="espn", tx_id="t1", ts=datetime(2026, 10, 6, 9), team_id="1", action="ADD",
                              cid="espn:m", group_id="t1"),
                 ActivityItem(source="espn", tx_id="t1", ts=datetime(2026, 10, 6, 9), team_id="1", action="DROP",
                              cid="espn:d", group_id="t1")]
        pull_transactions(ledger, FakeProvider(items), "espn", date(2026, 10, 1), FakeCtx())
        s = match_episodes(ledger, "espn", today=date(2026, 10, 20))
        assert s.by_kind_status == {"alert": {"followed": 1, "expired": 1}}
        assert s.decisions == {"followed": 1}
        subs = {(x.title.split(":")[0], x.basis): x for x in load_subjects(ledger, "espn")}
        first = subs[("Preseason standout", "first_seen")]
        assert (first.adds, first.drops, first.origin) == (["espn:m"], ["espn:d"], "followed")
        assert (subs[("Preseason standout", "acted_on")].adds, subs[("Preseason standout", "acted_on")].drops) == \
            (["espn:m"], ["espn:d"])
        ignored = subs[("Rising", "first_seen")]
        assert ignored.origin == "ignored" and ignored.adds == [] and ignored.drops == []   # never graded
