import json
import math
from datetime import date
from pathlib import Path

import pytest

from fantasy_manager.cache import HttpCache
from fantasy_manager.config import Settings
from fantasy_manager.models import Player, StatLine
from fantasy_manager.providers.base import ProviderError
from fantasy_manager.providers.fantrax import (FPTS_KEY, FantraxProvider, canonical_slot, data_split,
                                               fit_weights, load_cookies, parse_cookie_header,
                                               parse_cookie_text, parse_league_rules, parse_player_pool,
                                               parse_roster, player_positions, rules_weights,
                                               season_from_settings, status_from_icons, timeframe_codes,
                                               validate_weights, weights_check)
from fantasy_manager.scoring import PointsScoring

FIX = Path(__file__).parent / "fixtures" / "fantrax"
POINTS = "G=3,A=2,SOG=0.4,HIT=0.3,BLK=0.5,PPP=0.5,W=4,SV=0.2,GA=-1,SO=3"
TODAY = date(2026, 9, 28)


def fx(name):
    return json.loads((FIX / f"{name}.json").read_text(encoding="utf-8"))


POSITIONS = {k: v["shortName"] for k, v in fx("league_info")["positionMap"].items()}


class FakeResponse:
    def __init__(self, body, status=200):
        self._body = body
        self.status_code = status
        self.reason = "OK"

    def json(self):
        if isinstance(self._body, Exception):
            raise self._body
        return self._body


class FakeSession:
    """Answers fxpa POSTs from fixtures by method name (teamId / page for rosters and pool)."""

    def __init__(self, overrides=None, page_errors=None, raise_for=None):
        self.overrides = overrides or {}
        self.page_errors = page_errors or {}
        self.raise_for = raise_for or {}
        self.calls = []

    def data_for(self, method, data):
        if method in self.overrides:
            o = self.overrides[method]
            return o(data) if callable(o) else o
        if method == "getFantasyLeagueInfo":
            return fx("league_info")
        if method == "getTeamRosterInfo":
            return fx(f"roster_{data.get('teamId', 't1')}")
        if method == "getLeagueRulesOld":
            return {"newJoin": False, "content": ""}
        return fx({"getStandings": "standings", "getTradeBlocks": "trade_blocks",
                   "getPendingTransactions": "pending_trades",
                   "getTransactionDetailsHistory": "transactions",
                   "getPlayerStats": "player_pool"}[method])

    def post(self, url, params=None, json=None, timeout=None):
        assert url == "https://www.fantrax.com/fxpa/req" and params == {"leagueId": "lg1"}
        methods = [m["method"] for m in json["msgs"]]
        self.calls.append(json["msgs"])
        for m in methods:
            assert all(isinstance(v, str) for v in next(x for x in json["msgs"] if x["method"] == m)["data"].values())
            if m in self.raise_for:
                raise self.raise_for[m]
            if m in self.page_errors:
                return FakeResponse({"pageError": self.page_errors[m]})
        return FakeResponse({"responses": [{"data": self.data_for(m["method"], m["data"])} for m in json["msgs"]]})


def settings(tmp_path, **kw):
    base = dict(_env_file=None, fantrax_league_id="lg1", fantrax_cookie="FX_RM=secret; JSESSIONID=abc",
                fantrax_points=POINTS, fantrax_dynasty=True, fm_data_dir=tmp_path)
    base.update(kw)
    return Settings(**base)


def provider(tmp_path, session=None, **kw):
    s = settings(tmp_path, **kw)
    return FantraxProvider(s, HttpCache(tmp_path), session=session or FakeSession(), today=TODAY)


# -- pure helpers -------------------------------------------------------------

def test_parse_cookie_header():
    assert parse_cookie_header("Cookie: FX_RM=a=b; JSESSIONID=xyz ; empty=") == \
        {"FX_RM": "a=b", "JSESSIONID": "xyz", "empty": ""}
    assert parse_cookie_header('a="quoted"') == {"a": "quoted"}


def test_parse_cookie_text_netscape_and_json():
    txt = ("# Netscape HTTP Cookie File\n"
           ".fantrax.com\tTRUE\t/\tTRUE\t1893456000\tFX_RM\tremember\n"
           "#HttpOnly_www.fantrax.com\tFALSE\t/\tTRUE\t0\tJSESSIONID\tsess\n"
           ".espn.com\tTRUE\t/\tFALSE\t0\tSWID\t{nope}\n")
    assert parse_cookie_text(txt) == {"FX_RM": "remember", "JSESSIONID": "sess"}
    assert parse_cookie_text("FX_RM=x; b=y\n") == {"FX_RM": "x", "b": "y"}
    js = json.dumps([{"name": "FX_RM", "value": "v", "domain": ".fantrax.com"},
                     {"name": "other", "value": "o", "domain": ".example.com"}])
    assert parse_cookie_text(js) == {"FX_RM": "v"}


def test_load_cookies_from_file(tmp_path):
    f = tmp_path / "cookies.txt"
    f.write_text(".fantrax.com\tTRUE\t/\tTRUE\t0\tFX_RM\tfromfile\n", encoding="utf-8")
    s = settings(tmp_path, fantrax_cookie=None, fantrax_cookie_file=f)
    assert load_cookies(s) == {"FX_RM": "fromfile"}
    with pytest.raises(ProviderError, match="does not exist"):
        load_cookies(settings(tmp_path, fantrax_cookie=None, fantrax_cookie_file=tmp_path / "nope.txt"))


@pytest.mark.parametrize("raw,slot", [("C", "C"), ("Util", "UTIL"), ("Skt", "UTIL"), ("Res", "BN"),
                                      ("IR", "IR"), ("Min", "MIN"), ("Taxi", "TAXI"), ("<b>D</b>", "D")])
