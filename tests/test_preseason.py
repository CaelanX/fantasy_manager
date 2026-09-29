"""Preseason (gameType 1) lines as a weak signal for unproven skaters: NHL box-score parsing,
aggregation, unproven detection, the enrich step, the valuation blend (weight cap, fade,
established players untouched), "Preseason standout" alerts and the player-page row.

Fixtures: Ivar Stenberg's (SJS, NHL id 8486103) real 2026 preseason games, trimmed to three
SJS skaters and one opponent per box score: 2026010032 (VGK @ SJS: 0 G 1 A, a PP assist,
22:17), 2026010039 (ANA @ SJS: 3 G 1 A, a PP goal, 15:23), 2026010063 (SJS @ VGK: 0 PTS, 13:37).
"""
import json
from datetime import date
from pathlib import Path

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers import preseason_enrich as pe
from fantasy_manager.providers.nhl import (NhlClient, NhlSkaterGameLine, apply_goal_strength, boxscore_skater_lines,
                                           goal_strength_points)
from fantasy_manager.providers.preseason_enrich import (PreseasonLine, aggregate, enrich_preseason, is_unproven,
                                                        preseason_lines)
from fantasy_manager.recommend.flags import (PRESEASON_STRENGTH, preseason_standout, recommend_preseason_alerts,
                                             recommend_role_alerts)
from fantasy_manager.scoring import from_config
from fantasy_manager.valuation.blend import (PRESEASON_MAX_WEIGHT, PRESEASON_STATS, blend_preseason,
                                             preseason_weight)
from fantasy_manager.valuation.valuate import valuate_league

FIX = Path(__file__).parent / "fixtures" / "nhl"
STENBERG = 8486103
GAMES = (2026010032, 2026010039, 2026010063)
SEASON = 20262027


def load(name):
    return json.loads((FIX / name).read_text(encoding="utf-8"))


@pytest.fixture(autouse=True)
def _clean_registry():
    pe.clear_registry()
    yield
    pe.clear_registry()


class FakeFetch:
    """Routes the SJS club schedule and the three fixture games; 2026010009 (Stenberg did not
    play) has no fixture, so its box score fails like a dead request would."""

    def __init__(self):
        self.calls = []

    def __call__(self, url, params=None):
        self.calls.append(url)
        if "/club-schedule-season/SJS/" in url:
            return load("preseason_club_schedule_SJS.json")
        for gid in GAMES:
            if f"/gamecenter/{gid}/boxscore" in url:
                return load(f"preseason_boxscore_{gid}.json")
            if f"/gamecenter/{gid}/landing" in url:
                return load(f"preseason_landing_{gid}.json")
        raise KeyError(url)


def client(fetch=None):
    return NhlClient(fetch_json=fetch or FakeFetch(), season=SEASON)


# --------------------------------------------------------------------------- NHL parsing

def test_boxscore_lines_and_goal_strength():
    lines = boxscore_skater_lines(load("preseason_boxscore_2026010039.json"))
    st = next(ln for ln in lines if ln.player_id == STENBERG)
    assert (st.team, st.opponent, st.home, st.position, st.game_type) == ("SJS", "ANA", True, "LW", 1)
    assert st.toi == 923.0 and st.date == date(2026, 9, 24)
    assert {k: st.stats[k] for k in ("GP", "G", "A", "PTS", "SOG", "PPG")} == \
        {"GP": 1.0, "G": 3.0, "A": 1.0, "PTS": 4.0, "SOG": 3.0, "PPG": 1.0}
    assert "PPP" not in st.stats and not st.has_strength
    assert {ln.team for ln in lines} == {"SJS", "ANA"}
    apply_goal_strength(lines, goal_strength_points(load("preseason_landing_2026010039.json")))
    assert (st.stats["PPG"], st.stats["PPA"], st.stats["PPP"], st.has_strength) == (1.0, 0.0, 1.0, True)


def test_pp_assist_comes_from_the_scoring_summary():
    lines = client().game_skater_lines(2026010032)
    st = next(ln for ln in lines if ln.player_id == STENBERG)
    assert (st.stats["A"], st.stats["PPG"], st.stats["PPA"], st.stats["PPP"]) == (1.0, 0.0, 1.0, 1.0)


def test_unfinished_or_limited_box_scores_are_empty():
    box = load("preseason_boxscore_2026010039.json")
    assert boxscore_skater_lines({**box, "gameState": "LIVE"}) == []
    assert boxscore_skater_lines({k: v for k, v in box.items() if k != "playerByGameStats"}) == []
    assert goal_strength_points({"summary": {}}) is None


