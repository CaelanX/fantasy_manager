"""Daily Faceoff lines / goalie starts: parsers, crosswalk (source "dfo"), snapshots + diffs,
enrichment, alerts, ESPN confirmed-start lineups and advise integration."""
import copy
import json
from datetime import date, datetime, timezone
from pathlib import Path

import pytest

from fantasy_manager.matching.crosswalk import Crosswalk, SourceItem, parse_confirm
from fantasy_manager.matching.matcher import Candidate
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers import dailyfaceoff as dfo
from fantasy_manager.providers.dailyfaceoff import (TEAM_SLUGS, DailyFaceoffClient, DfoError, extract_next_data,
                                                    goalies_url, make_cached_fetch_text, parse_starting_goalies,
                                                    parse_team_lines, parse_team_slugs, team_url)
from fantasy_manager.providers.lines_enrich import (LineSnapshotStore, build_snapshot, diff_snapshots, enrich_lines,
                                                    team_meta_for)
from fantasy_manager.providers.nhl import NHL_TEAMS
from fantasy_manager.recommend import alerts as alerts_mod
from fantasy_manager.recommend.advise import PRIORITY, advise, normalize_scores
from fantasy_manager.recommend.alerts import recommend_line_alerts
from fantasy_manager.recommend.lineup import apply_confirmed_starts, confirmed_start_probability, recommend_lineup
from fantasy_manager.valuation.valuate import PlayerValue

FIX = Path(__file__).parent / "fixtures" / "dailyfaceoff"
DAL_HTML = (FIX / "dallas-stars.html").read_text(encoding="utf-8")
EDM_HTML = (FIX / "edmonton-oilers.html").read_text(encoding="utf-8")
GOALIES_HTML = (FIX / "starting-goalies-2026-09-29.html").read_text(encoding="utf-8")
DAY1, DAY2 = date(2026, 9, 28), date(2026, 9, 29)


def wrap(data) -> str:
    return ('<html><body><script id="__NEXT_DATA__" type="application/json">'
            + json.dumps(data) + "</script></body></html>")


def mutate(html, fn) -> str:
    data = copy.deepcopy(extract_next_data(html))
    fn(data)
    return wrap(data)


def entries(data):
    return data["props"]["pageProps"]["combinations"]["players"]


# ------------------------------------------------------------------------------ parsers

def test_parse_team_lines_dallas():
    tl = parse_team_lines(DAL_HTML)
    assert tl.team == "DAL" and tl.slug == "dallas-stars"
    assert tl.updated_at == datetime(2026, 9, 28, 18, 13, 31, 333000, tzinfo=timezone.utc)
    assert tl.source == "Sam Nestler" and tl.source_url.startswith("https://x.com/")
    assert len(tl.players) == 20                     # 38 rows merged into one entry per player
    by = {p.name: p for p in tl.players}
    rantanen = by["Mikko Rantanen"]
    assert (rantanen.line, rantanen.pp_unit, rantanen.pk_unit, rantanen.position) == ("f1", "pp1", None, "RW")
    assert rantanen.groups == ["f1", "pp1"] and rantanen.dfo_id == 2477
    assert rantanen.toi_last5 == pytest.approx((108 + 25 / 60) / 5, abs=0.01)
    assert rantanen.ppp_last5 == 3 and rantanen.gp_last10 == 10
    heiskanen = by["Miro Heiskanen"]
    assert (heiskanen.line, heiskanen.pp_unit, heiskanen.pk_unit, heiskanen.position) == ("d1", "pp1", "pk1", "D")
    assert by["Jake Oettinger"].line == "g" and by["Jake Oettinger"].goalie_depth == 1
    assert by["Casey DeSmith"].goalie_depth == 2 and by["Casey DeSmith"].position == "G"
    assert [p.name for p in tl.by_group("pp2")][:2] == ["Justin Hryckowian", "Roope Hintz"]
    assert all(p.injury_status is None and not p.gtd for p in tl.players)


def test_parse_team_lines_injured_group():
    tl = parse_team_lines(EDM_HTML)
    assert tl.team == "EDM" and tl.source == "Projected"
    by = {p.name: p for p in tl.players}
    rnh = by["Ryan Nugent-Hopkins"]
    assert rnh.group == "ir" and rnh.line is None and not rnh.in_lineup and rnh.injury_status == "dtd"
    assert by["Frederik Andersen"].injury_status == "out"
    assert by["Tristan Jarry"].dfo_id == 2626 and by["Tristan Jarry"].goalie_depth == 1