def test_canonical_slot(raw, slot):
    assert canonical_slot(raw) == slot


def test_positions_and_status_helpers():
    assert player_positions("<b>C</b>,RW") == ["C", "RW", "F"]
    assert player_positions("D") == ["D"]
    assert player_positions("G") == ["G"]
    assert player_positions("C,D") == ["C", "F", "D"]
    assert status_from_icons([{"typeId": "1", "tooltip": "Day-to-Day"}, {"typeId": "2", "tooltip": "IR"}]) == ("ir", "IR")
    assert status_from_icons([{"typeId": "30"}]) == ("out", None)
    assert status_from_icons([{"typeId": "6", "tooltip": "Suspended"}]) == ("suspended", "Suspended")
    assert status_from_icons([{"typeId": "9", "tooltip": "news"}]) == ("healthy", None)


def test_season_from_settings():
    assert season_from_settings(fx("league_info")["fantasySettings"]) == 2027
    assert season_from_settings({"subtitle": "2026-27 Season"}) == 2027
    assert season_from_settings({}, date(2026, 9, 28)) == 2027


def test_parse_roster_slots_statuses_and_stats():
    r = parse_roster(fx("roster_t1"), POSITIONS)
    by_slot = [(s.slot, s.player.name if s.player else None, s.starting) for s in r.slots]
    assert by_slot == [("C", "Young Star", True), ("LW", "Veteran Winger", True), ("D", "Steady Dman", True),
                       ("UTIL", None, True), ("BN", "Bench Guy", False), ("IR", "Hurt Center", False),
                       ("MIN", "Prospect Kid", False), ("G", "Top Goalie", True)]
    players = {s.player.cid: s.player for s in r.slots if s.player}
    star = players["fantrax:p1"]
    assert star.ids == {"fantrax": "p1"} and star.team == "NJD" and star.positions == ["C", "RW", "F"]
    season = star.lines["season"]
    assert season.gp == 10 and season.stats["G"] == 5 and season.stats["PTS"] == 12 and season.stats["PM"] == 3
    assert season.stats["HIT"] == 5 and season.stats["BLK"] == 3 and season.stats[FPTS_KEY] == 46.0
    assert players["fantrax:p2"].status == "dtd" and players["fantrax:p2"].status_note == "Lower Body - Day-to-Day"
    assert players["fantrax:p2"].team == "TBL" and players["fantrax:p3"].team == "LAK"
    assert players["fantrax:p4"].team == "WSH"
    hurt = players["fantrax:p5"]
    assert hurt.status == "ir" and hurt.team == "MTL" and "season" not in hurt.lines
    assert "season" not in players["fantrax:p6"].lines
    goalie = players["fantrax:p7"]
    assert goalie.team == "VGK" and goalie.is_goalie
    g = goalie.lines["season"]
    assert g.stats["SVPCT"] == pytest.approx(0.927) and g.stats["GAA"] == pytest.approx(2.2)
    assert "PTS" not in g.stats and g.per_game()["SVPCT"] == pytest.approx(0.927)
    assert r.fantasy_points["fantrax:p1"] == (46.0, 4.6)
    assert r.ages["fantrax:p6"] == 19
    assert r.slot_counts == {"C": 1, "LW": 1, "D": 1, "UTIL": 1, "G": 1}
    assert r.limits["Reserve"] == (1, 6) and r.limits["Minors"] == (1, 5)


def test_parse_roster_derives_gp_from_fpts():
    r = parse_roster(fx("roster_t2"), POSITIONS)
    players = {s.player.cid: s.player for s in r.slots if s.player}
    assert players["fantrax:r1"].lines["season"].gp == 10
    assert players["fantrax:r1"].lines["season"].stats == {"GP": 10.0, FPTS_KEY: 50.0}
    assert "season" not in players["fantrax:r3"].lines and players["fantrax:r3"].status == "suspended"
    assert len(r.slots) == 3  # empty reserve row dropped


def test_parse_player_pool_skips_rostered_and_percent_columns():
    players, rows, pages = parse_player_pool(fx("player_pool"))
    assert pages == 1 and [p.cid for p in players] == ["fantrax:f1", "fantrax:f2", "fantrax:p4"]
    f1 = players[0]
    assert f1.pct_owned == 12 and rows[f1.cid].age == 23 and f1.positions == ["C", "LW", "F"]
    assert "PM" not in f1.lines["season"].stats  # "+/-" there is roster % change, not plus-minus
    assert f1.lines["season"].gp == 10


def _player(cid, gp, stats, fpts):
    s = dict(stats, GP=gp)
    if fpts is not None:
        s[FPTS_KEY] = fpts
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=None, positions=["C"],
                  lines={"season": StatLine(split="season", gp=gp, stats=s)})


def test_validate_weights_error_math():
    w = {"G": 3.0, "A": 2.0}
    players = [
        _player("a", 10, {"G": 5, "A": 5}, 25.0),   # exact: 2.5 FP/G
        _player("b", 10, {"G": 2, "A": 0}, 8.0),    # predicted 0.6 vs 0.8 -> 0.2
        _player("c", 0, {}, 0.0),                   # no games: ignored
        _player("d", 5, {"SOG": 10}, 4.0),          # no weighted stat: ignored
        _player("e", 5, {"G": 1}, None),            # no Fantrax FPts: ignored
    ]
    assert validate_weights(w, players) == pytest.approx(0.1)
    assert math.isnan(validate_weights(w, players[2:]))


# -- provider end to end ------------------------------------------------------

