import random

import pytest

from fantasy_manager.models import Player
from fantasy_manager.valuation.adjust import availability_multiplier
from fantasy_manager.valuation.replacement import eligible_slots, replacement_levels, vorp


def mk(cid, positions):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=None, positions=positions)


def test_vorp_uses_best_slot():
    repl = {"C": 2.0, "LW": 1.0, "RW": 1.5, "D": 1.2, "G": 3.0}
    assert vorp(3.0, ["C", "LW", "F"], repl) == pytest.approx(2.0)  # LW is the scarcer slot
    assert vorp(3.0, ["C"], repl) == pytest.approx(1.0)
    assert vorp(3.0, ["G"], repl) == pytest.approx(0.0)


def test_eligibility_rules():
    assert eligible_slots(["F"]) == ["C", "LW", "RW"]
    assert eligible_slots(["C", "F"], ["C", "F", "UTIL", "G"]) == ["C", "F", "UTIL"]
    assert eligible_slots(["G"], ["C", "UTIL", "G"]) == ["G"]
    assert eligible_slots(["D"], ["C", "D", "UTIL"]) == ["D", "UTIL"]


def test_replacement_is_top3_mean_with_zero_padding():
    players = {c: mk(c, ["C"]) for c in "abcd"}
    repl = replacement_levels({"a": 3.0, "b": 2.0, "c": 1.0, "d": 0.5}, players, ["C", "D"])
    assert repl["C"] == pytest.approx(2.0)
    assert repl["D"] == 0.0
    assert replacement_levels({"a": 3.0}, players, ["C"])["C"] == pytest.approx(1.0)


def test_adding_free_agent_never_lowers_replacement():
    rng = random.Random(7)
    positions = [["C"], ["LW"], ["RW"], ["D"], ["G"], ["C", "LW"], ["F"], ["LW", "RW"]]
    for _ in range(200):
        players, vals = {}, {}
        for i in range(rng.randint(0, 8)):
            cid = f"p{i}"
            players[cid] = mk(cid, rng.choice(positions))
            vals[cid] = rng.uniform(-1, 4)
        before = replacement_levels(vals, players)
        players["new"] = mk("new", rng.choice(positions))
        vals["new"] = rng.uniform(-1, 4)
        after = replacement_levels(vals, players)
        for slot in before:
            assert after[slot] >= before[slot] - 1e-12


@pytest.mark.parametrize("status,week,season", [
    ("healthy", 1.0, 1.0), ("dtd", 0.75, 0.75), ("out", 0.0, 0.6), ("ir", 0.0, 0.4),
    ("ltir", 0.0, 0.1), ("suspended", 0.0, 0.5), ("unknown", 1.0, 1.0)])
def test_availability(status, week, season):
    assert availability_multiplier(status, "week") == week
    assert availability_multiplier(status, "season") == season