def test_preseason_games_dedupe_and_stop_before_as_of():
    games = client().preseason_games(SEASON, ["SJS", "SJS"], before=date(2026, 9, 26))
    assert [g.game_id for g in games] == [2026010009, 2026010032, 2026010039]
    assert all(g.game_type == 1 for g in client().preseason_games(SEASON, ["SJS"]))


def test_preseason_game_logs_use_game_type_1():
    seen = []

    def fetch(url, params=None):
        seen.append(url)
        return {"seasonId": SEASON, "gameTypeId": 1, "playerStatsSeasons": []}   # what the API returns today

    assert client(fetch).preseason_game_logs(STENBERG) == []
    assert seen == [f"https://api-web.nhle.com/v1/player/{STENBERG}/game-log/{SEASON}/1"]


def test_preseason_skater_games_skip_failing_games():
    lines = client().preseason_skater_games(SEASON, ["SJS"])
    assert {ln.game_id for ln in lines} == set(GAMES)


# --------------------------------------------------------------------------- aggregation

def stenberg_line() -> PreseasonLine:
    lines = [ln for g in GAMES for ln in client().game_skater_lines(g)]
    return aggregate(lines, SEASON)[STENBERG]


def test_aggregate_stenberg():
    ln = stenberg_line()
    assert ln.gp == 3 and ln.has_strength and ln.team == "SJS" and ln.last_date == date(2026, 9, 26)
    assert {k: ln.stats[k] for k in ("G", "A", "PTS", "SOG", "PPG", "PPA", "PPP")} == \
        {"G": 3.0, "A": 2.0, "PTS": 5.0, "SOG": 8.0, "PPG": 1.0, "PPA": 1.0, "PPP": 2.0}
    assert ln.pts_per_game == pytest.approx(5 / 3)
    assert ln.toi_per_game == pytest.approx((1337 + 923 + 817) / 3 / 60)
    assert ln.summary() == "3 GP: 3 G, 2 A, 5 PTS, 8 SOG, 2 PPP, 17.1 TOI"


def test_aggregate_drops_pp_assists_when_a_summary_is_missing():
    a = NhlSkaterGameLine(player_id=1, game_id=1, date=date(2026, 9, 20), toi=900.0, has_strength=True,
                          stats={"GP": 1.0, "G": 1.0, "PPG": 1.0, "PPA": 1.0, "PPP": 2.0})
    b = NhlSkaterGameLine(player_id=1, game_id=2, date=date(2026, 9, 22), toi=None,
                          stats={"GP": 1.0, "G": 0.0, "PPG": 0.0})
    ln = aggregate([a, b, a], SEASON)[1]                 # a repeated game id counts once
    assert ln.gp == 2 and "PPP" not in ln.stats and "PPA" not in ln.stats and ln.stats["PPG"] == 1.0
    assert ln.toi_per_game == pytest.approx(15.0)       # only games with a TOI value


# --------------------------------------------------------------------------- unproven

def sk(cid, nhl_id=None, career=None, prior_gp=None, prior2_gp=None, season_gp=0, proj=True, pos=("LW",),
       pp_unit=None):
    lines = {}
    if proj:
        lines["projected"] = StatLine(split="projected", gp=82,
                                      stats={"GP": 82, "G": 16.4, "A": 24.6, "PTS": 41.0, "SOG": 164.0,
                                             "PPP": 8.2})
    for split, gp in (("prior", prior_gp), ("prior2", prior2_gp)):
        if gp:
            lines[split] = StatLine(split=split, gp=gp, stats={"GP": gp, "G": 0.3 * gp, "A": 0.4 * gp,
                                                               "PTS": 0.7 * gp, "SOG": 2.5 * gp, "PPP": 0.2 * gp})
    if season_gp:
        lines["season"] = StatLine(split="season", gp=season_gp,
                                   stats={"GP": season_gp, "G": 0.2 * season_gp, "A": 0.3 * season_gp,
                                          "PTS": 0.5 * season_gp, "SOG": 2.0 * season_gp,
                                          "PPP": 0.1 * season_gp})
    return Player(cid=cid, name=cid, name_norm=cid, ids={"nhl": str(nhl_id)} if nhl_id else {}, team="SJS",
                  positions=list(pos), career_gp=career, lines=lines, pp_unit=pp_unit)


