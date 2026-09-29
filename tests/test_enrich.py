import json
from datetime import date
from pathlib import Path

import pytest

from fantasy_manager.matching.crosswalk import Crosswalk
from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine,
                                    normalize_name)
from fantasy_manager.providers import enrich
from fantasy_manager.providers.enrich import (SourceTracker, enrich_context, merge_status, recent_lines,
                                              statline_from_nhl, ttl_for)
from fantasy_manager.providers.nhl import NhlClient, NhlSkaterSeason

FIX = Path(__file__).parent / "fixtures"


def load(rel):
    return json.loads((FIX / rel).read_text(encoding="utf-8"))


EMPTY_REPORT = {"data": [], "total": 0}


class FixtureFetch:
    """NHL + injuries fixtures; current-season (20262027) stat reports are empty like pre-season."""

    def __init__(self):
        self.urls = []

    def __call__(self, url, params=None):
        self.urls.append(url)
        if "api.nhle.com/stats" in url:
            if "seasonId=20262027" in (params or {}).get("cayenneExp", ""):
                return EMPTY_REPORT
            kind, report = url.rsplit("/", 2)[-2:]
            name = {"summary": f"{kind}_summary.json", "realtime": "skater_realtime.json",
                    "faceoffwins": "skater_faceoffwins.json"}[report]
            return load(f"nhl/{name}")
        if "injuries" in url:
            return load("injuries/espn_injuries.json")
        for suffix, name in (("/club-schedule-season/EDM/20262027", "club_schedule_EDM.json"),
                             ("/club-schedule-season/CGY/20262027", "club_schedule_CGY.json"),
                             ("/roster/EDM/20262027", "roster_EDM.json")):
            if url.endswith(suffix):
                return load(f"nhl/{name}")
        if "/schedule/" in url:
            return load("nhl/schedule_2026-10-07.json")
        if "/game-log/" in url:
            return load("nhl/game_log_8478402.json")
        return {}


def pl(cid, name, team, pos, status="healthy", lines=None):
    return Player(cid=cid, name=name, name_norm=normalize_name(name), ids={"espn": cid.split(":")[1]},
                  team=team, positions=pos, status=status, lines=lines or {})


PROJ = {"projected": StatLine(split="projected", gp=80, stats={"G": 40.0, "GP": 80})}


def make_ctx(as_of=date(2026, 9, 28)):
    mine = [pl("espn:1", "Connor McDavid", "EDM", ["C", "F"], lines=dict(PROJ)),
            pl("espn:2", "Leon Draisaitl", "EDM", ["C", "F"]),
            pl("espn:3", "Troy Terry", "ANA", ["RW", "F"]),
            pl("espn:4", "Charlie McAvoy", "BOS", ["D"], status="dtd"),
            pl("espn:5", "Matthew Poitras", "BOS", ["C", "F"], status="out"),
            pl("espn:6", "A.J. Greer", "ANA", ["LW", "F"], status="ir"),
            pl("espn:7", "Andrei Vasilevskiy", "TBL", ["G"])]
    fas = [pl("espn:8", "Seamus Casey", "BOS", ["D"]),        # same name as the NJD report, wrong team
           pl("espn:9", "Nikita Kucherov", "TBL", ["RW", "F"])]
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in mine]
    return LeagueContext(provider="test", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}),
                         roster_shape={"C": 2, "BN": 5},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=fas, matchup_period=1, as_of=as_of)


class S:
    def __init__(self, d):
        self.fm_data_dir = d


@pytest.mark.parametrize("provider,feed,want", [
    ("healthy", "out", "out"), ("out", "dtd", "out"), ("out", "ir", "ir"), ("ir", "ltir", "ltir"),
    ("ltir", "dtd", "ltir"), ("unknown", "dtd", "dtd"), ("healthy", "unknown", "healthy"),
    ("healthy", "suspended", "suspended"), ("dtd", "suspended", "suspended"),
    ("suspended", "healthy", "suspended"), ("ir", "suspended", "ir"), ("suspended", "out", "out"),
])
def test_merge_status_severity(provider, feed, want):
    assert merge_status(provider, feed) == want


def run_enrich(tmp_path, ctx=None, deep=False):
    ctx = ctx or make_ctx()
    fetch = FixtureFetch()
    client = NhlClient(fetch_json=fetch, season=20262027)
    enrich_context(ctx, S(tmp_path), None, deep=deep, nhl=client, injuries_fetch=fetch,
                   crosswalk=Crosswalk(tmp_path), teams=["EDM", "CGY"])
    return ctx, fetch