def test_provider_load(tmp_path):
    prov = provider(tmp_path)
    ctx = prov.load()
    assert ctx.provider == "fantrax" and ctx.league_id == "lg1" and ctx.season == 2027
    assert ctx.name == "Test Dynasty League" and ctx.dynasty and ctx.keeper_horizon_years == 3
    assert ctx.as_of == TODAY
    assert ctx.my_team.team_id == "t1" and ctx.my_team.name == "Caelan's Crushers"
    assert ctx.my_team.record == (2, 2, 0)
    assert [t.record for t in ctx.teams if t.team_id == "t2"] == [(3, 1, 0)]
    assert ctx.roster_shape == {"C": 1, "LW": 1, "D": 1, "UTIL": 1, "G": 1, "BN": 6, "IR": 3, "MIN": 5}
    assert ctx.scoring.kind == "points" and ctx.scoring.weights["G"] == 3.0
    assert [p.cid for p in ctx.free_agents] == ["fantrax:f1", "fantrax:f2"]  # rostered p4 / r1 excluded
    assert prov.weights_mae == pytest.approx(0.0)
    assert prov.fantasy_points["fantrax:f1"] == (30.0, 3.0) and prov.ages["fantrax:r2"] == 35
    assert prov.trade_blocks[0].team_name == "Rival Team" and prov.trade_blocks[0].note == "Looking for D help"
    assert prov.trade_blocks[0].players_offered == ["fantrax:r1"]
    assert prov.trade_blocks[0].positions_wanted == ["D"] and prov.trade_blocks[0].stats_wanted == ["Blk"]
    assert len(prov.trade_blocks) == 1
    assert prov.pending_trades[0].moves[1]["pick"] == {"year": 2027, "round": 1, "original_owner": "t1"}
    assert [(t.kind, t.player_cid) for t in prov.transactions] == [("FA", "fantrax:p4"), ("DROP", "fantrax:x9")]
    # the real 8-column player-pool layout has FPts/FP/G but no per-stat columns
    assert len(prov.warnings) == 1 and "free-agent list has no per-stat columns" in prov.warnings[0]
    assert prov.weights_checked == 5  # p1, p2, p3, p4, p7 have itemized stats


def test_describe_settings_and_raw(tmp_path):
    prov = provider(tmp_path)
    lines = prov.describe_settings()
    text = "\n".join(lines)
    assert "Test Dynasty League" in text and "My team: Caelan's Crushers (t1)" in text
    assert "Minors 1/5" in text and "mean abs error 0.000 FP/G vs Fantrax FPts over 5 players" in text
    assert "fantasySettings.rosterSettings.maxMinorsPlayers: 5" in text
    assert "fantasySettings.draftSettings.futureDraftPicksTradeable: True" in text
    assert "secret" not in text
    raw = prov.raw_settings()
    assert raw["roster_limits"]["Inj Res"] == {"total": 1, "max": 3}
    assert raw["rules"]["fantasySettings.rosterSettings.keeperLimit"] == "ALL"


def test_free_agent_failure_sets_warning(tmp_path):
    sess = FakeSession(page_errors={"getPlayerStats": {"code": "UNEXPECTED_ERROR", "title": "boom"}})
    prov = provider(tmp_path, sess)
    ctx = prov.load()
    assert ctx.free_agents == [] and len(ctx.teams) == 2
    assert any("free-agent pool unavailable" in w for w in prov.warnings)
    # the skater and goalie queries were both tried
    assert sum(1 for c in sess.calls if c[0]["method"] == "getPlayerStats"
               and c[0]["data"]["statusOrTeamFilter"] == "ALL_AVAILABLE") == 2


def test_free_agent_pool_queries_goalies_separately(tmp_path):
    def pool(data):
        if data.get("positionOrGroup") == "POS_705":  # league_info positionMap: 705 = G
            return {"statsTable": []}
        return fx("player_pool")
    sess = FakeSession(overrides={"getPlayerStats": pool})
    prov = provider(tmp_path, sess)
    assert [p.cid for p in prov.load().free_agents] == ["fantrax:f1", "fantrax:f2"]
    groups = [c[0]["data"]["positionOrGroup"] for c in sess.calls if c[0]["method"] == "getPlayerStats"
              and c[0]["data"]["statusOrTeamFilter"] == "ALL_AVAILABLE"]
    assert groups == ["HOCKEY_SKATING", "POS_705"]
    assert not any("unavailable" in w for w in prov.warnings)


def test_extras_failures_are_warnings(tmp_path):
    import requests
    sess = FakeSession(raise_for={"getTradeBlocks": requests.ConnectionError("down")},
                       page_errors={"getStandings": {"code": "UNEXPECTED_ERROR", "title": "nope"}})
    prov = provider(tmp_path, sess)
    ctx = prov.load()
    assert ctx.my_team.record is None and prov.trade_blocks == []
    assert any(w.startswith("Standings unavailable") for w in prov.warnings)
    assert any(w.startswith("Trade blocks unavailable") for w in prov.warnings)


def test_not_logged_in_is_provider_error(tmp_path):
    sess = FakeSession(page_errors={"getFantasyLeagueInfo": {"code": "WARNING_NOT_LOGGED_IN"}})
    with pytest.raises(ProviderError, match="FANTRAX_COOKIE") as e:
        provider(tmp_path, sess).load()
    assert "secret" not in str(e.value)


def test_not_member_and_non_json(tmp_path):
    sess = FakeSession(page_errors={"getFantasyLeagueInfo": {"code": "NOT_MEMBER_OF_LEAGUE"}})
    with pytest.raises(ProviderError, match="not a member of league lg1"):
        provider(tmp_path, sess).load()

    class HtmlSession(FakeSession):
        def post(self, *a, **k):
            return FakeResponse(ValueError("html"), status=200)
    with pytest.raises(ProviderError, match="non-JSON"):
        provider(tmp_path / "x", HtmlSession()).load()