@pytest.mark.parametrize("career,prior,prior2,expected", [
    (0, None, None, True),        # rookie
    (60, 60, None, True),         # a 60-game rookie season is still < 82 career GP
    (None, 10, 15, True),         # call-up, no 20-GP season, pedigree not loaded
    (None, 25, None, False),      # a 20-GP season and unknown career
    (500, 82, 80, False),         # established
    (120, 30, None, False),       # 82+ career GP and a 20-GP season
])
def test_is_unproven(career, prior, prior2, expected):
    assert is_unproven(sk("p", 1, career, prior, prior2)) is expected


# --------------------------------------------------------------------------- enrich step

def ctx_with(mine=(), theirs=(), fas=(), as_of=date(2026, 9, 29), season_start=date(2026, 10, 1)):
    def team(tid, name, me, ps):
        return FantasyTeam(team_id=tid, name=name, owner_is_me=me,
                           slots=[RosterSlot(slot="F", player=p, starting=True) for p in ps])
    return LeagueContext(provider="test", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 3.0, "A": 2.0, "PPP": 1.0, "SOG": 0.4}),
                         roster_shape={"F": 3, "BN": 2},
                         teams=[team("1", "Mine", True, list(mine)), team("2", "Rivals", False, list(theirs))],
                         free_agents=list(fas), matchup_period=1, as_of=as_of, season_start=season_start)


def test_enrich_registers_unproven_skaters_only():
    rookie = sk("stenberg", STENBERG, career=0)
    vet = sk("vet", 8484994, career=300, prior_gp=82)            # played in the fixture games too
    goalie = Player(cid="g", name="g", name_norm="g", ids={"nhl": "8485402"}, team="SJS", positions=["G"])
    ctx = ctx_with(fas=[rookie, vet, goalie])
    fetch = FakeFetch()
    res = enrich_preseason(ctx, None, client(fetch), SEASON, teams=["SJS"])
    assert res == {"games": 3, "failed": 1, "unproven": 1, "with_games": 1, "attached": 0}
    assert set(preseason_lines(ctx)) == {"stenberg"}
    assert preseason_lines(ctx)["stenberg"].stats["PTS"] == 5.0
    assert pe.preseason_line(vet, SEASON) is None
    assert "preseason" not in rookie.lines          # models.Split has no "preseason" (yet)
    assert any(n.startswith("Preseason (NHL box scores") and "3/4 games" in n and "1 failed" in n
               for n in ctx.source_notes)
    # games on / after as_of are never read (none are fetched mid-game)
    ctx2 = ctx_with(fas=[sk("s2", STENBERG, career=0)], as_of=date(2026, 9, 24))
    assert enrich_preseason(ctx2, None, client(), SEASON, teams=["SJS"])["games"] == 1


def test_enrich_skips_after_the_window_and_without_targets():
    late = ctx_with(fas=[sk("s", STENBERG, career=0)], as_of=date(2026, 11, 20))
    fetch = FakeFetch()
    assert enrich_preseason(late, None, client(fetch), SEASON, teams=["SJS"])["games"] == 0
    assert fetch.calls == [] and "Preseason: skipped" in late.source_notes[0]
    none = ctx_with(fas=[sk("vet", 8484994, career=300, prior_gp=82)])
    assert enrich_preseason(none, None, client(fetch), SEASON, teams=["SJS"])["unproven"] == 0
    assert fetch.calls == []


def test_registry_is_keyed_by_season():
    rookie = sk("stenberg", STENBERG, career=0)
    pe.register([PreseasonLine(nhl_id=STENBERG, season=20252026, gp=4, stats={"GP": 4, "PTS": 8})])
    assert preseason_lines(ctx_with(fas=[rookie])) == {}


def test_cache_ttls_by_game_age():
    class Cache:
        def __init__(self):
            self.ttls = {}

        def get_json(self, url, params=None, ttl=0):
            self.ttls[url] = ttl
            return {}

    c = Cache()
    fetch = pe._fetch(c, date(2026, 9, 29), {1: date(2026, 9, 20), 2: date(2026, 9, 28)})
    fetch("https://api-web.nhle.com/v1/gamecenter/1/boxscore")
    fetch("https://api-web.nhle.com/v1/gamecenter/2/boxscore")
    fetch("https://api-web.nhle.com/v1/club-schedule-season/SJS/20262027")
    assert list(c.ttls.values()) == [pe.TTL_FINAL_GAME, pe.TTL_RECENT_GAME, pe.TTL_SCHEDULE]