def test_enrich_ids_birthdates_and_stats(tmp_path):
    ctx, _ = run_enrich(tmp_path)
    by = {p.name: p for p in ctx.all_players()}
    mcd = by["Connor McDavid"]
    assert mcd.cid == "espn:1" and mcd.nhl_id == 8478402
    row = load("nhl/skater_summary.json")["data"][0]
    prior = mcd.lines["prior"]
    assert prior.gp == row["gamesPlayed"] and prior.stats["PTS"] == row["points"]
    assert prior.stats["PPA"] == row["ppPoints"] - row["ppGoals"]
    assert "OTG" not in prior.stats                     # non-canonical keys dropped
    assert "season" not in mcd.lines                    # current season has no games yet
    assert "projected" in mcd.lines                     # provider lines untouched
    assert by["Leon Draisaitl"].birth_date == date(1995, 10, 27)   # from the NHL roster
    vas = by["Andrei Vasilevskiy"]
    assert vas.nhl_id == 8476883 and vas.lines["prior"].stats["GS"] > 0
    assert vas.lines["prior"].stats["SVPCT"] == pytest.approx(0.91233)
    assert "HIT" in by["Nikita Kucherov"].lines["prior"].stats
    assert any(n.startswith("NHL ids:") for n in ctx.source_notes)


def test_enrich_injury_merge(tmp_path):
    ctx, _ = run_enrich(tmp_path)
    by = {p.name: p for p in ctx.all_players()}
    assert by["Troy Terry"].status == "out" and "hip surgery" in by["Troy Terry"].status_note
    assert by["Charlie McAvoy"].status == "suspended"          # dtd provider + suspension feed
    assert by["Matthew Poitras"].status == "ir"                # feed more severe
    assert by["A.J. Greer"].status == "ir"                     # provider more severe than feed dtd
    assert "Upper Body" in by["A.J. Greer"].status_note
    assert by["Seamus Casey"].status == "healthy"              # NJD report, BOS player: not applied
    assert by["Connor McDavid"].status == "healthy"


def test_enrich_injury_matches_espn_id(tmp_path):
    ctx = make_ctx()
    p = ctx.free_agents[1]
    p.ids["espn"] = "3942905"          # Troy Terry's ESPN athlete id on a differently named player
    p.cid = "espn:3942905"
    run_enrich(tmp_path, ctx=ctx)
    assert p.status == "out"


def test_enrich_schedule(tmp_path):
    ctx, _ = run_enrich(tmp_path)
    assert ctx.season_start == date(2026, 9, 29)              # from the API, not hardcoded
    assert set(ctx.schedule) == {"EDM", "CGY"}
    assert ctx.schedule["EDM"][0] == date(2026, 9, 29)
    assert ctx.games_per_day and all(n >= 1 for n in ctx.games_per_day.values())
    # opponents from the same club schedules: home games show the visitor, away games "@home"
    assert ctx.opponents["EDM"][date(2026, 9, 29)] == "VAN"
    assert set(ctx.opponents["EDM"]) == set(ctx.schedule["EDM"])
    assert all(o.startswith("@") or len(o) == 3 for o in ctx.opponents["CGY"].values())


def test_enrich_degrades_gracefully(tmp_path):
    ctx = make_ctx()

    def broken(url, params=None):
        raise ConnectionError("no network")

    enrich_context(ctx, S(tmp_path), None, nhl=NhlClient(fetch_json=broken, season=20262027),
                   injuries_fetch=broken, crosswalk=Crosswalk(tmp_path), teams=["EDM"])
    assert any("Injury feed" in w for w in ctx.warnings)
    assert any("NHL rosters" in w for w in ctx.warnings)
    assert ctx.schedule == {} and ctx.season_start is None
    assert ctx.all_players()[0].nhl_id is None


def test_deep_skipped_before_season(tmp_path):
    ctx, fetch = run_enrich(tmp_path, deep=True)
    assert not any("/game-log/" in u for u in fetch.urls)
    assert any("Game logs skipped" in n for n in ctx.source_notes)


def test_deep_fetches_game_logs_in_season(tmp_path):
    ctx, fetch = run_enrich(tmp_path, ctx=make_ctx(as_of=date(2026, 10, 20)), deep=True)
    assert any("/game-log/" in u for u in fetch.urls)
    # the fixture log is from April, so nothing falls in the October windows
    assert all("last7" not in p.lines for p in ctx.all_players())
    assert any(n.startswith("Game logs:") for n in ctx.source_notes)


