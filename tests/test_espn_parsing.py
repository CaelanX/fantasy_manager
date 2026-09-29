import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fantasy_manager.providers.base import ProviderError
from fantasy_manager.providers.espn import (canonical_stats, find_my_team, map_injury_status,
                                            parse_stat_lines, player_from_espn, pro_team_abbrev,
                                            roster_shape_from_counts, scoring_from_settings)
from fantasy_manager.scoring import PointsScoring

FIXTURE = json.loads((Path(__file__).parent / "fixtures" / "espn" / "player_stats_sample.json")
                     .read_text(encoding="utf-8"))
YEAR = FIXTURE["year"]


def espn_player(d):
    return SimpleNamespace(**d)


def test_canonical_stats_maps_and_drops_placeholders():
    raw = FIXTURE["players"][0]["stats"]["Total 2027"]["total"]
    cs = canonical_stats(raw)
    assert cs["PM"] == 3.0 and "+/-" not in cs
    assert cs["PTS"] == 13.0  # computed from G + A
    for junk in ("5", "12", "TTOI ?", "ATOI"):
        assert junk not in cs
    g = canonical_stats(FIXTURE["players"][1]["stats"]["Total 2027"]["total"])
    assert g["SVPCT"] == pytest.approx(0.917) and "MIN ?" not in g and "PTS" not in g


def test_parse_stat_lines_splits():
    lines = parse_stat_lines(FIXTURE["players"][0]["stats"], YEAR)
    assert set(lines) == {"season", "last7", "prior", "projected"}
    assert lines["season"].gp == 10 and lines["prior"].gp == 80 and lines["projected"].gp == 82
    assert lines["season"].per_game()["G"] == pytest.approx(0.5)
    goalie = parse_stat_lines(FIXTURE["players"][1]["stats"], YEAR)
    assert goalie["season"].per_game()["GAA"] == pytest.approx(2.5)  # rate stat not divided


def test_player_from_espn():
    mcd = player_from_espn(espn_player(FIXTURE["players"][0]), YEAR, pct_owned=99.9)
    assert mcd.cid == "espn:3895074" and mcd.ids == {"espn": "3895074"}
    assert mcd.team == "EDM" and mcd.positions == ["C", "F"] and mcd.status == "healthy"
    assert mcd.name_norm == "connor mcdavid" and mcd.pct_owned == 99.9
    g = player_from_espn(espn_player(FIXTURE["players"][1]), YEAR)
    assert g.team == "MTL" and g.positions == ["G"] and g.is_goalie
    fa = player_from_espn(espn_player(FIXTURE["players"][2]), YEAR)
    assert fa.team == "UTA" and fa.status == "dtd" and fa.status_note == "DAY_TO_DAY"
    assert fa.positions == ["RW", "F"] and "season" not in fa.lines


@pytest.mark.parametrize("name,abbr", [
    ("Montréal Canadiens", "MTL"), ("Montr?al Canadiens", "MTL"), ("Vegas Golden Knights", "VGK"),
    ("Seattle Kraken", "SEA"), ("Utah Hockey Club", "UTA"), ("Utah Mammoth", "UTA"),
    ("St. Louis Blues", "STL"), ("Unknown Team", None), (None, None)])
def test_pro_team_abbrev(name, abbr):
    assert pro_team_abbrev(name) == abbr


def test_every_espn_constant_team_maps():
    from espn_api.hockey.constant import PRO_TEAM_MAP

    for name in PRO_TEAM_MAP.values():
        assert pro_team_abbrev(name), name


def test_injury_status_mapping():
    assert map_injury_status("ACTIVE") == "healthy"
    assert map_injury_status("INJURY_RESERVE") == "ir"
    assert map_injury_status("SUSPENSION") == "suspended"
    assert map_injury_status("OUT") == "out"
    assert map_injury_status([]) == "healthy"  # espn_api json_parsing returns [] when absent
    assert map_injury_status(None, injured=True) == "unknown"


def test_scoring_from_settings_points():
    cfg = scoring_from_settings("H2H_POINTS", FIXTURE["scoringItems"])
    assert cfg.kind == "points"
    assert cfg.weights == {"G": 2.0, "A": 1.0, "PM": 0.5, "SOG": 0.1, "HIT": 0.1, "BLK": 0.5,
                           "PPP": 0.5, "W": 4.0, "GA": -2.0, "SV": 0.2, "SO": 3.0}  # statId 12 dropped
    # Reproduce season fantasy points for the fixture skater from its totals.
    season = parse_stat_lines(FIXTURE["players"][0]["stats"], YEAR)["season"]
    total = PointsScoring(cfg.weights).value(season.stats)
    assert total == pytest.approx(5 * 2 + 8 + 3 * 0.5 + 30 * 0.1 + 4 * 0.1 + 2 * 0.5 + 5 * 0.5)


def test_scoring_from_settings_categories():
    cfg = scoring_from_settings("H2H_CATEGORY", FIXTURE["scoringItems"])
    assert cfg.kind == "categories" and "G" in cfg.categories and cfg.weights["GA"] == -1.0
    assert scoring_from_settings("ROTO", FIXTURE["scoringItems"]).kind == "roto"


def test_roster_shape():
    assert roster_shape_from_counts(FIXTURE["lineupSlotCounts"]) == {
        "C": 2, "LW": 2, "RW": 2, "D": 4, "G": 2, "UTIL": 1, "BN": 5, "IR": 3}