def test_parser_walks_json_instead_of_fixed_path():
    data = extract_next_data(DAL_HTML)
    moved = {"props": {"pageProps": {"page": {"sections": [{"x": 1}, {"lineup": data["props"]["pageProps"]["combinations"]}]}}}}
    tl = parse_team_lines(wrap(moved))
    assert tl.team == "DAL" and len(tl.players) == 20
    with pytest.raises(DfoError):
        parse_team_lines("<html>no data</html>")
    with pytest.raises(DfoError):
        parse_team_lines(wrap({"props": {"pageProps": {"combinations": {"players": []}}}}))


def test_parse_starting_goalies():
    starts = parse_starting_goalies(GOALIES_HTML)
    assert len(starts) == 10 and {s.game for s in starts} == {"FLA@CAR", "MTL@TOR", "NYR@BOS", "VAN@EDM", "CHI@VGK"}
    sway = next(s for s in starts if s.goalie_name == "Jeremy Swayman")
    assert (sway.team, sway.opponent, sway.home, sway.dfo_id) == ("BOS", "NYR", True, 28098)
    assert sway.strength == "Confirmed" and sway.is_start and sway.is_confirmed and sway.source == "Jack Studley"
    assert sway.created_at == datetime(2026, 9, 29, 15, 2, 24, 985000, tzinfo=timezone.utc)
    assert sway.game_time == datetime(2026, 9, 30, 0, 0, tzinfo=timezone.utc)
    shesterkin = next(s for s in starts if s.goalie_name == "Igor Shesterkin")
    assert shesterkin.strength is None and not shesterkin.is_start
    assert sum(s.is_start for s in starts) == 2
    assert parse_starting_goalies(wrap({"props": {"pageProps": {"data": []}}})) == []


def test_team_slug_map_complete_and_matches_site():
    assert len(TEAM_SLUGS) == 32 and set(TEAM_SLUGS) == set(NHL_TEAMS)
    assert len(set(TEAM_SLUGS.values())) == 32
    assert TEAM_SLUGS["UTA"] == "utah-mammoth"
    assert parse_team_slugs(DAL_HTML) == TEAM_SLUGS          # sortedTeams on the real page
    assert team_url("VEG") == "https://www.dailyfaceoff.com/teams/vegas-golden-knights/line-combinations"
    assert goalies_url(DAY2) == "https://www.dailyfaceoff.com/starting-goalies/2026-09-29"
    with pytest.raises(DfoError):
        team_url("XXX")


def test_client_degrades_per_team_and_uses_cache_ttls():
    calls = []

    def fetch(url, params=None):
        calls.append(url)
        if "dallas-stars" in url:
            return DAL_HTML
        if "edmonton-oilers" in url:
            return "<html>maintenance</html>"
        raise RuntimeError("HTTP 503")

    c = DailyFaceoffClient(fetch_text=fetch)
    out = c.all_lines(["DAL", "EDM", "TB"])
    assert list(out) == ["DAL"] and len(calls) == 3
    assert len(c.warnings) == 1 and "2 team(s)" in c.warnings[0] and "EDM" in c.warnings[0]

    class FakeCache:
        def __init__(self):
            self.seen = []

        def get_text(self, url, params=None, headers=None, ttl=0):
            self.seen.append((url, headers, ttl))
            return GOALIES_HTML

    cache = FakeCache()
    DailyFaceoffClient(fetch_text=make_cached_fetch_text(cache)).starting_goalies(DAY2)
    make_cached_fetch_text(cache)(team_url("DAL"))
    (u1, h1, t1), (u2, _, t2) = cache.seen
    assert h1 == {"User-Agent": dfo.USER_AGENT} and "personal use" in dfo.USER_AGENT
    assert t1 == 3 * 3600 and t2 == 12 * 3600


# ------------------------------------------------------------------------------ crosswalk