def test_recent_lines_windows():
    client = NhlClient(fetch_json=FixtureFetch(), season=20252026)
    log = client.game_log(8478402, 20252026)       # 04-16, 04-13, 04-11, 04-08
    lines = recent_lines(log, date(2026, 4, 16))
    assert lines["last7"].gp == 3 and lines["last15"].gp == 4 and lines["last30"].gp == 4
    assert lines["last15"].stats["A"] == sum(e.stats["A"] for e in log)
    assert lines["last7"].split == "last7"
    assert recent_lines(log, date(2026, 10, 1)) == {}


def test_recent_lines_goalie_rates():
    def fetch(url, params=None):
        return load("nhl/game_log_goalie_8476883.json")

    log = NhlClient(fetch_json=fetch, season=20252026).game_log(8476883)
    line = recent_lines(log, max(e.date for e in log))["last30"]
    sa, ga = line.stats["SA"], line.stats["GA"]
    assert line.stats["SVPCT"] == pytest.approx((sa - ga) / sa)
    assert line.per_game()["SVPCT"] == pytest.approx((sa - ga) / sa)   # rate passes through


def test_statline_from_nhl_requires_games():
    row = NhlSkaterSeason(player_id=1, name="x", season=20262027, stats={"GP": 0.0, "G": 0.0})
    assert statline_from_nhl(row, "season") is None


def test_ttl_routing_and_tracker():
    assert ttl_for("https://api.nhle.com/stats/rest/en/skater/summary")[1] == enrich.TTL_NHL_STATS
    assert ttl_for("https://api-web.nhle.com/v1/player/1/game-log/20262027/2")[1] == enrich.TTL_GAME_LOG
    assert ttl_for("https://api-web.nhle.com/v1/club-schedule-season/EDM/20262027")[1] == enrich.TTL_SCHEDULE
    assert ttl_for("https://api-web.nhle.com/v1/schedule/2026-09-28")[1] == enrich.TTL_SCHEDULE
    assert ttl_for("https://api-web.nhle.com/v1/roster/EDM/20262027")[1] == enrich.TTL_ROSTER
    assert ttl_for(enrich.INJURIES_URL)[1] == enrich.TTL_INJURIES
    assert ttl_for("https://www.rotowire.com/rss/news.php?sport=NHL")[1] == enrich.TTL_RSS

    class FakeCache:
        def __init__(self):
            self.calls = []

        def get_json(self, url, params=None, ttl=0):
            self.calls.append((url, ttl))
            return {"ok": True}

        def _lookup(self, key):
            return ("u", 200, "{}", 1000.0)

    c = FakeCache()
    t = SourceTracker(c)
    assert t.fetch_json("https://api-web.nhle.com/v1/roster/EDM/20262027") == {"ok": True}
    assert c.calls[0][1] == enrich.TTL_ROSTER
    assert t.notes(now=1000.0 + 7200) == ["NHL rosters: 1 requests, data 2h ago"]


class LandingFetch(FixtureFetch):
    def __call__(self, url, params=None):
        if url.endswith("/player/8476883/landing"):
            self.urls.append(url)
            return {"playerId": 8476883, "birthDate": "1994-07-28",
                    "draftDetails": {"year": 2012, "round": 1, "overallPick": 19},
                    "careerTotals": {"regularSeason": {"gamesPlayed": 600}}}
        return super().__call__(url, params)


def test_enrich_pedigree_from_player_pages(tmp_path):
    ctx = make_ctx()
    ctx.my_team.players[0].birth_date = date(1997, 1, 13)      # McDavid: 29, full prior season
    fetch = LandingFetch()
    enrich_context(ctx, S(tmp_path), None, nhl=NhlClient(fetch_json=fetch, season=20262027),
                   injuries_fetch=fetch, crosswalk=Crosswalk(tmp_path), teams=["EDM", "CGY"])
    by = {p.name: p for p in ctx.all_players()}
    vas = by["Andrei Vasilevskiy"]                   # no NHL roster birth date: looked up
    assert (vas.draft_overall, vas.draft_round, vas.draft_year, vas.career_gp) == (19, 1, 2012, 600)
    assert vas.birth_date == date(1994, 7, 28)
    mcd = by["Connor McDavid"]                       # 29 with a full prior season: not needed
    assert mcd.draft_overall is None and not any(u.endswith("/8478402/landing") for u in fetch.urls)
    assert any(n.startswith("Pedigree:") for n in ctx.source_notes)
    assert ttl_for("https://api-web.nhle.com/v1/player/1/landing") == ("NHL player pages", enrich.TTL_LANDING)