def test_missing_config_errors(tmp_path):
    with pytest.raises(ProviderError, match="FANTRAX_LEAGUE_ID"):
        FantraxProvider(settings(tmp_path, fantrax_league_id=None), HttpCache(tmp_path)).load()
    with pytest.raises(ProviderError, match="FANTRAX_COOKIE"):
        FantraxProvider(settings(tmp_path, fantrax_cookie=None), HttpCache(tmp_path)).load()


def test_team_selection(tmp_path):
    ctx = provider(tmp_path, fantrax_team="rival").load()
    assert ctx.my_team.team_id == "t2"

    def no_mine(data):
        d = fx(f"roster_{data.get('teamId', 't1')}")
        d["myTeamIds"] = []
        return d
    with pytest.raises(ProviderError, match="t1=Caelan's Crushers, t2=Rival Team"):
        provider(tmp_path / "b", FakeSession(overrides={"getTeamRosterInfo": no_mine})).load()
    with pytest.raises(ProviderError, match="matches no team"):
        provider(tmp_path / "c", fantrax_team="nobody").load()


def test_fpts_fallback_scoring_without_points(tmp_path):
    prov = provider(tmp_path, fantrax_points="")
    ctx = prov.load()
    assert ctx.scoring.weights == {FPTS_KEY: 1.0}
    assert any("FANTRAX_POINTS is not set" in w for w in prov.warnings)
    from fantasy_manager.scoring import from_config
    star = next(p for p in ctx.all_players() if p.cid == "fantrax:p1")
    assert from_config(ctx.scoring).value(star.lines["season"].per_game()) == pytest.approx(4.6)


def test_cache_and_offline(tmp_path):
    sess = FakeSession()
    provider(tmp_path, sess).load()
    n = len(sess.calls)
    assert n > 0 and not any("secret" in p.read_text() for p in (tmp_path / "fantrax_cache").iterdir())

    class Boom(FakeSession):
        def post(self, *a, **k):
            raise AssertionError("network used")
    ctx = provider(tmp_path, Boom(), fm_offline=True).load()  # served from the JSON cache
    assert ctx.my_team.team_id == "t1" and len(ctx.free_agents) == 2
    with pytest.raises(ProviderError, match="Offline"):
        provider(tmp_path / "empty", Boom(), fm_offline=True).load()


# -- live-shaped fixtures (trimmed real responses, fantasy team ids/names scrubbed) ---------

LIVE_POSITIONS = {"207": "F", "202": "D", "201": "G"}
PROJ = "PROJECTION_0_31n_SEASON"
YTD = "SEASON_31n_YEAR_TO_DATE"
RULE_WEIGHTS = {"A": 2.0, "BLK": 0.5, "ENG": 2.0, "G": 4.0, "HIT": 0.3, "FT": 3.0, "PIM": 0.5, "SHG": 2.0,
                "SOG": 0.5, "PPP": 1.0, "GA": -1.0, "SV": 0.25, "SO": 7.5, "W": 5.0}


def test_period_view_gp_is_not_season_gp():
    """The default roster view is 'Projected - Per Game' for the current scoring period: its GP
    column counts the period's games (1-4), so it must never become a season line."""
    period = fx("roster_period")
    assert data_split(period) is None
    r = parse_roster(period, LIVE_POSITIONS)
    eichel = next(s.player for s in r.slots if s.player and s.player.name == "Jack Eichel")
    header = [c["shortName"] for c in period["tables"][0]["header"]["cells"]]
    row = next(x for x in period["tables"][0]["rows"] if x.get("scorer", {}).get("name") == "Jack Eichel")
    assert row["cells"][header.index("GP")]["content"] == "3"   # period games, not season GP
    assert eichel.lines == {} and r.fantasy_points == {} and r.ages[eichel.cid] == 29

    proj = parse_roster(fx("roster_projected"), LIVE_POSITIONS)
    assert proj.split == "projected"
    e = next(s.player for s in proj.slots if s.player and s.player.cid == eichel.cid)
    line = e.lines["projected"]
    assert set(e.lines) == {"projected"} and line.gp == 76 and line.stats["G"] == 28
    assert line.stats["ENG"] == 0 and line.stats["FT"] == 0 and line.stats[FPTS_KEY] == 440.4
    goalie = next(s.player for s in proj.slots if s.player and s.player.is_goalie)
    assert goalie.lines["projected"].stats["SO"] == 3 and goalie.lines["projected"].stats["SV"] == 990

    ytd = fx("roster_ytd")   # the season has not started: YTD rows are all zero -> no season line
    assert data_split(ytd) == "season"
    assert all(not s.player.lines for s in parse_roster(ytd, LIVE_POSITIONS).slots if s.player)
    # once games are played, the YTD GP column is season GP
    row = next(x for x in ytd["tables"][0]["rows"] if x.get("scorer", {}).get("name") == "Jack Eichel")
    hdr = [c["shortName"] for c in ytd["tables"][0]["header"]["cells"]]
    for col, v in (("GP", "5"), ("FPts", "30"), ("FP/G", "6"), ("G", "3")):
        row["cells"][hdr.index(col)]["content"] = v
    e = next(s.player for s in parse_roster(ytd, LIVE_POSITIONS).slots if s.player and s.player.name == "Jack Eichel")
    assert e.lines["season"].gp == 5 and e.lines["season"].stats["G"] == 3


def test_timeframe_codes():
    codes = timeframe_codes(fx("roster_period"))
    assert codes["season"] == {"seasonOrProjection": YTD, "timeframeTypeCode": "YEAR_TO_DATE"}
    assert codes["prior"]["seasonOrProjection"] == "SEASON_31l_YEAR_TO_DATE"
    assert codes["projected"] == {"seasonOrProjection": PROJ, "timeframeTypeCode": "PROJECTED_SEASON"}
    assert timeframe_codes({}) == {} and data_split({}) == "season"


