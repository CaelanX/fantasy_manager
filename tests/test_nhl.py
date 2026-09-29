import json
from datetime import date
from pathlib import Path

import pytest

from fantasy_manager.providers import nhl
from fantasy_manager.providers.nhl import (
    NhlClient, back_to_backs, current_season, games_in_window, games_per_day, off_nights,
    parse_toi, prior_season, split_teams,
)

FIX = Path(__file__).parent / "fixtures" / "nhl"


def load(name):
    return json.loads((FIX / name).read_text(encoding="utf-8"))


ROUTES = {
    "/skater/summary": "skater_summary.json",
    "/skater/realtime": "skater_realtime.json",
    "/skater/faceoffwins": "skater_faceoffwins.json",
    "/goalie/summary": "goalie_summary.json",
    "/player/8478402/landing": "player_landing_8478402.json",
    "/player/8478402/game-log/20252026/2": "game_log_8478402.json",
    "/player/8476883/game-log/20252026/2": "game_log_goalie_8476883.json",
    "/club-schedule-season/EDM/20262027": "club_schedule_EDM.json",
    "/club-schedule-season/CGY/20262027": "club_schedule_CGY.json",
    "/roster/EDM/20262027": "roster_EDM.json",
    "/schedule/2026-10-07": "schedule_2026-10-07.json",
    "/search/player": "search_sebastian_aho.json",
}


class FakeFetch:
    def __init__(self):
        self.calls = []

    def __call__(self, url, params=None):
        self.calls.append((url, params))
        for suffix, fname in ROUTES.items():
            if url.endswith(suffix):
                return load(fname)
        raise AssertionError(f"unexpected url {url}")


@pytest.fixture
def client():
    return NhlClient(fetch_json=FakeFetch(), season=20262027)


def test_current_season_rollover():
    assert current_season(date(2026, 9, 28)) == 20262027
    assert current_season(date(2026, 9, 1)) == 20262027
    assert current_season(date(2026, 8, 31)) == 20252026
    assert current_season(date(2027, 3, 1)) == 20262027
    assert prior_season(20262027) == 20252026
    assert NhlClient(fetch_json=lambda u, p=None: None).season == current_season()


def test_helpers():
    assert parse_toi("24:49") == 1489
    assert parse_toi(1234.5) == 1234.5
    assert parse_toi(None) is None
    assert split_teams("SJS, TOR") == ["SJS", "TOR"]
    assert split_teams(None) == []


def test_report_params(client):
    client.skater_summary(20252026)
    url, params = client.fetch_json.calls[-1]
    assert url == "https://api.nhle.com/stats/rest/en/skater/summary"
    assert params["cayenneExp"] == "seasonId=20252026 and gameTypeId=2"
    assert params["limit"] == -1
    assert "playerId" in params["sort"]


def test_all_skaters_merge(client):
    skaters = client.all_skaters(20252026)
    assert len(skaters) == 3
    by_id = {s.player_id: s for s in skaters}
    mcd = by_id[8478402]
    assert mcd.name == "Connor McDavid"
    assert mcd.team == "EDM" and mcd.position == "C"
    for k in nhl.SKATER_STAT_KEYS:
        assert k in mcd.stats, k
    row = load("skater_summary.json")["data"][0]
    rt = {r["playerId"]: r for r in load("skater_realtime.json")["data"]}[8478402]
    fo = {r["playerId"]: r for r in load("skater_faceoffwins.json")["data"]}[8478402]
    assert mcd.stats["PTS"] == row["points"]
    assert mcd.stats["PPP"] == row["ppPoints"]
    assert mcd.stats["SOG"] == row["shots"]
    assert mcd.stats["HIT"] == rt["hits"]
    assert mcd.stats["BLK"] == rt["blockedShots"]
    assert mcd.stats["FOW"] == fo["totalFaceoffWins"]
    assert mcd.toi_per_game == row["timeOnIcePerGame"]
    assert mcd.games_played == row["gamesPlayed"]
    assert mcd.per_game()["PTS"] == pytest.approx(row["points"] / row["gamesPlayed"])
    assert by_id[8476453].position == "RW"


def test_report_paginates_when_capped():
    rows = [{"playerId": i} for i in range(5)]
    calls = []

    def fetch(url, params=None):
        calls.append(params)
        start = params["start"]
        if params["limit"] == -1:
            return {"data": rows[:2], "total": 5}
        return {"data": rows[start:start + 2], "total": 5}

    got = NhlClient(fetch_json=fetch, season=20252026)._report("skater", "summary", None, 2)
    assert [r["playerId"] for r in got] == [0, 1, 2, 3, 4]


def test_all_skaters_without_optional_reports(client):
    skaters = client.all_skaters(20252026, include_realtime=False, include_faceoffs=False)
    assert all("FOW" not in s.stats and "HIT" not in s.stats for s in skaters)
    assert not any("realtime" in u or "faceoff" in u for u, _ in client.fetch_json.calls)


def test_traded_player_takes_last_team():
    row = dict(load("skater_summary.json")["data"][0], teamAbbrevs="SJS, TOR", positionCode="L")
    s = nhl._skater_from_rows(row)
    assert s.team == "TOR" and s.teams == ["SJS", "TOR"] and s.position == "LW"
    assert "HIT" not in s.stats  # no realtime merged


def test_goalie_summary(client):
    goalies = client.goalie_summary(20252026)
    g = goalies[0]
    assert g.name == "Andrei Vasilevskiy" and g.team == "TBL" and g.position == "G"
    for k in nhl.GOALIE_STAT_KEYS:
        assert k in g.stats, k
    assert g.stats["W"] == 39 and g.stats["SV"] == 1353 and g.stats["SA"] == 1483
    assert g.stats["SVPCT"] == pytest.approx(0.91233)


