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
