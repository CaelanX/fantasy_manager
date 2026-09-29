import pytest

from fantasy_manager.matching.matcher import Candidate, build_index, candidates_from, match_player
from fantasy_manager.matching.normalize import (
    normalize_name, normalize_positions, normalize_team, positions_agree,
)


@pytest.mark.parametrize("a,b", [
    ("Martin Nečas", "Martin Necas"),
    ("T.J. Oshie", "TJ Oshie"),
    ("T. J. Oshie", "TJ Oshie"),
    ("J.T. Miller", "JT Miller"),
    ("Alex Ovechkin", "Alexander Ovechkin"),
    ("Mitch Marner", "Mitchell Marner"),
    ("Matt Boldy", "Matthew Boldy"),
    ("Nick Suzuki", "Nicholas Suzuki"),
    ("Zach Werenski", "Zachary Werenski"),
    ("Josh Morrissey", "Joshua Morrissey"),
    ("Chris Tanev", "Christopher Tanev"),
    ("Mike Matheson", "Michael Matheson"),
    ("Will Smith", "William Smith"),
    ("Tim Stützle", "Tim Stutzle"),
    ("Jesperi Kotkaniemi", "JESPERI  KOTKANIEMI"),
    ("Ryan O'Reilly", "Ryan OReilly"),
    ("Ryan O’Reilly", "Ryan O'Reilly"),
    ("Jacob Bernard-Docker", "Jacob Bernard Docker"),
    ("Lukas Dostál", "Lukas Dostal"),
    ("Oliver Ekman-Larsson", "Oliver Ekman Larsson"),
    ("Mads Søgaard", "Mads Sogaard"),
    ("Tim Stützle Jr.", "Tim Stutzle"),
])
def test_normalize_equivalences(a, b):
    assert normalize_name(a) == normalize_name(b)


def test_normalize_details():
    assert normalize_name("Martin Nečas") == "martin necas"
    assert normalize_name("T.J. Oshie") == "tj oshie"
    assert normalize_name("Alex Ovechkin") == "alexander ovechkin"
    assert normalize_name("  Juraj   Slafkovský ") == "juraj slafkovsky"
    assert normalize_name("") == ""
    assert normalize_name(None) == ""
    # nickname canonicalization only touches the first name
    assert normalize_name("Nick Alex") == "nicholas alex"
    # unsafe short forms are left alone
    assert normalize_name("Jake Guentzel") == "jake guentzel"


def test_team_and_position_normalization():
    assert normalize_team("NJ") == "NJD"
    assert normalize_team("tb") == "TBL"
    assert normalize_team("Utah") == "UTA"
    assert normalize_team("EDM") == "EDM"
    assert normalize_team(None) is None
    assert normalize_positions("C/LW") == {"C", "LW"}
    assert normalize_positions("L") == {"LW"}
    assert normalize_positions("F") == {"C", "LW", "RW"}
    assert positions_agree("C", "LW")          # forwards agree loosely
    assert not positions_agree("C", "D")
    assert positions_agree("R", "RW")
    assert not positions_agree(None, "C")


POOL = [
    Candidate(key=8478427, name="Sebastian Aho", team="CAR", position="C"),
    Candidate(key=8480222, name="Sebastian Aho", team="NYI", position="D"),
    Candidate(key=8471214, name="Alexander Ovechkin", team="WSH", position="L"),
    Candidate(key=8478483, name="Mitchell Marner", team="VGK", position="R"),
    Candidate(key=8480039, name="Martin Necas", team="COL", position="C"),
    Candidate(key=8471698, name="T.J. Oshie", team="WSH", position="R"),
    Candidate(key=8478402, name="Connor McDavid", team="EDM", position="C"),
    Candidate(key=8477934, name="Leon Draisaitl", team="EDM", position="C"),
    Candidate(key=8479318, name="Auston Matthews", team="TOR", position="C"),
    Candidate(key=8481559, name="Jack Hughes", team="NJD", position="C"),
    Candidate(key=8480800, name="Quinn Hughes", team="MIN", position="D"),
]


def test_exact_matches():
    idx = build_index(POOL)
    r = idx.match("Nečas", team="COL")  # last-name-only fuzzy vs full name is too weak
    assert r.confidence in ("pending", "none")
    r = idx.match("Martin Nečas")
    assert (r.key, r.confidence, r.score) == (8480039, "exact", 100.0)
    assert idx.match("TJ Oshie").key == 8471698
    assert idx.match("TJ Oshie").confidence == "exact"
    assert idx.match("Alex Ovechkin", team="WSH").key == 8471214
    assert idx.match("Mitch Marner").confidence == "exact"
    assert idx.match("Mitch Marner").matched


def test_duplicate_name_tie_break_by_team():
    idx = build_index(POOL)
    car = idx.match("Sebastian Aho", team="CAR")
    nyi = idx.match("Sebastian Aho", team="NYI")
    assert (car.key, car.confidence) == (8478427, "exact")
    assert (nyi.key, nyi.confidence) == (8480222, "exact")


def test_duplicate_name_tie_break_by_position():
    idx = build_index(POOL)
    d = idx.match("Sebastian Aho", position="D")
    f = idx.match("Sebastian Aho", position="C/LW")
    assert (d.key, d.confidence) == (8480222, "high")
    assert (f.key, f.confidence) == (8478427, "high")


def test_duplicate_name_ambiguous_is_pending():
    r = match_player("Sebastian Aho", POOL)
    assert r.confidence == "pending" and not r.matched


def test_fuzzy_high_needs_agreement():
    # misspelling scores >= 92 and team agrees -> high
    r = match_player("Leon Draisaitel", POOL, team="EDM")
    assert (r.key, r.confidence) == (8477934, "high")
    assert r.score >= 92
    # same misspelling without team/position agreement -> pending
    r = match_player("Leon Draisaitel", POOL)
    assert (r.key, r.confidence) == (8477934, "pending")


def test_fuzzy_pending_band_and_none():
    r = match_player("Austin Mathews", POOL, team="TOR")
    assert r.key == 8479318 and r.confidence in ("pending", "high")
    assert 85 <= r.score
    r = match_player("Wayne Gretzky", POOL)
    assert r.confidence == "none" and r.key is None
    assert match_player("", POOL).confidence == "none"
    assert match_player("Connor McDavid", []).confidence == "none"


def test_fuzzy_prefers_team_agreement():
    # "Hughes" siblings: an incomplete first name should lean on team
    r = match_player("Jak Hughes", POOL, team="NJD")
    assert r.key == 8481559


def test_candidates_from_objects_and_dicts():
    class P:
        def __init__(self, player_id, name, team, position):
            self.player_id, self.name, self.team, self.position = player_id, name, team, position

    cands = candidates_from([P(1, "Nikita Kucherov", "TBL", "RW"),
                             {"player_id": 2, "name": "Brayden Point", "team": "TBL", "position": "C"}])
    assert [c.key for c in cands] == [1, 2]
    assert match_player("Brayden Point", cands).key == 2
