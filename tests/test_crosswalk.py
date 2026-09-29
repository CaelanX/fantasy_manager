import pytest

from fantasy_manager.matching.crosswalk import Crosswalk, nhl_candidates, parse_confirm, split_cid
from fantasy_manager.matching.matcher import Candidate
from fantasy_manager.models import Player, normalize_name
from fantasy_manager.providers.nhl import NhlClient

from .test_nhl import FakeFetch


def pl(cid, name, team, pos):
    return Player(cid=cid, name=name, name_norm=normalize_name(name), ids={"espn": cid.split(":")[1]},
                  team=team, positions=pos)


@pytest.fixture
def cands():
    client = NhlClient(fetch_json=FakeFetch(), season=20262027)
    return nhl_candidates(client.all_skaters(20252026), client.goalie_summary(20252026),
                          client.team_roster("EDM"),
                          [Candidate(key=8478427, name="Sebastian Aho", team="CAR", position="C"),
                           Candidate(key=8480222, name="Sebastian Aho", team="NYI", position="D")])


def test_nhl_candidates_merge_rows_and_rosters(cands):
    by_id = {c.key: c for c in cands}
    assert by_id[8478402].name == "Connor McDavid" and by_id[8478402].team == "EDM"
    assert by_id[8477934].name == "Leon Draisaitl"          # roster-only (no season row in fixture)
    assert by_id[8476883].position == "G"                     # goalie summary row


def test_resolve_and_persist(tmp_path, cands):
    players = [pl("espn:1", "Connor McDavid", "EDM", ["C", "F"]),
               pl("espn:2", "Leon Draisaitl", "EDM", ["C", "F"]),
               pl("espn:3", "Nathan McKinnon", "COL", ["C", "F"]),     # typo -> fuzzy "high"
               pl("espn:4", "Sebastian Aho", None, ["C", "F"]),         # duplicate name, no team -> ...
               pl("espn:5", "Nobody Atall", "EDM", ["D"])]
    xw = Crosswalk(tmp_path)
    stats = xw.resolve(players, cands)
    assert [p.cid for p in players] == [f"espn:{i}" for i in range(1, 6)]   # cid never changes
    assert players[0].nhl_id == 8478402 and players[0].ids["nhl"] == "8478402"
    assert players[1].nhl_id == 8477934
    assert players[2].nhl_id == 8477492
    assert players[4].nhl_id is None
    # Aho: position C picks the CAR Aho but without a team the match is only "high"
    assert players[3].nhl_id == 8478427
    assert stats.resolved == 4 and stats.new == 4 and stats.unmatched == 1
    assert xw.get("espn", "1").confidence == "exact"
    assert xw.get("espn", "5").confidence == "none"
    xw.close()

    # a fresh instance reuses stored matches without an index
    xw2 = Crosswalk(tmp_path)
    again = [pl("espn:1", "C. McDavid", "EDM", ["C"])]
    assert xw2.resolve(again, []).resolved == 1 and again[0].nhl_id == 8478402


def test_pending_review_and_confirm(tmp_path):
    cands = [Candidate(key=1, name="Elias Pettersson", team="VAN", position="C"),
             Candidate(key=2, name="Elias Pettersson", team="VAN", position="C")]
    xw = Crosswalk(tmp_path)
    p = pl("espn:77", "Elias Pettersson", "VAN", ["C"])
    stats = xw.resolve([p], cands)
    assert stats.pending == 1 and p.nhl_id is None
    pend = xw.pending()
    assert [(r.source, r.source_id, r.confidence) for r in pend] == [("espn", "77", "pending")]
    row = xw.confirm("espn", "77", 2)
    assert row.confidence == "confirmed" and row.name == "Elias Pettersson"
    assert xw.pending() == []
    p2 = pl("espn:77", "Elias Pettersson", "VAN", ["C"])
    xw.resolve([p2], cands)
    assert p2.nhl_id == 2                        # confirmed rows are never re-matched


def test_provider_supplied_nhl_id_is_kept(tmp_path):
    p = pl("fantrax:abc", "Someone", "EDM", ["C"])
    p.ids["nhl"] = "123"
    xw = Crosswalk(tmp_path)
    assert xw.resolve([p], []).resolved == 1
    assert xw.get("fantrax", "abc").nhl_id == 123


def test_parse_confirm():
    assert parse_confirm("espn:123=8478402") == ("espn", "123", 8478402)
    assert split_cid("fantrax:x:y") == ("fantrax", "x:y")
    for bad in ("espn123=1", "espn:1=abc", "espn:1"):
        with pytest.raises(ValueError):
            parse_confirm(bad)