# --------------------------------------------------------------------------- valuation blend

@pytest.mark.parametrize("gp,season_gp,w", [
    # 0.25 * gp / (gp + 9), faded by (1 - season_gp / 20)
    (0, 0, 0.0), (1, 0, 0.25 * 1 / 10), (2, 0, 0.25 * 2 / 11), (3, 0, 0.25 * 3 / 12),
    (8, 0, 0.25 * 8 / 17), (30, 0, 0.25 * 30 / 39),
    (6, 10, 0.25 * 6 / 15 * 0.5), (6, 20, 0.0), (6, 40, 0.0),
])
def test_preseason_weight_cap_and_fade(gp, season_gp, w):
    assert preseason_weight(gp, season_gp) == pytest.approx(w)
    assert preseason_weight(gp, season_gp) <= PRESEASON_MAX_WEIGHT


def test_blend_preseason_touches_points_stats_only():
    base = {"G": 0.2, "A": 0.3, "PTS": 0.5, "SOG": 2.0, "PPP": 0.1, "HIT": 1.0, "PIM": 0.4}
    pre = {"G": 1.0, "A": 1.0, "PTS": 2.0, "SOG": 3.0, "PPP": 1.0, "HIT": 0.0, "BLK": 5.0}
    out = blend_preseason(base, pre, 0.25)
    for k in ("G", "A", "PTS", "SOG", "PPP"):
        assert out[k] == pytest.approx(0.75 * base[k] + 0.25 * pre[k])
    assert out["HIT"] == 1.0 and out["PIM"] == 0.4 and "BLK" not in out   # never invents a stat
    assert blend_preseason(base, pre, 0.0) == base
    assert set(PRESEASON_STATS) >= {"G", "A", "PTS", "SOG", "PPP"}


def _values(ctx):
    return valuate_league(ctx, from_config(ctx.scoring))


def test_valuation_blends_unproven_and_leaves_established_untouched():
    rookie, vet = sk("rookie", STENBERG, career=0), sk("vet", 8484994, career=300, prior_gp=82)
    other = sk("other", 8485402, career=0)                       # unproven, no preseason games
    ctx = ctx_with(fas=[rookie, vet, other])
    before = _values(ctx)
    pre = stenberg_line()
    pe.register([pre, PreseasonLine(nhl_id=8484994, season=SEASON, gp=5,
                                    stats={"GP": 5, "G": 10, "A": 10, "PTS": 20, "SOG": 30, "PPP": 5})])
    after = _values(ctx)
    w = preseason_weight(3)
    for k in ("G", "A", "SOG", "PPP"):
        # the rookie's baseline is his projection (no season games), shrunk toward the mean
        assert after["rookie"].rates[k] > before["rookie"].rates[k]
    assert after["rookie"].fpg > before["rookie"].fpg
    r = next(x for x in after["rookie"].reasons if x.code == "PRESEASON")
    assert r.value == pytest.approx(5 / 3, abs=1e-4) and r.baseline == 3.0
    assert "blended at 6%" in r.text and "3 GP: 3 G, 2 A, 5 PTS" in r.text
    # exact formula on the baseline: rates' = (1 - w) * rates + w * preseason (no season games)
    pg = pre.per_game()
    assert after["rookie"].rates["G"] == pytest.approx((1 - w) * before["rookie"].rates["G"] + w * pg["G"])
    # established players and unproven players without preseason games are untouched
    for cid in ("vet", "other"):
        assert after[cid].rates == before[cid].rates
        assert not any(x.code == "PRESEASON" for x in after[cid].reasons)


def test_valuation_fades_with_regular_season_games_and_skips_goalies():
    p20 = sk("p20", STENBERG, career=0, season_gp=20)
    ctx = ctx_with(fas=[p20])
    before = _values(ctx)["p20"]
    pe.register([stenberg_line()])
    after = _values(ctx)["p20"]
    assert after.rates == before.rates
    assert "no longer blended (20 regular-season GP)" in next(x.text for x in after.reasons if x.code == "PRESEASON")
    g = Player(cid="g", name="g", name_norm="g", ids={"nhl": str(STENBERG)}, team="SJS", positions=["G"],
               career_gp=0)
    assert not any(x.code == "PRESEASON" for x in _values(ctx_with(fas=[g]))["g"].reasons)