def test_crosswalk_dfo_source(tmp_path):
    cands = [Candidate(key=8478420, name="Mikko Rantanen", team="DAL", position="RW"),
             Candidate(key=8479999, name="Jake Oettinger", team="DAL", position="G"),
             Candidate(key=1, name="Elias Pettersson", team="VAN", position="C"),
             Candidate(key=2, name="Elias Pettersson", team="VAN", position="C")]
    xw = Crosswalk(tmp_path)
    items = [SourceItem(source_id="2477", name="Mikko Rantanen", team="DAL", position="RW"),
             SourceItem(source_id="2813", name="Jake Oetinger", team="DAL", position="G"),   # typo -> fuzzy high
             SourceItem(source_id="555", name="Elias Pettersson", team="VAN", position="C"),  # duplicate -> pending
             SourceItem(source_id="777", name="Nobody Atall", team="DAL", position="D")]
    ids, stats = xw.resolve_source("dfo", items, cands)
    assert ids == {"2477": 8478420, "2813": 8479999}
    assert (stats.resolved, stats.new, stats.pending, stats.unmatched) == (2, 2, 1, 1)
    assert xw.get("dfo", "2477").confidence == "exact"
    assert xw.get("dfo", "777") is None                     # unmatched feed rows are not stored
    assert [(r.source, r.source_id) for r in xw.pending(include_unmatched=True)] == [("dfo", "555")]
    src, sid, nid = parse_confirm("dfo:555=2")
    xw.confirm(src, sid, nid)
    ids2, stats2 = xw.resolve_source("dfo", items[:3], [])   # stored / confirmed rows need no candidates
    assert ids2 == {"2477": 8478420, "2813": 8479999, "555": 2} and stats2.new == 0
    assert xw.pending() == []
    xw.close()


# ------------------------------------------------------------------------------ snapshots / diffs

def _snap(teams, players):
    return {"teams": {t: {} for t in teams}, "players": players}


def _pl(team, line, pp=None, group=None, depth=None):
    return {"team": team, "line": line, "pp_unit": pp, "group": group or line or "ir", "goalie_depth": depth}


def test_diff_snapshots_change_strings():
    prev = _snap(["DAL", "EDM"], {
        "1": _pl("DAL", "f3", "pp2"), "2": _pl("DAL", "f1", "pp1"), "3": _pl("DAL", None, None, "ir"),
        "4": _pl("DAL", "f2"), "5": _pl("DAL", "d2"), "6": _pl("DAL", "g", depth=2), "7": _pl("EDM", "f1", "pp1"),
        "8": _pl("TOR", "f1", "pp1"), "9": _pl("DAL", "f4")})
    cur = _snap(["DAL", "EDM", "TOR"], {
        "1": _pl("DAL", "f1", "pp1"), "2": _pl("DAL", "f2", "pp2"), "3": _pl("DAL", "f3"),
        "4": _pl("DAL", None, None, "ir"), "5": _pl("DAL", "d2"), "6": _pl("DAL", "g", depth=1),
        "8": _pl("TOR", "f4"), "9": _pl("DAL", "f4", "pp2"), "10": _pl("EDM", "f2", "pp1")})
    d = diff_snapshots(prev, cur)
    assert d["1"] == "F3 -> F1; PP2 -> PP1"
    assert d["2"] == "F1 -> F2; PP1 -> PP2"
    assert d["3"] == "new to lineup"
    assert d["4"] == "F2 -> IR"
    assert "5" not in d
    assert d["6"] == "G2 -> G1"
    assert d["7"] == "scratched"                  # gone from EDM's page
    assert "8" not in d                           # TOR not in the previous snapshot: no comparison
    assert d["9"] == "no PP -> PP2"
    assert d["10"] == "new to lineup; no PP -> PP1"
    assert diff_snapshots(None, cur) == {}


def test_snapshot_store_previous(tmp_path):
    store = LineSnapshotStore(tmp_path)
    assert store.previous(DAY2) is None
    store.save(date(2026, 9, 26), {"a": 1})
    store.save(DAY1, {"a": 2})
    store.save(DAY2, {"a": 3})
    assert store.path(DAY2) == tmp_path / "lines" / "lines-2026-09-29.json"
    assert store.previous(DAY2) == (DAY1, {"a": 2})
    assert store.dates() == [date(2026, 9, 26), DAY1, DAY2]
    snap = build_snapshot(DAY2, {"DAL": parse_team_lines(DAL_HTML)}, [], {"2477": 8478420})
    assert snap["players"]["2477"]["nhl_id"] == 8478420 and snap["players"]["2477"]["pp_unit"] == "pp1"
    json.dumps(snap)