def test_goalie_pool_positions_stats_owned_age():
    players, rows, _ = parse_player_pool(fx("pool_goalies_projected"))
    assert [p.name for p in players] == ["Sergei Murashov", "Jake Allen", "Kevin Lankinen"]
    m = players[0]
    assert m.positions == ["G"] and m.is_goalie and m.pct_owned == 64 and rows[m.cid].age == 22
    line = m.lines["projected"]
    assert line.gp == 47 and {k: line.stats[k] for k in ("W", "GA", "SV", "SO")} == \
        {"W": 25, "GA": 127, "SV": 1180, "SO": 3}
    assert "PTS" not in line.stats
    prior, _, _ = parse_player_pool(fx("pool_goalies_prior"))
    assert set(prior[0].lines) == {"prior"}
    period, _, _ = parse_player_pool(fx("pool_goalies_period"))
    assert all(not p.lines for p in period)   # per-period projection: no stat line


def test_league_rules_parse():
    rules = parse_league_rules(fx("league_rules")["content"])
    assert rules.get("Maximum Total Players") == "16" and rules.get_int("Maximum Active Players") == 10
    assert rules.get("Maximum Reserve Players") == "6"
    assert rules.get("Maximum Injury Reserve Players") == "Not Used"
    assert rules.get("Maximum Minor League Players") == "Not Used"
    assert rules.get("Keeper league Type") == "Dynasty"
    assert rules.get("Allow trading of draft Picks") == "Yes"
    assert rules.get_int("Number of future years for draft pick trading") == 3
    assert rules.get_int("Number of rounds available for draft pick trading") == 10
    assert rules.get("Lineup changes are executed") == "Weekly every Monday"
    assert rules.get_int("Playoffs will begin in this Scoring Period") == 25
    assert rules.get_int("Number of teams qualifying for playoffs") == 6
    assert {p: v["max_active"] for p, v in rules.positions.items()} == {"F": 5, "D": 3, "G": 2}
    assert rules.scoring["Skaters"]["Ft"] == 3 and rules.scoring["Goalies"]["G"] == 20
    base, goalie, notes = rules_weights(rules)
    assert base == RULE_WEIGHTS and goalie == {"A": 3.0, "G": 20.0} and notes == []


def _synthetic(n, goalie, weights, goalie_weights=None, seed=1):
    import random
    rnd = random.Random(seed)
    out = []
    scoring = PointsScoring(weights, goalie_weights or {})
    for i in range(n):
        if goalie:
            gp = rnd.randint(10, 60)
            st = {"W": rnd.randint(0, gp), "GA": rnd.randint(gp, 3 * gp), "SV": rnd.randint(20 * gp, 30 * gp),
                  "SO": rnd.randint(0, 5), "A": rnd.randint(0, 3), "G": 1 if i == 0 else 0, "PIM": rnd.randint(0, 4)}
        else:
            gp = rnd.randint(20, 82)
            st = {"G": rnd.randint(0, 50), "A": rnd.randint(0, 70), "SOG": rnd.randint(50, 300),
                  "HIT": rnd.randint(0, 250), "BLK": rnd.randint(0, 150), "PPP": rnd.randint(0, 30),
                  "PIM": rnd.randint(0, 80), "ENG": rnd.randint(0, 4), "FT": rnd.randint(0, 3),
                  "SHG": rnd.randint(0, 3)}
            st["PTS"] = st["G"] + st["A"]          # derived aggregate: collinear with G and A
        fpts = round(scoring.value(st), 4)
        st = {k: float(v) for k, v in st.items()}
        st.update(GP=float(gp), **{FPTS_KEY: fpts})
        out.append(Player(cid=f"{'g' if goalie else 's'}{i}", name=f"p{i}", name_norm=f"p{i}", ids={},
                          team=None, positions=["G"] if goalie else ["F"],
                          lines={"projected": StatLine(split="projected", gp=gp, stats=st)}))
    return out


def test_fit_weights_recovers_league_rules():
    base, goalie = dict(RULE_WEIGHTS), {"A": 3.0, "G": 20.0}
    players = _synthetic(60, False, base) + _synthetic(25, True, base, goalie, seed=2)
    res = fit_weights(players)
    assert res.weights == base and res.goalie_weights == goalie
    assert res.n == 85 and res.mae < 1e-6 and res.residual_max < 1e-6
    assert res.points_line.startswith("FANTRAX_POINTS=G=4,A=2,")
    assert set(res.points_line.split(",")[-2:]) == {"goalie.A=3", "goalie.G=20"}
    # wrong weights show up in the check, the fitted ones do not
    assert weights_check({"G": 3, "A": 2}, players)[0] > 1
    assert weights_check(res.weights, players, res.goalie_weights)[0] < 1e-6


def test_fit_weights_rounds_and_drops_tiny():
    base = {"G": 4.0, "A": 2.0, "SOG": 0.02, "HIT": 0.333}
    res = fit_weights(_synthetic(40, False, base))
    assert res.weights == {"G": 4.0, "A": 2.0, "HIT": 0.33} and res.goalie_weights == {}
    assert 0 < res.mae < 0.2 and fit_weights([]).n == 0