def test_no_baseline_means_no_blend():
    bare = sk("bare", STENBERG, career=0, proj=False)
    ctx = ctx_with(fas=[bare])
    pe.register([stenberg_line()])
    pv = _values(ctx)["bare"]
    assert pv.fpg == 0.0 and "not blended (no baseline" in next(x.text for x in pv.reasons if x.code == "PRESEASON")


# --------------------------------------------------------------------------- standout alerts

def line_of(gp, pts, toi_min):
    return PreseasonLine(nhl_id=1, season=SEASON, gp=gp, stats={"GP": gp, "G": pts / 2, "A": pts / 2, "PTS": pts},
                         toi_seconds=toi_min * 60 * gp, toi_games=gp)


@pytest.mark.parametrize("gp,pts,toi,strength", [
    (2, 6, 20.0, None),       # < 3 GP
    (3, 2, 15.0, None),       # 0.67 PTS/GP, 15 min
    (3, 3, 12.0, 4.5),        # exactly 1.0 PTS/GP
    (3, 1, 16.0, 4.0),        # exactly 16 min
    (3, 3, 16.0, 5.0),        # both: 4.5 + 0.5
    (4, 6, 14.0, 5.25),       # 1.5 PTS/GP
    (4, 12, 22.0, 6.0),       # 3 PTS/GP and 22 min: capped at 6
])
def test_standout_thresholds(gp, pts, toi, strength):
    c = preseason_standout(line_of(gp, pts, toi))
    if strength is None:
        assert c is None
    else:
        assert c["strength"] == pytest.approx(strength)
        assert PRESEASON_STRENGTH[0] <= c["strength"] <= PRESEASON_STRENGTH[1]


def test_standout_alerts_cover_mine_rivals_and_free_agents():
    mine = sk("mine", 11, career=0, pp_unit="pp1")
    theirs = sk("theirs", 12, career=40)
    fa = sk("fa", STENBERG, career=0)
    vet = sk("vet", 13, career=300, prior_gp=82)
    late = sk("late", 14, career=0, season_gp=5)
    quiet = sk("quiet", 15, career=0)
    ctx = ctx_with(mine=[mine], theirs=[theirs, vet], fas=[fa, late, quiet])
    good = line_of(3, 4, 17.0)
    lines = {"mine": good, "theirs": good, "fa": stenberg_line(), "vet": good, "late": good,
             "quiet": line_of(4, 1, 11.0)}
    recs = {r.subjects[0].cid: r for r in recommend_preseason_alerts(ctx, lines=lines)}
    assert set(recs) == {"mine", "theirs", "fa"}
    assert all(r.kind == "alert" and 4.0 <= r.strength <= 6.0 for r in recs.values())
    assert (recs["mine"].counterparty, recs["theirs"].counterparty, recs["fa"].counterparty) == \
        ("Mine", "Rivals", "FA")
    assert recs["mine"].title == "Preseason standout: mine (3 GP, 1.33 PTS/GP, TOI 17.0, PP1; my team)"
    assert recs["fa"].title == ("Preseason standout: fa (3 GP, 1.67 PTS/GP, TOI 17.1, PP unit unknown; "
                                "free agent)")
    assert "Rivals)" in recs["theirs"].title
    assert {x.code for x in recs["mine"].reasons} >= {"PRESEASON", "PRESEASON_TOI", "PP_UNIT", "UNPROVEN"}


def test_role_alerts_include_registered_standouts():
    fa = sk("stenberg", STENBERG, career=0)
    ctx = ctx_with(fas=[fa])
    assert recommend_role_alerts(ctx) == []
    pe.register([stenberg_line()])
    recs = recommend_role_alerts(ctx)
    assert [r.title.split(":")[0] for r in recs] == ["Preseason standout"]
    assert recs[0].rank_in_kind == 1


# --------------------------------------------------------------------------- player page

def test_player_page_shows_preseason_row():
    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    from fantasy_manager.models import Reason
    from fantasy_manager.web.app import create_app
    from tests.test_web import make_result

    res = make_result()
    res.values["a"].reasons.append(Reason(code="PRESEASON", text="Preseason 3 GP: 3 G, 2 A, 5 PTS, 8 SOG, 2 PPP, "
                                          "17.1 TOI (1.67 PTS/GP): blended at 25% into the baseline",
                                          value=1.6667, baseline=3.0))
    h = TestClient(create_app(lambda league: res)).get("/player/a").text
    assert 'class="preseason-row"' in h and ">Preseason</th>" in h and "blended at 25% into the baseline" in h
    assert 'class="preseason-row"' not in TestClient(create_app(lambda league: make_result())).get("/player/a").text