def test_game_log_skater(client):
    log = client.game_log(8478402, 20252026)
    assert len(log) == 4
    e = log[0]
    assert e.date == date(2026, 4, 16)
    assert e.opponent == "VAN" and e.home is True and e.team == "EDM"
    assert e.stats["A"] == 4 and e.stats["PTS"] == 4 and e.stats["PPP"] == 2 and e.stats["SOG"] == 3
    assert e.toi_seconds == 18 * 60 + 16
    assert not e.is_goalie and e.game_type == 2


def test_game_log_goalie(client):
    e = client.game_log(8476883, 20252026)[0]
    assert e.is_goalie and e.decision == "W"
    assert e.stats["W"] == 1 and e.stats["L"] == 0
    assert e.stats["SA"] == 30 and e.stats["GA"] == 3 and e.stats["SV"] == 27
    assert e.stats["SVPCT"] == pytest.approx(0.9)


def test_player_landing(client):
    p = client.player_landing(8478402)
    assert p.name == "Connor McDavid" and p.team == "EDM" and p.position == "C"
    assert p.birth_date == date(1997, 1, 13)
    assert len(p.last5) == 5
    assert p.last5[0].toi_seconds == 24 * 60 + 49 and p.last5[0].home is False
    assert p.featured_regular_season


def test_team_schedule_filters_regular_season(client):
    games = client.team_schedule("EDM")
    assert len(games) == 10
    assert all(g.game_type == 2 for g in games)
    # the NHL API lists the 2026-27 regular season as starting 2026-09-29 (EDM hosts VAN)
    assert games[0].date == date(2026, 9, 29) and games[0].game_id == 2026020004
    assert "EDM" in games[0].teams()
    assert games == sorted(games, key=lambda g: g.date)
    assert len(client.team_schedule("EDM", game_type=None)) == 12


def test_league_schedule_and_window(client):
    sched = client.league_schedule(teams=["EDM", "CGY"])
    assert set(sched) == {"EDM", "CGY"}
    edm = sched["EDM"]
    assert games_in_window(edm, edm[0], edm[0]) == 1
    cutoff = date(2026, 10, 13)
    assert games_in_window(edm, date(2026, 10, 1), cutoff) == sum(1 for d in edm if date(2026, 10, 1) <= d <= cutoff)
    per_day = games_per_day(sched)
    assert set(per_day) == set(edm) | set(sched["CGY"])


def test_games_per_day_off_nights_back_to_backs():
    sched = {"AAA": [date(2026, 10, 7)], "BBB": [date(2026, 10, 7)]}
    sched.update({f"T{i}": [date(2026, 10, 8)] for i in range(16)})
    per_day = games_per_day(sched)
    assert per_day == {date(2026, 10, 7): 1, date(2026, 10, 8): 8}
    assert off_nights(per_day, threshold=8) == {date(2026, 10, 7)}
    b2b = back_to_backs([date(2026, 10, 7), date(2026, 10, 8), date(2026, 10, 10), date(2026, 10, 11)])
    assert b2b == [(date(2026, 10, 7), date(2026, 10, 8)), (date(2026, 10, 10), date(2026, 10, 11))]


def test_team_roster_and_age(client):
    roster = client.team_roster("EDM")
    assert len(roster) == 9  # fixture keeps 3 per group
    dach = next(p for p in roster if p.last_name == "Dach")
    assert dach.player_id == 8482703 and dach.position == "C" and dach.team == "EDM"
    assert dach.name == "Colton Dach"
    assert dach.birth_date == date(2003, 1, 4)
    assert dach.age_on(date(2026, 1, 4)) == pytest.approx(23.0)
    assert 23.7 < dach.age_on(date(2026, 9, 28)) < 23.8
    assert {p.position for p in roster} >= {"D", "G"}
    assert list(client.league_rosters(teams=["EDM"])) == ["EDM"]


def test_week_schedule(client):
    wk = client.week_schedule(date(2026, 10, 7))
    assert date(2026, 10, 7) in wk.days
    g = wk.days[date(2026, 10, 7)][0]
    assert g.away == "PIT" and g.home == "WSH" and g.game_type == 2
    assert g.start_time_utc.isoformat().startswith("2026-10-07T23:30")
    assert wk.games_per_team()["PIT"] >= 1


def test_search_player(client):
    aho = client.search_player("sebastian aho")[0]
    assert aho.player_id == 8478427 and aho.team == "CAR" and aho.position == "C"
    _, params = client.fetch_json.calls[-1]
    assert params["q"] == "sebastian aho" and params["active"] == "true"


def test_nhl_teams_complete():
    assert len(nhl.NHL_TEAMS) == 32 and len(set(nhl.NHL_TEAMS)) == 32


def test_player_landing_draft_and_career():
    body = {"playerId": 8486103, "firstName": {"default": "Ivar"}, "lastName": {"default": "Stenberg"},
            "birthDate": "2007-09-30", "position": "C",
            "draftDetails": {"year": 2026, "teamAbbrev": "SJS", "round": 1, "pickInRound": 2, "overallPick": 2}}
    p = NhlClient(fetch_json=lambda url, params=None: body).player_landing(8486103)
    assert (p.draft_year, p.draft_round, p.draft_overall, p.draft_team) == (2026, 1, 2, "SJS")
    assert p.career_gp == 0                         # no careerTotals yet: no NHL games
    body = {"playerId": 1, "careerTotals": {"regularSeason": {"gamesPlayed": 311}}}
    p = NhlClient(fetch_json=lambda url, params=None: body).player_landing(1)
    assert p.draft_overall is None and p.career_gp == 311