def test_points_parser_goalie_group_and_scoring():
    from fantasy_manager.config import format_points, parse_points, split_points
    from fantasy_manager.models import ScoringConfig
    from fantasy_manager.scoring import from_config
    pts = parse_points("G=4, a=2, Fights=3, goalie.G=20, Goalie.a=3")
    assert pts == {"G": 4.0, "A": 2.0, "FT": 3.0, "goalie.G": 20.0, "goalie.A": 3.0}
    base, goalie = split_points(pts)
    assert base == {"G": 4.0, "A": 2.0, "FT": 3.0} and goalie == {"G": 20.0, "A": 3.0}
    assert parse_points(format_points(base, goalie)) == pts
    assert Settings(_env_file=None, fantrax_points="G=4,goalie.G=20").fantrax_points == {"G": 4.0, "goalie.G": 20.0}
    sc = from_config(ScoringConfig(kind="points", weights={"G": 4, "W": 5, "SV": 0.25}, goalie_weights={"G": 20}))
    assert sc.value({"G": 1.0}) == 4.0                          # skater line
    assert sc.value({"G": 1.0, "W": 1.0, "SV": 20.0}) == 30.0   # goalie line: goalie G = 20
    assert sc.breakdown({"G": 1.0, "GA": 2.0}) == {"G": 20.0}


class LiveSession(FakeSession):
    """Answers like the real league: per-period default view, explicit timeframes, rules page."""

    def data_for(self, method, data):
        if method in self.overrides:
            return super().data_for(method, data)
        tf = data.get("seasonOrProjection")
        if method == "getFantasyLeagueInfo":
            info = fx("league_info")
            info["positionMap"] = {k: {"id": k, "shortName": v} for k, v in LIVE_POSITIONS.items()}
            return info
        if method == "getTeamRosterInfo":
            d = fx({None: "roster_period", PROJ: "roster_projected", YTD: "roster_ytd"}[tf])
            if data.get("teamId") == "t2":
                for t in d["tables"]:
                    t["rows"] = [r for r in t["rows"] if not r.get("scorer")]
            return d
        if method == "getPlayerStats":
            if data.get("statusOrTeamFilter") == "ALL_TAKEN":
                return fx("pool_taken")
            goalies = data.get("positionOrGroup") == "POS_201"
            if tf == PROJ:
                return fx("pool_goalies_projected" if goalies else "pool_skaters_projected")
            empty = fx("pool_goalies_projected")
            empty["statsTable"] = []
            empty["displayedSelections"]["displayedSeasonOrProjection"] = {"code": YTD,
                                                                          "timeframeTypeCode": "YEAR_TO_DATE"}
            return empty
        if method == "getLeagueRulesOld":
            return fx("league_rules")
        return super().data_for(method, data)


def test_provider_live_shape_end_to_end(tmp_path):
    sess = LiveSession()
    prov = provider(tmp_path, sess, fantrax_dynasty=False)
    ctx = prov.load()
    # rules page wins over FANTRAX_POINTS; goalie group kept separately
    assert prov.scoring_source == "Fantrax league rules"
    assert ctx.scoring.weights == RULE_WEIGHTS and ctx.scoring.goalie_weights == {"A": 3.0, "G": 20.0}
    assert prov.weights_mae == pytest.approx(0.0, abs=1e-9) and prov.weights_checked == 10
    assert prov.config_mae > 0.1   # the test POINTS are not this league's rules
    assert ctx.roster_shape == {"BN": 6, "F": 5, "D": 3, "G": 2}   # no IR / minors slots
    assert ctx.dynasty is True                                     # from "Keeper league Type: Dynasty"
    eichel = next(p for p in ctx.my_team.players if p.name == "Jack Eichel")
    assert set(eichel.lines) == {"projected"} and eichel.gp() == 0
    assert prov.fantasy_points[eichel.cid] == (440.4, 5.79) and prov.ages[eichel.cid] == 29
    # rostered rows get % rostered from the ALL_TAKEN player query (the roster view has no Ros column)
    owned = {p.name: p.pct_owned for p in ctx.my_team.players}
    assert owned == {"Jack Eichel": 99.0, "Dylan Holloway": 97.0, "William Nylander": 99.0,
                     "Mackenzie Blackwood": 91.0}
    change = {p.name: p.pct_owned_change for p in ctx.my_team.players}   # ALL_TAKEN "+/-" column
    assert change == {"Jack Eichel": 0.0, "Dylan Holloway": -1.0, "William Nylander": 0.0,
                      "Mackenzie Blackwood": -2.0}
    fas = ctx.free_agents
    assert all(p.pct_owned_change is not None for p in fas)                # pool "+/-" column
    goalies = [p for p in fas if p.is_goalie]
    assert len(fas) == 6 and [p.name for p in goalies] == ["Sergei Murashov", "Jake Allen", "Kevin Lankinen"]
    assert goalies[0].pct_owned == 64 and prov.ages[goalies[0].cid] == 22
    assert goalies[0].lines["projected"].stats["W"] == 25
    reqs = [c[0]["data"] for c in sess.calls if c[0]["method"] == "getPlayerStats"
            and c[0]["data"]["statusOrTeamFilter"] == "ALL_AVAILABLE"]
    assert {(r["positionOrGroup"], r["seasonOrProjection"]) for r in reqs} == \
        {("HOCKEY_SKATING", PROJ), ("POS_201", PROJ), ("HOCKEY_SKATING", YTD), ("POS_201", YTD)}
    rosters = [m["data"] for c in sess.calls for m in c if m["method"] == "getTeamRosterInfo"]
    assert sum(1 for r in rosters if r.get("seasonOrProjection") == PROJ) == 2
    assert prov.draft_picks[2027][0] == {"round": 1, "original_owner": "t1"}

    text = "\n".join(prov.describe_settings())
    for s in ("Scoring (Fantrax league rules): A=2", "goalies: A=3, G=20", "Scoring check: mean abs error 0.000",
              "Roster: max total 16, active 10, reserve 6, IR Not Used, minors Not Used",
              "Active by position (max): F 5, D 3, G 2", "Keeper league type: Dynasty",
              "Draft picks tradeable: Yes (3 future years, 10 rounds)",
              "Lineup changes: weekly (Rules page: Weekly every Monday)",
              "Playoffs: start scoring period 25", ", 6 teams", "My draft picks: 2027: 10 picks (rounds 1-10)",
              "Dynasty: yes (from Fantrax keeper league type", "FANTRAX_POINTS (not used: league rules win)",
              "To match the league rules set: FANTRAX_POINTS=A=2,", "Free agents loaded: 6 (3 goalies)"):
        assert s in text, s
    assert "Commish" not in text and "secret" not in text
    assert prov.lineup_lock == "weekly" and ctx.lineup_lock == "weekly"
    raw = prov.raw_settings()
    assert raw["league_rules"]["Keeper League"]["Keeper league Type"] == "Dynasty"
    assert raw["position_limits"]["G"]["max_active"] == 2 and raw["draftPickTradingAllowed"] is True

    fit = prov.fit_points()
    assert fit.n == 10 and fit.mae < 0.01