# ------------------------------------------------------------------------------ enrichment

NHL = {"Mikko Rantanen": 8478420, "Jamie Benn": 8473994, "Wyatt Johnston": 8482740, "Matt Duchene": 8475168,
       "Jake Oettinger": 8479979, "Casey DeSmith": 8479193, "Tristan Jarry": 8477465, "Devon Levi": 8482221,
       "Ryan Nugent-Hopkins": 8476454, "Justin Hryckowian": 8484000, "Sam Steel": 8479351}
TEAM_OF = {"Tristan Jarry": "EDM", "Devon Levi": "EDM", "Ryan Nugent-Hopkins": "EDM"}


def player(name, pos, nhl=True, cid=None, team=None):
    ids = {"espn": cid or name}
    if nhl:
        ids["nhl"] = str(NHL[name])
    return Player(cid=cid or f"espn:{name}", name=name, name_norm=name.lower(), ids=ids,
                  team=team or TEAM_OF.get(name, "DAL"), positions=pos, status="healthy")


def league(mine, fas, provider="espn", as_of=DAY2, schedule=None):
    me = FantasyTeam(team_id="1", name="me", owner_is_me=True,
                     slots=[RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR")) for s, p in mine])
    return LeagueContext(provider=provider, league_id="1", season=2027, name="L",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 1, "G": 1, "BN": 3},
                         teams=[me], free_agents=fas, matchup_period=1, as_of=as_of, schedule=schedule or {})


def candidates():
    return [Candidate(key=v, name=k, team=TEAM_OF.get(k, "DAL"), position=None) for k, v in NHL.items()]


def day1_dal(data):
    """Yesterday: Benn on PP1 and Johnston on F3 (swap with today's page)."""
    for e in entries(data):
        if e["name"] == "Jamie Benn" and e["groupIdentifier"] == "pp2":
            e["groupIdentifier"] = "pp1"
        elif e["name"] == "Matt Duchene" and e["groupIdentifier"] == "pp1":
            e["groupIdentifier"] = "pp2"
        elif e["name"] == "Wyatt Johnston" and e["groupIdentifier"] == "f1":
            e["groupIdentifier"] = "f3"
        elif e["name"] == "Sam Steel" and e["groupIdentifier"] == "f3":
            e["groupIdentifier"] = "f1"


def fake_fetch(dal_html, edm_html=EDM_HTML, goalies_html=GOALIES_HTML):
    def fetch(url, params=None):
        if "dallas-stars" in url:
            return dal_html
        if "edmonton-oilers" in url:
            return edm_html
        if "/starting-goalies/" in url:
            return goalies_html
        raise RuntimeError(f"unexpected {url}")
    return fetch


def test_enrich_lines_end_to_end(tmp_path):
    store = LineSnapshotStore(tmp_path)
    xw = Crosswalk(tmp_path)
    # day 1 snapshot (no previous one -> no changes)
    mk = lambda: [player(n, pos) for n, pos in (("Mikko Rantanen", ["RW"]), ("Jamie Benn", ["LW"]),  # noqa: E731
                                                ("Wyatt Johnston", ["C"]), ("Tristan Jarry", ["G"]),
                                                ("Devon Levi", ["G"]), ("Ryan Nugent-Hopkins", ["C"]))]
    ps = mk()
    ctx1 = league([("C", ps[0]), ("BN", ps[1]), ("BN", ps[2]), ("G", ps[3]), ("BN", ps[4]), ("BN", ps[5])],
                  [player("Matt Duchene", ["C"], cid="espn:fa1")], as_of=DAY1)
    goalies_day1 = wrap({"props": {"pageProps": {"data": []}}})
    r1 = enrich_lines(ctx1, None, xw, DAY1, store, teams=["DAL", "EDM"], candidates=candidates(),
                      client=DailyFaceoffClient(fake_fetch(mutate(DAL_HTML, day1_dal), goalies_html=goalies_day1)))
    assert r1.teams == 2 and r1.previous_date is None and r1.changes == {}
    assert (tmp_path / "lines" / "lines-2026-09-28.json").exists()

    # day 2: real pages
    ps = mk()
    rant, benn, john, jarry, levi, rnh = ps
    rnh.status_note = None
    fa = player("Matt Duchene", ["C"], cid="espn:fa1")
    unk = player("Sam Steel", ["C"], nhl=False, cid="espn:nonhl")      # no NHL id -> untouched
    ctx = league([("C", rant), ("BN", benn), ("BN", john), ("G", jarry), ("BN", levi), ("BN", rnh)], [fa, unk])
    res = enrich_lines(ctx, None, xw, DAY2, store, teams=["DAL", "EDM"], candidates=candidates(),
                       client=DailyFaceoffClient(fake_fetch(DAL_HTML)))
    assert res.previous_date == DAY1 and res.snapshot_path.endswith("lines-2026-09-29.json")
    assert (rant.line, rant.pp_unit, rant.pk_unit, rant.line_change) == ("f1", "pp1", None, None)
    assert benn.line_change == "PP1 -> PP2" and benn.pp_unit == "pp2"
    assert john.line_change == "F3 -> F1" and john.line == "f1"
    assert fa.line_change == "PP2 -> PP1"
    assert unk.line is None and unk.line_change is None
    assert rnh.line is None and rnh.status_note == "DFO: dtd"
    assert jarry.confirmed_start is True and levi.confirmed_start is False
    assert jarry.start_source == "DFO Confirmed: Tristan Jarry starts VAN@EDM (Bob Stauffer, Sep 28 17:42 UTC)"
    assert levi.start_source == jarry.start_source
    assert rant.confirmed_start is None                   # skaters never get a start flag
    assert res.matched == len(NHL) and res.listed == 20 + len(parse_team_lines(EDM_HTML).players)
    assert res.changes["Jamie Benn"] == "PP1 -> PP2"
    assert any("Daily Faceoff: lines for 2 teams" in n for n in ctx.source_notes)
    saved = json.loads(Path(res.snapshot_path).read_text(encoding="utf-8"))
    assert saved["changes"]["425"] == "PP1 -> PP2" and saved["players"]["2477"]["nhl_id"] == 8478420
    assert xw.get("dfo", "2626").nhl_id == NHL["Tristan Jarry"]
    assert team_meta_for(DAY2)["DAL"]["source"] == "Sam Nestler"
    xw.close()


def test_enrich_lines_degrades_to_warnings(tmp_path):
    def broken(url, params=None):
        raise RuntimeError("HTTP 500")
    p = player("Mikko Rantanen", ["RW"])
    ctx = league([("C", p)], [])
    res = enrich_lines(ctx, None, None, DAY2, tmp_path, teams=["DAL"], candidates=candidates(),
                       client=DailyFaceoffClient(broken))
    assert res.teams == 0 and p.line is None
    assert any("lines unavailable" in w for w in ctx.warnings)
    assert any("starting goalies unavailable" in w for w in ctx.warnings)
    assert not (tmp_path / "lines").exists()


# ------------------------------------------------------------------------------ alerts

def with_change(name, pos, change, line=None, pp=None, cid=None):
    p = player(name, pos, cid=cid)
    p.line_change, p.line, p.pp_unit = change, line, pp
    return p


META = {"DAL": {"source": "Sam Nestler", "updated_at": "2026-09-28T18:13:31.333Z"}}


def test_line_alert_thresholds():
    mine = [with_change("Wyatt Johnston", ["C"], "F3 -> F1; PP2 -> PP1", "f1", "pp1"),
            with_change("Jamie Benn", ["LW"], "PP1 -> PP2", "f3", "pp2"),
            with_change("Mikko Rantanen", ["RW"], "F2 -> F1", "f1", "pp1"),
            with_change("Sam Steel", ["C"], "scratched")]
    fas = [with_change("Matt Duchene", ["C"], "PP2 -> PP1", "f3", "pp1", cid="espn:fa1"),
           with_change("Justin Hryckowian", ["LW"], "PP1 -> PP2", "f1", "pp2", cid="espn:fa2")]  # FA demotion: ignored
    ctx = league([("C", p) for p in mine], fas)
    recs = recommend_line_alerts(ctx, team_meta=META)
    by = {r.subjects[0].name: r for r in recs}
    assert set(by) == {"Wyatt Johnston", "Jamie Benn", "Mikko Rantanen", "Sam Steel", "Matt Duchene"}
    assert all(r.kind == "alert" for r in recs)
    assert by["Wyatt Johnston"].strength == 7 and "promoted to PP1" in by["Wyatt Johnston"].title
    assert by["Mikko Rantanen"].strength == 5 and "top unit" in by["Mikko Rantanen"].title
    assert by["Jamie Benn"].strength == 4 and "sell / bench caution" in by["Jamie Benn"].title
    assert by["Sam Steel"].strength == 4
    assert by["Matt Duchene"].strength == 7 and "free agent" in by["Matt Duchene"].title
    codes = {r.code for r in by["Wyatt Johnston"].reasons}
    assert {"LINE_CHANGE", "PP_UNIT"} <= codes
    lc = next(r for r in by["Wyatt Johnston"].reasons if r.code == "LINE_CHANGE")
    assert "Sam Nestler" in lc.text and "Sep 28 18:13 UTC" in lc.text
    assert [r.strength for r in recs] == sorted([r.strength for r in recs], reverse=True)
    assert recs[0].subjects[0].name == "Wyatt Johnston"      # mine before the FA on a tie


def test_goalie_start_alerts_and_weak_opponent():
    src = "DFO Confirmed: {} starts VAN@EDM (Bob Stauffer, Sep 28 17:42 UTC)"
    jarry, levi = player("Tristan Jarry", ["G"]), player("Devon Levi", ["G"])
    jarry.confirmed_start, jarry.start_source = True, src.format("Tristan Jarry")
    levi.confirmed_start, levi.start_source = False, src.format("Tristan Jarry")
    fa_g = player("Jake Oettinger", ["G"], cid="espn:fag")
    fa_g.team = "VAN"
    fa_g.confirmed_start, fa_g.start_source = True, "DFO Likely: Jake Oettinger starts VAN@EDM (X, Sep 29 10:00 UTC)"
    ctx = league([("G", jarry), ("BN", levi)], [fa_g])
    recs = recommend_line_alerts(ctx, weak_teams={"EDM": 2.1})
    by = {r.subjects[0].name: r for r in recs}
    assert by["Tristan Jarry"].strength == 6 and "confirmed to start tonight vs VAN" in by["Tristan Jarry"].title
    assert by["Tristan Jarry"].reasons[0].code == "CONFIRMED_START" and "Bob Stauffer" in by["Tristan Jarry"].reasons[0].text
    assert by["Devon Levi"].strength == 4 and "not starting" in by["Devon Levi"].title
    assert by["Jake Oettinger"].strength == 6 and "likely to start tonight vs EDM" in by["Jake Oettinger"].title
    assert {r.code for r in by["Jake Oettinger"].reasons} == {"CONFIRMED_START", "OPPONENT"}
    # FA goalie against a team that is not a weak offense: no alert
    assert "Jake Oettinger" not in {r.subjects[0].name for r in recommend_line_alerts(ctx, weak_teams={"BOS": 2.0})}
    # the schedule says EDM does not play today -> no start alerts
    ctx.schedule = {"EDM": [date(2026, 10, 8)], "VAN": [date(2026, 10, 8)]}
    assert recommend_line_alerts(ctx, weak_teams={"EDM": 2.1}) == []


def test_weak_offenses_ranks_bottom_teams():
    fas = []
    for i, t in enumerate(sorted(NHL_TEAMS)):
        p = Player(cid=f"x{i}", name=f"p{i}", name_norm=f"p{i}", ids={}, team=t, positions=["C"])
        p.lines["prior"] = StatLine(split="prior", gp=82, stats={"G": 100 + i})
        fas.append(p)
    ctx = league([], fas)
    weak = alerts_mod.weak_offenses(ctx, n=3)
    assert list(weak) == sorted(NHL_TEAMS)[:3]
    assert alerts_mod.weak_offenses(league([], fas[:5])) == {}


# ------------------------------------------------------------------------------ lineup (ESPN)

def V(p, proj, games=3, share=0.6):
    return PlayerValue(player=p, fpg=proj / games, fpg_season=proj / games, fpg_week=proj / games, vorp=0.0,
                       proj_week=proj, games_next7=games, start_share=share)


def test_confirmed_start_probability():
    p = player("Tristan Jarry", ["G"])
    assert confirmed_start_probability(p) is None
    p.confirmed_start, p.start_source = True, "DFO Confirmed: x"
    assert confirmed_start_probability(p) == 1.0
    p.start_source = "DFO Likely: x"
    assert confirmed_start_probability(p) == 0.8
    p.confirmed_start = False
    assert confirmed_start_probability(p) == pytest.approx(0.2)
    p.start_source = "DFO Confirmed: x"
    assert confirmed_start_probability(p) == 0.0


def test_espn_lineup_uses_confirmed_start():
    a = player("Tristan Jarry", ["G"])        # starting, higher week projection, but sits tonight
    b = player("Devon Levi", ["G"])           # bench, confirmed tonight
    src = "DFO Confirmed: Devon Levi starts VAN@EDM (Bob Stauffer, Sep 29 17:42 UTC)"
    a.confirmed_start, a.start_source = False, src
    b.confirmed_start, b.start_source = True, src
    sched = {"EDM": [DAY2, date(2026, 10, 1), date(2026, 10, 3)]}
    values = {a.cid: V(a, 6.0, share=0.6), b.cid: V(b, 4.0, share=0.4)}
    ctx = league([("G", a), ("BN", b)], [], provider="espn", schedule=sched)
    new, used = apply_confirmed_starts(ctx, values)
    # a: 6 + 6/(3*0.6) * (0 - 0.6) = 4.0 ; b: 4 + 4/(3*0.4) * (1 - 0.4) = 6.0
    assert new[a.cid].proj_week == pytest.approx(4.0) and new[b.cid].proj_week == pytest.approx(6.0)
    assert used == {a.cid: (0.0, 0.6), b.cid: (1.0, 0.4)}
    assert values[a.cid].proj_week == 6.0                  # caller's values untouched
    recs = recommend_lineup(ctx, values)
    swap = next(r for r in recs if r.add and r.add[0].cid == b.cid)
    assert swap.drop[0].cid == a.cid
    assert [r.code for r in swap.reasons].count("CONFIRMED_START") == 2
    # unknown start -> season share; other providers (weekly lineups) ignore confirmed starts
    assert not any(r.add and r.add[0].cid == b.cid for r in recommend_lineup(ctx.model_copy(update={"provider": "fantrax"}), values))
    b.confirmed_start = a.confirmed_start = None
    assert apply_confirmed_starts(ctx, values) == (values, {})
    # no game today -> unchanged
    b.confirmed_start = True
    ctx.schedule = {"EDM": [date(2026, 10, 1)]}
    assert apply_confirmed_starts(ctx, values)[1] == {}


# ------------------------------------------------------------------------------ advise

def test_advise_includes_alerts_after_injury():
    assert PRIORITY.index("alert") == PRIORITY.index("injury") + 1
    mine = [with_change("Wyatt Johnston", ["C"], "PP2 -> PP1", "f1", "pp1"),
            with_change("Jamie Benn", ["LW"], "PP1 -> PP2", "f3", "pp2")]
    ctx = league([("C", mine[0]), ("BN", mine[1])], [])
    recs = advise(ctx, {}, include=("alerts",))
    alerts = [r for r in recs if r.kind == "alert"]
    assert [r.subjects[0].name for r in alerts] == ["Wyatt Johnston", "Jamie Benn"]
    assert alerts[0].score == 10 and alerts[0].strength == 7 and alerts[1].strength == 4
    assert (alerts[0].rank_in_kind, alerts[0].kind_total) == (1, 2)
    # default include runs the alert engines too
    assert any(r.kind == "alert" for r in advise(ctx, {}))
    out = normalize_scores(alerts)
    assert {r.kind for r in out} == {"alert"} and out[0].reasons[-1].code == "RAW_SCORE"


def test_advise_alert_engine_taking_only_ctx(monkeypatch):
    from fantasy_manager.recommend import advise as adv
    from fantasy_manager.models import Recommendation

    def only_ctx(ctx):
        return [Recommendation(kind="alert", score=1.0, title="role", strength=3.0)]

    monkeypatch.setattr(alerts_mod, "recommend_line_alerts", only_ctx)
    recs = adv.advise(league([], []), {}, include=("alerts",))
    assert [r.title for r in recs] == ["role"] and recs[0].strength == 3.0