def test_pedigree_request_cap_and_targets(tmp_path):
    ctx = make_ctx()
    fetch = LandingFetch()
    enrich_context(ctx, S(tmp_path), None, nhl=NhlClient(fetch_json=fetch, season=20262027),
                   injuries_fetch=fetch, crosswalk=Crosswalk(tmp_path), teams=["EDM", "CGY"],
                   pedigree_limit=0)
    assert not any(u.endswith("/landing") for u in fetch.urls)
    assert any("deferred to the next run" in n for n in ctx.source_notes)
    young = Player(cid="x:1", name="Kid", name_norm="kid", ids={"nhl": "1"}, team="EDM", positions=["C"],
                   birth_date=date(2006, 1, 1))
    vet = Player(cid="x:2", name="Vet", name_norm="vet", ids={"nhl": "2"}, team="EDM", positions=["C"],
                 birth_date=date(1990, 1, 1),
                 lines={"prior": StatLine(split="prior", gp=70, stats={"GP": 70})})
    assert enrich.needs_pedigree(young, ctx.as_of) and not enrich.needs_pedigree(vet, ctx.as_of)
    young.draft_overall = 3
    assert not enrich.needs_pedigree(young, ctx.as_of)          # already loaded


def test_enrich_fills_prior2_prior3_with_one_pull_per_season(tmp_path):
    seasons = []

    class Counting(FixtureFetch):
        def __call__(self, url, params=None):
            if "api.nhle.com/stats" in url:
                seasons.append(((params or {}).get("cayenneExp", "").split("seasonId=")[1][:8], url.rsplit("/", 2)[-2]))
            return super().__call__(url, params)

    fetch = Counting()
    ctx = make_ctx()
    client = NhlClient(fetch_json=fetch, season=20262027)
    enrich_context(ctx, S(tmp_path), None, nhl=client, injuries_fetch=fetch, crosswalk=Crosswalk(tmp_path),
                   teams=["EDM", "CGY"])
    mcd = next(p for p in ctx.all_players() if p.name == "Connor McDavid")
    assert {"prior", "prior2", "prior3"} <= set(mcd.lines)
    assert mcd.lines["prior2"].split == "prior2" and mcd.lines["prior3"].gp == mcd.lines["prior"].gp  # same fixture
    # league-wide reports: 4 per season (skater summary / realtime / faceoffs, goalie summary), not per player
    from collections import Counter
    per_season = Counter(s for s, _ in seasons)
    assert set(per_season) == {"20262027", "20252026", "20242025", "20232024"}
    assert per_season["20242025"] == per_season["20232024"] == 4
    assert any("prior2" in n and "prior3" in n for n in ctx.source_notes)


def test_history_seasons_cached_for_30_days():
    url = "https://api.nhle.com/stats/rest/en/skater/summary"
    old = {"cayenneExp": "seasonId=20232024 and gameTypeId=2"}
    prior = {"cayenneExp": "seasonId=20252026 and gameTypeId=2"}
    today = date(2026, 9, 28)
    assert ttl_for(url, old, today) == ("NHL stats (history)", enrich.TTL_NHL_HISTORY)
    assert ttl_for(url, {"cayenneExp": "seasonId=20242025 and gameTypeId=2"}, today)[1] == 30 * 24 * 3600
    assert ttl_for(url, prior, today) == ("NHL stats", enrich.TTL_NHL_STATS)
    assert ttl_for(url, None, today)[1] == enrich.TTL_NHL_STATS


def test_fill_stat_lines_prior2_prior3_only_when_missing():
    from fantasy_manager.providers.enrich import fill_stat_lines
    row = NhlSkaterSeason(player_id=7, name="X", season=20242025, stats={"GP": 50, "G": 10, "PTS": 20})
    p = Player(cid="t:7", name="X", name_norm="x", ids={"nhl": "7"}, team=None, positions=["C"],
               lines={"prior2": StatLine(split="prior2", gp=3, stats={"GP": 3})})
    filled = fill_stat_lines([p], {}, {}, {7: row}, {7: row})
    assert filled == {"season": 0, "prior": 0, "prior2": 0, "prior3": 1}
    assert p.lines["prior2"].gp == 3 and p.lines["prior3"].gp == 50 and p.lines["prior3"].split == "prior3"