def test_fantrax_points_fallback_and_fit_hint(tmp_path):
    sess = LiveSession(overrides={"getLeagueRulesOld": {"content": ""}})
    prov = provider(tmp_path, sess)
    ctx = prov.load()
    assert prov.scoring_source == "FANTRAX_POINTS" and ctx.scoring.weights["G"] == 3.0
    assert prov.weights_mae > 0.1 and ctx.roster_shape == {"F": 3, "G": 1, "BN": 6}
    text = "\n".join(prov.describe_settings())
    assert "FANTRAX_POINTS check: mean abs error" in text and "--fit-points" in text


def test_cli_settings_fit_points(tmp_path, monkeypatch):
    from rich.console import Console
    from typer.testing import CliRunner

    from fantasy_manager import cli
    from fantasy_manager.providers import fantrax as fmod

    orig_init = fmod.FantraxProvider.__init__

    def init(self, settings, cache=None, session=None, **kw):
        orig_init(self, settings, cache, session=LiveSession(), today=TODAY)
    monkeypatch.setattr(fmod.FantraxProvider, "__init__", init)
    monkeypatch.chdir(tmp_path)
    for k, v in {"FM_DATA_DIR": str(tmp_path), "FANTRAX_LEAGUE_ID": "lg1", "FANTRAX_COOKIE": "FX_RM=secret",
                 "FANTRAX_POINTS": POINTS}.items():
        monkeypatch.setenv(k, v)
    cli.get_settings.cache_clear()
    monkeypatch.setattr(cli, "console", Console(width=250))
    runner = CliRunner()
    try:
        res = runner.invoke(cli.app, ["--league", "fantrax", "settings", "--fit-points"])
        assert res.exit_code == 0, res.output
        # 10 fixture lines cannot pin down 14 weights; the fit output format is what matters here
        assert "Paste into .env:" in res.output and "\nFANTRAX_POINTS=" in res.output
        assert "Fit error: mean abs" in res.output
        assert "not used: Fantrax league rules win" in res.output and "secret" not in res.output
        res = runner.invoke(cli.app, ["--league", "fantrax", "--json", "settings", "--fit-points"])
        assert res.exit_code == 0, res.output
        data = json.loads(res.output)
        assert data["fit"]["n"] == 10 and data["fit"]["points_line"].startswith("FANTRAX_POINTS=")
        assert data["in_use"]["source"] == "Fantrax league rules" and data["in_use"]["mae"] < 1e-9
        res = runner.invoke(cli.app, ["--league", "fantrax", "settings"])
        assert res.exit_code == 0, res.output
        assert "G (goalies)" in res.output and "Keeper league type: Dynasty" in res.output
    finally:
        cli.get_settings.cache_clear()


def test_parse_taken_ownership_reads_rostered_rows():
    from fantasy_manager.providers.fantrax import parse_taken_ownership

    got = parse_taken_ownership(fx("pool_taken"))
    assert got["fantrax:03924"] == (99.0, 29.0)                # Jack Eichel
    assert got["fantrax:03mbn"] == (91.0, 29.0)                # Mackenzie Blackwood
    # the first "Ros" header is roster status ("Act"); % rostered comes from OVERVIEW_PERCENT_OWNED_2
    pool = parse_taken_ownership(fx("player_pool"))
    assert set(pool) == {"fantrax:r1"}                        # free agents (no team) are skipped


# -- harness capture: activity feed and lineup snapshots ------------------------

def test_parse_fantrax_when_formats_and_season_inference():
    from datetime import datetime

    from fantasy_manager.providers.fantrax import parse_fantrax_when
    assert parse_fantrax_when("Mon Sep 21, 2026, 10:15AM") == datetime(2026, 9, 21, 10, 15)
    assert parse_fantrax_when("Sep 21, 10:15 AM EDT", 2026) == datetime(2026, 9, 21, 10, 15)
    assert parse_fantrax_when("Jan 3, 9:00 PM EST", 2026) == datetime(2027, 1, 3, 21, 0)   # second half of season
    assert parse_fantrax_when("<b>Tue Oct 6, 2026, 1:05PM</b>") == datetime(2026, 10, 6, 13, 5)
    assert parse_fantrax_when("yesterday") is None and parse_fantrax_when(None) is None


