from datetime import datetime, timezone

from fantasy_manager.models import Player, normalize_name
from fantasy_manager.providers.news import NewsItem
from fantasy_manager.report.news_match import match_news_to_players, name_keys, player_news_summary


def P(cid, name, team="OTT", pos=("C",)):
    return Player(cid=cid, name=name, name_norm=normalize_name(name), ids={}, team=team, positions=list(pos))


def N(headline, player_name=None, blurb="", source="rotowire", day=1, id=None):
    return NewsItem(source=source, id=id, player_name=player_name, headline=headline, blurb=blurb,
                    published=datetime(2026, 9, day, 12, tzinfo=timezone.utc))


PLAYERS = [
    P("stutzle", "Tim Stützle"),
    P("necas", "Martin Nečas", "COL"),
    P("ovi", "Alex Ovechkin", "WSH", ("LW",)),
    P("aho-car", "Sebastian Aho", "CAR"),
    P("aho-nyi", "Sebastian Aho", "NYI", ("D",)),
    P("hughes", "Jack Hughes", "NJD"),
]


def test_rotowire_player_name_with_accents():
    items = [N("Scores twice", player_name="Tim Stutzle", id="a"),
             N("Out week-to-week", player_name="Martin Necas", id="b")]
    out = match_news_to_players(items, PLAYERS)
    assert [n.id for n in out["stutzle"]] == ["a"]
    assert [n.id for n in out["necas"]] == ["b"]


def test_rotowire_unmatched_and_ambiguous_are_dropped():
    items = [N("Nothing", player_name="Connor McDavid", id="x"),
             N("Two goals", player_name="Sebastian Aho", id="y")]  # duplicate name, no team -> pending
    out = match_news_to_players(items, PLAYERS)
    assert out == {}


def test_espn_headline_scan_exact_full_names():
    items = [
        N("Stützle, Nečas lead the way in preseason win", source="espn",
          blurb="Tim Stützle scored and Martin Necas added two assists.", id="e1"),
        N("Hughes brothers reunite", source="espn", blurb="Quinn and Luke talk Jack.", id="e2"),
        N("Alex Ovechkin chases another record", source="espn", id="e3"),
        N("Sebastian Aho signs extension", source="espn", id="e4"),  # ambiguous duplicate name
        N("Jack Hughesman is not a player", source="espn", id="e5"),  # no partial-word match
    ]
    out = match_news_to_players(items, PLAYERS)
    assert [n.id for n in out["stutzle"]] == ["e1"]
    assert [n.id for n in out["necas"]] == ["e1"]
    assert [n.id for n in out["ovi"]] == ["e3"]
    assert "hughes" not in out
    assert "aho-car" not in out and "aho-nyi" not in out


def test_dedupe_and_newest_first():
    items = [N("Old", player_name="Jack Hughes", day=1, id="1"),
             N("New", player_name="Jack Hughes", day=5, id="2"),
             N("New", player_name="Jack Hughes", day=5, id="2")]
    out = match_news_to_players(items, PLAYERS)
    assert [n.id for n in out["hughes"]] == ["2", "1"]


def test_name_keys_include_nickname_forms():
    keys = name_keys("Alexander Ovechkin")
    assert "alex ovechkin" in keys and "alexander ovechkin" in keys
    assert name_keys("Madonna") == set()


def test_player_news_summary():
    items = [N("Old news", day=1, blurb="x" * 500), N("Fresh news", day=9, blurb="Short.")]
    lines = player_news_summary(items, limit=1)
    assert lines == ["2026-09-09 (rotowire): Fresh news -- Short."]
    long = player_news_summary(items, limit=3)
    assert len(long) == 2 and long[1].endswith("…") and len(long[1]) < 260