def test_find_my_team():
    teams = [SimpleNamespace(team_id=1, team_name="Puck Luck", team_abbrev="PL",
                             owners=[{"id": "{AAAA-1111}"}]),
             SimpleNamespace(team_id=2, team_name="Ice Holes", team_abbrev="ICE",
                             owners=[{"id": "{BBBB-2222}"}])]
    assert find_my_team(teams, "{bbbb-2222}", None).team_id == 2
    assert find_my_team(teams, None, "1").team_id == 1
    assert find_my_team(teams, "{bbbb-2222}", "puck").team_id == 1  # explicit ESPN_TEAM wins
    with pytest.raises(ProviderError, match="1=Puck Luck"):
        find_my_team(teams, "{nobody}", None)


# -- harness capture: league activity and box-score lineups ----------------------

ACTIVITY = json.loads((Path(__file__).parent / "fixtures" / "espn" / "activity.json").read_text(encoding="utf-8"))


def test_parse_espn_activity_maps_adds_drops_trades():
    from fantasy_manager.providers.espn import parse_espn_activity

    items = parse_espn_activity(ACTIVITY["topics"], {"1": "Mine", "2": "Rival", "3": "Other"}, {101: "Added Guy"})
    got = [(i.action, i.cid, i.team_id, i.group_id, i.counterparty_id) for i in items]
    assert got == [("ADD", "espn:101", "1", "t-add", None), ("DROP", "espn:102", "1", "t-add", None),
                   ("TRADE_OUT", "espn:103", "1", "t-trade", "2"), ("TRADE_IN", "espn:103", "2", "t-trade", "1"),
                   ("TRADE_OUT", "espn:104", "2", "t-trade", "1"), ("TRADE_IN", "espn:104", "1", "t-trade", "2"),
                   ("DROP", "espn:105", "3", "t-drop", None), ("ADD", "espn:106", "3", "t-waiver", None)]
    assert items[0].player_name == "Added Guy" and items[0].team_name == "Mine" and items[0].source == "espn"
    assert items[0].ts.date().isoformat() == "2026-10-05"


class _FakeRequest:
    def __init__(self, pages):
        self.pages, self.offsets = pages, []

    def league_get(self, extend="", params=None, headers=None):
        f = json.loads(headers["x-fantasy-filter"])["topics"]
        self.offsets.append(f["offset"])
        return {"topics": self.pages[f["offset"] // f["limit"]] if f["offset"] // f["limit"] < len(self.pages) else []}


def _espn_provider(league):
    from fantasy_manager.providers.espn import EspnProvider

    prov = EspnProvider(SimpleNamespace(), cache=None)
    prov._league_obj = league
    return prov


def test_espn_activity_pages_until_since():
    from datetime import date, datetime, timedelta

    base = datetime(2026, 10, 30, 12)

    def topic(i):
        return {"id": f"t{i}", "date": int((base - timedelta(days=i)).timestamp() * 1000),
                "messages": [{"messageTypeId": 178, "targetId": i, "to": 1}]}
    pages = [[topic(i) for i in range(p * 25, p * 25 + 25)] for p in range(4)]
    req = _FakeRequest(pages)
    league = SimpleNamespace(espn_request=req, teams=[SimpleNamespace(team_id=1, team_name="Mine")], player_map={})
    prov = _espn_provider(league)
    items = prov.activity(since=date(2026, 9, 25))          # 35 days back: pages 0 and 1 only
    assert req.offsets == [0, 25]
    assert len(items) == 36 and min(i.ts.date() for i in items) == date(2026, 9, 25)
    req2 = _FakeRequest([[topic(i) for i in range(25)] for _ in range(20)])
    prov2 = _espn_provider(SimpleNamespace(espn_request=req2, teams=[], player_map={}))
    prov2.activity()                                         # no since: capped at 10 pages
    assert len(req2.offsets) == 10


def test_espn_activity_failure_is_a_warning():
    class Broken:
        def league_get(self, **kw):
            raise RuntimeError("boom")
    prov = _espn_provider(SimpleNamespace(espn_request=Broken(), teams=[], player_map={}))
    assert prov.activity() == [] and "boom" in prov.warnings[0]


def test_espn_box_scores_to_lineup_days():
    from datetime import date

    from fantasy_manager.providers.espn import parse_espn_box_lineups

    bp = lambda pid, slot, pts: SimpleNamespace(playerId=pid, slot_position=slot, points=pts)  # noqa: E731
    box = SimpleNamespace(home_team=SimpleNamespace(team_id=1), away_team=SimpleNamespace(team_id=2),
                          home_lineup=[bp(1, "Center", 4.5), bp(2, "Bench", 1.0), bp(3, "IR", 0)],
                          away_lineup=[bp(4, "Goalie", 7.2)])
    rows = parse_espn_box_lineups([box], date(2026, 10, 10))
    assert [(r.team_id, r.cid, r.slot, r.starting, r.provider_pts) for r in rows] == [
        ("1", "espn:1", "C", True, 4.5), ("1", "espn:2", "BN", False, 1.0), ("1", "espn:3", "IR", False, 0.0),
        ("2", "espn:4", "G", True, 7.2)]
    league = SimpleNamespace(current_week=12, box_scores=lambda **kw: [box])
    prov = _espn_provider(league)
    assert prov.scoring_period_for(date(2026, 10, 9), today=date(2026, 10, 10)) == 11
    assert len(prov.box_scores(11, date(2026, 10, 9))) == 4
    assert prov.box_scores(0) == []                             # before the first scoring period: no-op