def test_fantrax_activity_maps_transactions_and_proposals(tmp_path):
    from fantasy_manager.providers.fantrax import PendingTradeInfo, activity_from_pending

    prov = provider(tmp_path)
    items = prov.activity()
    tx = [(i.action, i.cid, i.team_id, i.group_id) for i in items if i.action != "PROPOSED"]
    assert tx == [("ADD", "fantrax:p4", "t1", "a1"), ("DROP", "fantrax:x9", "t1", "a1")]
    assert items[0].ts.isoformat() == "2026-09-21T10:15:00" and items[0].team_name == "Caelan's Crushers"
    prop = [i for i in items if i.action == "PROPOSED"]
    assert [(i.cid, i.team_id, i.counterparty_id) for i in prop] == [("fantrax:r2", "t1", "t2")]   # picks skipped
    assert prov.activity(since=date(2026, 9, 22)) == [i for i in items if i.ts.date() >= date(2026, 9, 22)]
    dated = activity_from_pending([PendingTradeInfo(trade_id="x", proposed_by="t1", proposed_at="Oct 2, 9:00 PM EDT",
                                                    moves=[{"from": "t1", "to": "t2", "player_cid": "fantrax:a"}])],
                                  season_start_year=2026)
    assert dated[0].ts.isoformat() == "2026-10-02T21:00:00"


def test_fantrax_trade_rows_become_in_and_out():
    from fantasy_manager.providers.fantrax import TransactionInfo, activity_from_transactions
    rows = [TransactionInfo(tx_id="T", team_id="t1", when="Mon Oct 5, 2026, 9:00AM", kind="TRADE",
                            player_cid="fantrax:a", player_name="A"),
            TransactionInfo(tx_id="T", team_id="t2", when="Mon Oct 5, 2026, 9:00AM", kind="TRADE",
                            player_cid="fantrax:b", player_name="B")]
    got = {(i.action, i.cid, i.team_id, i.counterparty_id) for i in activity_from_transactions(rows)}
    assert got == {("TRADE_IN", "fantrax:a", "t1", "t2"), ("TRADE_OUT", "fantrax:a", "t2", "t1"),
                   ("TRADE_IN", "fantrax:b", "t2", "t1"), ("TRADE_OUT", "fantrax:b", "t1", "t2")}


def test_fantrax_lineup_snapshot(tmp_path):
    prov = provider(tmp_path)
    rows = prov.lineup_snapshot(date(2026, 10, 12))
    mine = [r for r in rows if r.team_id == "t1"]
    assert mine and all(r.date == date(2026, 10, 12) for r in rows)
    assert all(r.starting == (r.slot not in ("BN", "IR", "MIN", "TAXI")) for r in rows)
    assert {r.slot for r in mine} & {"BN", "IR", "MIN"}


def test_fantrax_activity_failure_is_a_warning(tmp_path):
    prov = provider(tmp_path)

    def boom():
        raise ProviderError("down")
    prov.load = boom
    assert prov.activity() == [] and prov.lineup_snapshot() == []
    assert any("activity unavailable" in w for w in prov.warnings)


def test_player_pool_parses_roster_percent_change():
    from fantasy_manager.providers.fantrax import parse_row_cells, stat_columns

    players, rows, _ = parse_player_pool(fx("player_pool"))
    by = {p.cid: p for p in players}
    assert by["fantrax:f1"].pct_owned_change == 2.0 and rows["fantrax:f1"].owned_change == 2.0   # "+2%"
    assert by["fantrax:f2"].pct_owned_change == 0.0
    proj, _, _ = parse_player_pool(fx("pool_skaters_projected"))
    assert proj[0].pct_owned == 66 and proj[0].pct_owned_change == -7.0                           # "-7%"
    header = fx("pool_skaters_projected")["tableHeader"]["cells"]
    plus_minus = next(i for i, c in enumerate(header) if c.get("shortName") == "+/-")
    assert plus_minus not in stat_columns(header)                     # still not a plus-minus stat
    assert "PM" not in proj[0].lines.get("projected", proj[0].lines.get("season")).stats
    # a Unicode minus sign is still negative
    rs = parse_row_cells(header, [{"content": "1"}, {"content": "FA"}, {"content": "24"}, {"content": "1"},
                                  {"content": "1"}, {"content": "5%"}, {"content": "−3%"}])
    assert rs.owned_change == -3.0


def test_taken_rows_carry_roster_percent_change():
    from fantasy_manager.providers.fantrax import parse_taken_rows

    rows = parse_taken_rows(fx("pool_taken"))
    assert rows["fantrax:03924"].owned_change == 0.0                  # Jack Eichel "0%"
    assert rows["fantrax:051yz"].owned_change == -1.0                 # Dylan Holloway "-1%"
    assert rows["fantrax:03mbn"].owned == 91.0 and rows["fantrax:03mbn"].owned_change == -2.0


def test_lineup_lock_is_parsed_once_and_shared():
    """One parsed value (FantraxProvider.lineup_lock / lineup_lock_for) drives describe_settings,
    the lineup wording and the matchup preview."""
    from types import SimpleNamespace

    from fantasy_manager.providers.fantrax import lineup_lock_for, parse_lineup_lock

    assert parse_lineup_lock("Daily") == "daily"
    assert parse_lineup_lock("Weekly                 every Monday") == "weekly"
    assert parse_lineup_lock(None) == parse_lineup_lock("") == "weekly"
    fx = SimpleNamespace(provider="fantrax", lineup_lock=None)
    assert lineup_lock_for(fx) == "weekly"
    assert lineup_lock_for(fx, SimpleNamespace(lineup_lock="daily")) == "daily"
    assert lineup_lock_for(fx, SimpleNamespace(rules=SimpleNamespace(get=lambda k: "Daily"))) == "daily"
    assert lineup_lock_for(SimpleNamespace(provider="fantrax", lineup_lock="daily")) == "daily"
    assert lineup_lock_for(SimpleNamespace(provider="espn", lineup_lock=None)) == "daily"
