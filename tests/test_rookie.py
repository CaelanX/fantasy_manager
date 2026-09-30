"""Rookie / unproven-player model (valuation.rookie): league-history parsing (NHL landing
seasonTotals, fixtures trimmed from the live pages of Ivar Stenberg 8486103 - SHL / J20, no NHL
games - Ivan Demidov 8484984 - KHL / MHL / NHL - and Michael Brandsegg-Nygard 8484794 - AHL /
SHL / HockeyAllsvenskan / NHL), NHLe math and age adjustment, pedigree prior, the vote blend,
role signals (DFO + news), the games share, the valuate / dynasty hooks, rookie role alerts and
the player page block."""
import json
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers import preseason_enrich as pe
from fantasy_manager.providers import rookie_enrich as re_
from fantasy_manager.providers.news_roles import RoleSignal
from fantasy_manager.providers.nhl import NHLE_FACTORS, LeagueSeason, NhlClient, load_nhle_factors, parse_season_totals
from fantasy_manager.recommend.flags import (ROOKIE_ALERT_STRENGTH, recommend_role_alerts,
                                             recommend_rookie_role_alerts, rookie_signal_strength)
from fantasy_manager.scoring import from_config
from fantasy_manager.valuation import params as vparams
from fantasy_manager.valuation.blend import preseason_weight
from fantasy_manager.valuation.dynasty import apply_dynasty
from fantasy_manager.valuation.rookie import (GP_AHL, GP_JUNIOR, ROLE_BOOST, ROLE_MINOR_BOOST, W_BASELINE, W_NHLE,
                                              W_PEDIGREE, age_multiplier, has_rookie_evidence, is_unproven,
                                              nhle_estimate, pedigree_prior, rookie_prior, rookie_value)
from fantasy_manager.valuation.valuate import valuate_league

FIX = Path(__file__).parent / "fixtures" / "nhl"
STENBERG, DEMIDOV, NYGARD = 8486103, 8484984, 8484794
AS_OF = date(2026, 9, 29)
NOW = datetime(2026, 9, 28, 18, 0, tzinfo=timezone.utc)


@pytest.fixture(autouse=True)
def _clean(monkeypatch):
    monkeypatch.delenv("FM_ROOKIE_MODEL", raising=False)
    re_.clear_registry()
    pe.clear_registry()
    yield
    re_.clear_registry()
    pe.clear_registry()


def landing(pid):
    return json.loads((FIX / f"landing_history_{pid}.json").read_text(encoding="utf-8"))


def client():
    def fetch(url, params=None):
        for pid in (STENBERG, DEMIDOV, NYGARD):
            if url.endswith(f"/player/{pid}/landing"):
                return landing(pid)
        raise KeyError(url)
    return NhlClient(fetch_json=fetch, season=20262027)


def hist(pid):
    return client().player_history(pid)


# --------------------------------------------------------------------------- history parsing

def test_stenberg_history_from_landing():
    h = hist(STENBERG)
    assert all(isinstance(r, LeagueSeason) for r in h)
    assert [r.season for r in h] == sorted(r.season for r in h)            # oldest first
    shl = [r for r in h if r.league == "SHL"]
    assert [(r.season, r.gp, r.g, r.a, r.pts) for r in shl] == [(20242025, 25, 1, 2, 3), (20252026, 43, 11, 22, 33)]
    assert shl[-1].team == "Frölunda HC" and shl[-1].age_at_season == pytest.approx(18.0, abs=0.01)
    assert shl[0].age_at_season == pytest.approx(17.0, abs=0.01)
    assert not any(r.league == "NHL" for r in h)
    # playoffs (gameTypeId 3) are dropped: 6 SHL playoff games in 2025-26 are not counted
    assert sum(r.gp for r in h if r.season == 20252026 and r.league == "SHL") == 43
    p = client().player_landing(STENBERG)
    assert (p.draft_overall, p.draft_round, p.draft_year, p.career_gp) == (2, 1, 2026, 0)


def test_khl_and_ahl_history():
    d = {(r.season, r.league): r for r in hist(DEMIDOV)}
    k = d[(20242025, "KHL")]
    assert (k.gp, k.g, k.a, k.pts, k.team) == (65, 19, 30, 49, "SKA St. Petersburg")
    assert k.age_at_season == pytest.approx(18.81, abs=0.01)
    assert d[(20252026, "NHL")].gp == 82
    n = {(r.season, r.league): r for r in hist(NYGARD)}
    assert (n[(20252026, "AHL")].gp, n[(20252026, "AHL")].pts) == (60, 44)
    assert (n[(20252026, "NHL")].gp, n[(20252026, "NHL")].pts) == (14, 1)


def test_parse_season_totals_edge_cases():
    rows = [{"season": 20252026, "gameTypeId": 2, "leagueAbbrev": "OHL", "teamName": {"default": "Kitchener"},
             "gamesPlayed": 10, "goals": 3, "assists": 4},                         # no points -> G + A
            {"season": 20252026, "gameTypeId": 2, "leagueAbbrev": "OHL", "gamesPlayed": 0},   # no games
            {"season": "bad", "gameTypeId": 2, "gamesPlayed": 5},
            {"season": 20252026, "gameTypeId": 3, "leagueAbbrev": "OHL", "gamesPlayed": 4, "points": 9}]
    out = parse_season_totals(rows, None)
    assert [(r.league, r.gp, r.pts, r.team, r.age_at_season) for r in out] == [("OHL", 10, 7, "Kitchener", None)]
    assert parse_season_totals(None) == []


def test_nhle_factor_table_is_data():
    t = load_nhle_factors()
    assert t is NHLE_FACTORS or t == NHLE_FACTORS
    lg = t["leagues"]
    assert lg["NHL"]["factor"] == 1.0
    for league, lo, hi in (("AHL", 0.44, 0.47), ("KHL", 0.75, 0.8), ("SHL", 0.55, 0.6), ("Liiga", 0.45, 0.5),
                           ("NCAA", 0.35, 0.4), ("OHL", 0.25, 0.3), ("WHL", 0.25, 0.3), ("QMJHL", 0.25, 0.3),
                           ("Czechia", 0.5, 0.5), ("NLA", 0.45, 0.45)):
        assert lo <= lg[league]["factor"] <= hi, league
    assert "approximate" in t["_about"].lower() and "approximate" in t["version"]


# --------------------------------------------------------------------------- NHLe math

@pytest.mark.parametrize("league,age,mult", [
    ("OHL", 17.0, 1.16), ("OHL", 18.5, 1.04), ("OHL", 19.5, 1.0), ("OHL", 15.0, 1.24),   # junior norm 19, cap 3 yrs
    ("SHL", 18.0, 1.24), ("AHL", 21.0, 1.08), ("AHL", 23.0, 1.0), ("NCAA", 20.0, 1.08),
    ("WJC-20", 17.0, 1.0), ("SHL", None, 1.0),
])
def test_age_multiplier(league, age, mult):
    assert age_multiplier(league, age) == pytest.approx(mult)


def test_nhl_rows_use_the_fitted_age_factor():
    assert age_multiplier("NHL", 20.3, group="F") == pytest.approx(vparams.age_factor("F", 20.3))


def test_stenberg_nhle_by_hand():
    est = nhle_estimate(hist(STENBERG))
    shl25 = 33 / 43 * 0.575 * 1.24          # 18: 4 years under the pro norm (22), capped at 3
    j20 = 53 / 27 * 0.1 * 1.16              # 17 in junior (norm 19)
    shl24 = 3 / 25 * 0.575 * 1.24
    expected = (2 * 43 * shl25 + 27 * j20 + 25 * shl24) / (2 * 43 + 27 + 25)
    assert est["pts_pg"] == pytest.approx(expected, rel=1e-6)
    rel = (43 * 0.9 + 27 * 0.4 + 25 * 0.9) / 95
    assert est["gp"] == 95 and est["confidence"] == pytest.approx(min(1.0, 95 / 60) * rel)
    assert {(r["season"], r["league"]) for r in est["rows"]} == {(20252026, "SHL"), (20242025, "SHL"),
                                                                 (20242025, "J20 Nationell")}


def test_nhle_nhl_rows_and_current_season():
    h = hist(NYGARD)
    with_nhl = nhle_estimate(h)
    no_nhl = nhle_estimate(h, include_nhl=False)
    assert ("NHL" in {r["league"] for r in with_nhl["rows"]}) and "NHL" not in {r["league"] for r in no_nhl["rows"]}
    assert no_nhl["pts_pg"] > with_nhl["pts_pg"]                  # 1 point in 14 NHL games drags it down
    # NHL rows of the season being valued are the in-season model's job
    cur = nhle_estimate(h, before_season=20252026)
    assert "NHL" not in {r["league"] for r in cur["rows"] if r["season"] == 20252026}
    # a season needs 10 GP in table leagues; nothing usable -> None
    assert nhle_estimate([LeagueSeason(season=20252026, league="OHL", gp=5, pts=9)]) is None
    assert nhle_estimate([LeagueSeason(season=20252026, league="WJC-20", gp=7, pts=10)]) is None


# --------------------------------------------------------------------------- pedigree / prior

def sk(cid, *, nhl_id=None, career=0, draft=None, pos=("LW",), team="SJS", proj=None, prior_gp=0, season_gp=0,
       born=date(2007, 9, 30), line=None, pp_unit=None, line_change=None):
    lines = {}
    if proj:
        g, a, gp = proj
        lines["projected"] = StatLine(split="projected", gp=gp, stats={"GP": gp, "G": g, "A": a, "PTS": g + a,
                                                                       "SOG": 2.2 * gp, "PPP": 0.25 * (g + a),
                                                                       "HIT": 0.5 * gp})
    if prior_gp:
        lines["prior"] = StatLine(split="prior", gp=prior_gp, stats={"GP": prior_gp, "G": 0.3 * prior_gp,
                                                                     "A": 0.4 * prior_gp, "PTS": 0.7 * prior_gp,
                                                                     "SOG": 2.5 * prior_gp, "PPP": 0.2 * prior_gp,
                                                                     "HIT": 1.0 * prior_gp})
    if season_gp:
        lines["season"] = StatLine(split="season", gp=season_gp, stats={"GP": season_gp, "G": 0.2 * season_gp,
                                                                        "A": 0.3 * season_gp, "PTS": 0.5 * season_gp,
                                                                        "SOG": 2.0 * season_gp})
    return Player(cid=cid, name=cid, name_norm=cid, ids={"nhl": str(nhl_id)} if nhl_id else {}, team=team,
                  positions=list(pos), career_gp=career, draft_overall=draft, draft_year=2026 if draft else None,
                  lines=lines, birth_date=born, line=line, pp_unit=pp_unit, line_change=line_change)


@pytest.mark.parametrize("pick,pos,pts", [(1, "C", 0.75), (3, "LW", 0.75), (4, "C", 0.6), (10, "RW", 0.6),
                                          (23, "RW", 0.45), (50, "C", 0.35), (120, "C", 0.3), (2, "D", 0.75 * 0.7)])
def test_pedigree_tiers(pick, pos, pts):
    got = pedigree_prior(sk("p", draft=pick, pos=(pos,)), 19.0)
    assert got[0] == pytest.approx(pts) and got[1] == pytest.approx(W_PEDIGREE)


def test_pedigree_fades_and_needs_a_draft_slot():
    assert pedigree_prior(sk("p", draft=None), 19.0) is None
    assert pedigree_prior(sk("p", draft=2), 24.0)[1] == pytest.approx(W_PEDIGREE / 2)
    assert pedigree_prior(sk("p", draft=2, career=100), 19.0)[1] == pytest.approx(W_PEDIGREE / 2)
    assert pedigree_prior(sk("p", draft=2), 25.0) is None and pedigree_prior(sk("p", draft=2, career=250), 19) is None


def test_rookie_prior_reasons():
    pr = rookie_prior(sk("s", draft=2), hist(STENBERG), age=19.0)
    codes = [r.code for r in pr.reasons]
    assert codes == ["NHLE", "PEDIGREE_PRIOR"]
    assert "SHL 43 GP 33 PTS x0.575 x1.24 age" in pr.reasons[0].text and "approximate" in pr.reasons[0].text
    assert "#2 overall 2026 (top-3 pick)" in pr.reasons[1].text
    w = W_NHLE * pr.nhle_confidence + W_PEDIGREE
    assert pr.confidence == pytest.approx(w / (w + W_BASELINE), abs=1e-3)
    assert 0.3 < pr.g_share < 0.45 and pr.sog_pg_est == pytest.approx(pr.pts_pg_nhle * pr.g_share / 0.11)


# --------------------------------------------------------------------------- the blend

BASE = {"G": 0.2, "A": 0.3, "PTS": 0.5, "SOG": 2.0, "PPP": 0.12, "HIT": 0.8}


def test_projection_only_is_unchanged():
    p = sk("p")
    est = rookie_value(p, rookie_prior(p, [], age=19.0), BASE, as_of=AS_OF)
    assert est.rates == BASE and est.pts_pg == pytest.approx(0.5) and est.confidence == pytest.approx(0.5)
    assert [v["name"] for v in est.votes] == ["baseline"]
    assert not any(r.code == "ROOKIE_BLEND" for r in est.reasons)


def test_nhle_and_pedigree_votes():
    p = sk("s", draft=2)
    prior = rookie_prior(p, hist(STENBERG), age=19.0)
    est = rookie_value(p, prior, BASE, as_of=AS_OF)
    wn = W_NHLE * prior.nhle_confidence
    post = (W_BASELINE * 0.5 + wn * prior.pts_pg_nhle + W_PEDIGREE * 0.75) / (W_BASELINE + wn + W_PEDIGREE)
    assert est.pts_pg == pytest.approx(post)
    s = post / 0.5
    for k in ("G", "A", "PTS", "SOG", "PPP"):
        assert est.rates[k] == pytest.approx(BASE[k] * s)
    assert est.rates["HIT"] == BASE["HIT"]                        # non-scoring stats untouched
    r = next(x for x in est.reasons if x.code == "ROOKIE_BLEND")
    assert r.baseline == pytest.approx(0.5) and "baseline 0.50" in r.text and "NHLe" in r.text


def test_no_projection_builds_rates_from_the_priors():
    p = sk("s", draft=2)
    prior = rookie_prior(p, hist(STENBERG), age=19.0)
    mean = {"G": 0.25, "A": 0.35, "PTS": 0.6, "SOG": 2.2, "PPP": 0.18, "HIT": 1.1, "BLK": 0.5}
    est = rookie_value(p, prior, {}, mean=mean, as_of=AS_OF)
    post = (W_NHLE * prior.nhle_confidence * prior.pts_pg_nhle + W_PEDIGREE * 0.75) / \
        (W_NHLE * prior.nhle_confidence + W_PEDIGREE)
    r = est.rates
    assert r["PTS"] == pytest.approx(post) and r["G"] == pytest.approx(post * prior.g_share)
    assert r["G"] + r["A"] == pytest.approx(post) and r["SOG"] == pytest.approx(r["G"] / 0.11)
    assert r["PPP"] == pytest.approx(post * 0.18 / 0.6) and (r["HIT"], r["BLK"]) == (1.1, 0.5)
    # nothing at all -> nothing invented
    assert rookie_value(sk("x"), rookie_prior(sk("x"), [], age=19), {}, as_of=AS_OF).rates == {}


def test_incomparable_baseline_is_not_blended():
    p = sk("s", draft=2)
    goals_only = {"G": 3.17}                                      # a fantasy-points proxy, not a stat line
    est = rookie_value(p, rookie_prior(p, hist(STENBERG), age=19.0), goals_only, as_of=AS_OF)
    assert est.rates == goals_only and "not blended" in est.reasons[0].text


def sig(kind, direction, days_ago=1, conf=0.8, quote=None, ambiguous=False):
    return RoleSignal(player_name="s", kind=kind, direction=direction, confidence=conf,
                      quote=quote or f"{kind} quote", published=NOW - timedelta(days=days_ago), source="rotowire",
                      ambiguous=ambiguous)


def value(p=None, signals=(), proj_gp=None, season_gp=0):
    p = p or sk("s", draft=30)
    return rookie_value(p, None, BASE, news_signals=list(signals), projection_gp=proj_gp, season_gp=season_gp,
                        as_of=AS_OF, deployment=p)


def test_role_boost_from_news_and_dfo():
    plain = value()
    assert plain.role_mult == 1.0 and plain.gp_expectation == pytest.approx(0.75)   # on a team, not confirmed
    top = value(signals=[sig("top_line", 1, quote="skated on the top line with Celebrini")])
    assert top.role_mult == ROLE_BOOST and top.rates["PTS"] == pytest.approx(0.5 * ROLE_BOOST)
    assert top.gp_expectation == pytest.approx(0.95)
    news = next(r for r in top.reasons if r.code == "ROLE_NEWS")
    assert "skated on the top line with Celebrini" in news.text and "rotowire, 2026-09-27" in news.text
    dfo = value(p=sk("s", draft=30, line="f1", pp_unit="pp1"))
    assert dfo.role_mult == ROLE_BOOST and dfo.gp_expectation == 1.0            # in a DFO lineup: confirmed
    assert {r.code for r in dfo.reasons} >= {"ROLE_DFO", "ROLE_BOOST", "GP_EXPECTATION"}
    assert value(p=sk("s", draft=30, line="f2", pp_unit="pp2")).role_mult == ROLE_MINOR_BOOST
    assert value(p=sk("s", draft=30, line="f4")).role_mult == pytest.approx(0.92)
    assert value(signals=[sig("pp1", 1), sig("top_line", 1)]).role_mult == ROLE_BOOST   # not stacked


def test_demotion_zeroes_games_until_a_newer_recall():
    down = value(signals=[sig("ahl_demotion", -1, quote="assigned to AHL Barracuda")])
    assert down.gp_expectation == GP_AHL and down.week_share == GP_AHL
    assert "assigned to AHL Barracuda" in next(r.text for r in down.reasons if r.code == "GP_EXPECTATION")
    assert value(signals=[sig("junior_return", -1)]).gp_expectation == GP_JUNIOR
    # the latest roster-status news wins
    back = value(signals=[sig("ahl_demotion", -1, days_ago=5), sig("nhl_roster", 1, days_ago=1)])
    assert back.gp_expectation == 1.0
    again = value(signals=[sig("nhl_roster", 1, days_ago=5), sig("ahl_demotion", -1, days_ago=1)])
    assert again.gp_expectation == GP_AHL


def test_scratches_stale_and_ambiguous_signals():
    sc = value(signals=[sig("nhl_roster", 1, days_ago=3), sig("scratched", -1)])
    assert sc.gp_expectation == pytest.approx(0.85) and sc.week_share == pytest.approx(0.85)
    assert value(signals=[sig("top_line", 1, days_ago=30)]).role_mult == 1.0       # > 21 days old
    assert value(signals=[sig("top_line", 1, ambiguous=True)]).role_mult == 1.0
    assert value(signals=[sig("top_line", 1, conf=0.5)]).role_mult == 1.0
    llm = sig("top_line", 1, conf=0.65).model_copy(update={"origin": "llm"})
    assert value(signals=[llm]).role_mult == 1.0                                    # LLM labels need 0.7
    assert value(signals=[llm.model_copy(update={"confidence": 0.8})]).role_mult == ROLE_BOOST


def test_games_share_from_projection_team_and_season_games():
    assert value(p=sk("s", draft=5)).gp_expectation == pytest.approx(0.9)            # top-10 pick on a team
    assert value(p=sk("s", draft=30, team=None)).gp_expectation == pytest.approx(0.3)
    assert value(proj_gp=45).gp_expectation == pytest.approx(0.5 * 0.75 + 0.5 * 45 / 75)
    assert value(season_gp=10).gp_expectation == 1.0                                 # playing: confirmed
    confirmed = value(p=sk("s", draft=30, line="f2"), proj_gp=45)                  # DFO lineup: projection 25%
    assert confirmed.gp_expectation == pytest.approx(0.75 * 1.0 + 0.25 * 45 / 75)


# --------------------------------------------------------------------------- valuate / dynasty hooks

def ctx_with(mine=(), theirs=(), fas=()):
    def team(tid, name, me, ps):
        return FantasyTeam(team_id=tid, name=name, owner_is_me=me,
                           slots=[RosterSlot(slot="F", player=p, starting=True) for p in ps])
    vets = [sk(f"v{i}", career=400, prior_gp=80, proj=(20, 30, 80), born=date(1998, 1, 1)) for i in range(3)]
    return LeagueContext(provider="test", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 3.0, "A": 2.0, "PPP": 1.0, "SOG": 0.4,
                                                                       "HIT": 0.2}),
                         roster_shape={"F": 3, "BN": 3},
                         teams=[team("1", "Mine", True, list(mine) + vets), team("2", "Rivals", False, list(theirs))],
                         free_agents=list(fas), matchup_period=1, as_of=AS_OF, season_start=date(2026, 10, 6))


def _vals(ctx):
    return valuate_league(ctx, from_config(ctx.scoring))


def test_valuate_uses_the_rookie_model_for_unproven_only(monkeypatch):
    sten = sk("sten", nhl_id=STENBERG, draft=2, proj=(10, 15, 60))
    vet = sk("vet", nhl_id=8470000, career=500, prior_gp=82, proj=(20, 30, 82), born=date(1995, 1, 1))
    ctx = ctx_with(mine=[sten, vet])
    before = _vals(ctx)
    assert before["sten"].rookie is not None                      # pedigree loaded: the model engages
    assert "rookie" not in before["sten"].model_dump()            # never dumped / archived
    re_.register_history(STENBERG, hist(STENBERG))
    re_.register_history(8470000, hist(DEMIDOV))                  # history / signals for a vet are ignored
    re_.register_signals("vet", [sig("ahl_demotion", -1)])
    after = _vals(ctx)
    assert after["vet"].rates == before["vet"].rates and after["vet"].fpg_season == before["vet"].fpg_season
    assert after["vet"].rookie is None
    assert after["sten"].rates["PTS"] != before["sten"].rates["PTS"]
    blend = next(r for r in after["sten"].reasons if r.code == "ROOKIE_BLEND")
    assert "NHLe" in blend.text and "pedigree" in blend.text and " -> " in blend.text
    assert any(r.code == "NHLE" for r in after["sten"].reasons)
    assert "rookie blend of" in next(r.text for r in after["sten"].reasons if r.code == "BASELINE")
    est = after["sten"].rookie
    assert after["sten"].fpg_season == pytest.approx(after["sten"].fpg * est.gp_expectation)
    # the kill switch restores the plain baseline
    monkeypatch.setenv("FM_ROOKIE_MODEL", "0")
    off = _vals(ctx)
    assert off["sten"].rookie is None and off["sten"].fpg_season == pytest.approx(off["sten"].fpg)


def test_valuate_demotion_and_role_signals():
    sten = sk("sten", nhl_id=STENBERG, draft=2, proj=(10, 15, 60))
    ctx = ctx_with(mine=[sten])
    base = _vals(ctx)["sten"]
    re_.register_signals("sten", [sig("ahl_demotion", -1, quote="assigned to AHL San Jose")])
    down = _vals(ctx)["sten"]
    assert down.fpg == pytest.approx(base.fpg)                    # healthy per-game value unchanged
    assert down.fpg_season == pytest.approx(down.fpg * GP_AHL) and down.fpg_week == pytest.approx(down.fpg * GP_AHL)
    assert any(r.code == "ROOKIE_GAMES" for r in down.reasons)
    re_.register_signals("sten", [sig("top_line", 1)])
    up = _vals(ctx)["sten"]
    assert up.rates["PTS"] == pytest.approx(base.rates["PTS"] * ROLE_BOOST)
    assert up.fpg_season > base.fpg_season


def test_week_projection_applies_the_games_share_until_confirmed():
    from fantasy_manager.providers.nhl import games_per_day
    from fantasy_manager.valuation.schedule import proj_week
    bonk = sk("bonk", draft=30, pos=("D",), proj=(2, 8, 21))           # on a team, roster spot unconfirmed
    vet = sk("vet", nhl_id=8470000, career=500, prior_gp=82, proj=(20, 30, 82), born=date(1995, 1, 1))
    ctx = ctx_with(mine=[vet], fas=[bonk])
    opening = ctx.season_start
    ctx.schedule = {"SJS": [opening + timedelta(days=d) for d in (0, 1, 3, 5)], "TOR": [opening]}
    ctx.games_per_day = games_per_day(ctx.schedule)
    vals = _vals(ctx)
    pv, est = vals["bonk"], vals["bonk"].rookie
    assert est.gp_expectation == pytest.approx(0.515) and est.week_share == est.gp_expectation
    assert pv.games_next7 == 4
    full = proj_week(pv.fpg, 1.0, pv.games_next7, pv.offnight_next7)
    assert pv.proj_week == pytest.approx(0.515 * full)                # not 4 x per-game
    assert pv.fpg_season == pytest.approx(pv.fpg * 0.515)
    assert "week value x0.52" in next(r.text for r in pv.reasons if r.code == "ROOKIE_GAMES")
    assert "rookie games share" in next(r.text for r in pv.reasons if r.code == "PROJ_WEEK")
    # an established player's week projection is untouched
    v = vals["vet"]
    assert v.rookie is None
    assert v.proj_week == pytest.approx(proj_week(v.fpg, 1.0, v.games_next7, v.offnight_next7))
    assert not any(r.code == "ROOKIE_GAMES" for r in v.reasons)
    # confirmed in the NHL lineup: the full week, while the season value still blends the projection's GP
    re_.register_signals("bonk", [sig("nhl_roster", 1)])
    conf = _vals(ctx)["bonk"]
    assert conf.rookie.week_share == 1.0 and conf.rookie.gp_expectation < 1.0
    assert conf.proj_week == pytest.approx(proj_week(conf.fpg, 1.0, conf.games_next7, conf.offnight_next7))


def test_bare_unproven_player_without_evidence_is_untouched():
    bare = sk("bare", career=None, proj=(10, 15, 60))              # pedigree never loaded, no signals
    assert is_unproven(bare) and not has_rookie_evidence(bare, [], [])
    pv = _vals(ctx_with(fas=[bare]))["bare"]
    assert pv.rookie is None and pv.fpg_season == pytest.approx(pv.fpg)


def test_preseason_still_blends_after_the_rookie_model():
    sten = sk("sten", nhl_id=STENBERG, draft=2, proj=(10, 15, 60))
    ctx = ctx_with(mine=[sten])
    re_.register_history(STENBERG, hist(STENBERG))
    before = _vals(ctx)["sten"]
    pe.register([pe.PreseasonLine(nhl_id=STENBERG, season=20262027, gp=3,
                                  stats={"GP": 3, "G": 3, "A": 2, "PTS": 5, "SOG": 8, "PPP": 2})])
    after = _vals(ctx)["sten"]
    w = preseason_weight(3)
    assert after.rates["G"] == pytest.approx((1 - w) * before.rates["G"] + w * 1.0)
    assert [r.code for r in after.reasons].index("ROOKIE_BLEND") < [r.code for r in after.reasons].index("PRESEASON")


def test_dynasty_upside_scaled_by_rookie_confidence():
    sten = sk("sten", nhl_id=STENBERG, draft=2, proj=(10, 15, 60))
    ctx = ctx_with(mine=[sten])
    ctx.dynasty, ctx.keeper_horizon_years, ctx.dynasty_mode = True, 3, "balanced"
    re_.register_history(STENBERG, hist(STENBERG))
    values = _vals(ctx)
    conf = values["sten"].rookie.confidence
    with_conf = apply_dynasty(values, ctx)["sten"]
    values["sten"].rookie = None
    plain = apply_dynasty(values, ctx)["sten"]
    f = 0.6 + 0.4 * conf
    assert with_conf.upside == pytest.approx(plain.upside * f)
    r = next(x for x in with_conf.reasons if x.code == "ROOKIE_CONFIDENCE")
    assert r.value == pytest.approx(f)


# --------------------------------------------------------------------------- enrich reuse

def test_histories_reuse_pedigree_landings_and_respect_the_limit():
    a = sk("a", nhl_id=STENBERG, draft=2)
    b = sk("b", nhl_id=DEMIDOV, draft=5, career=40)
    c = sk("c", nhl_id=NYGARD, draft=20, career=14)
    vet = sk("vet", nhl_id=8470000, career=500, prior_gp=82)
    ctx = ctx_with(mine=[a, b, c, vet])
    calls = []
    cl = client()
    orig = cl.fetch_json
    cl.fetch_json = lambda url, params=None: calls.append(url) or orig(url, params)
    res = re_.load_histories(ctx, cl, {STENBERG: cl.player_landing(STENBERG)}, limit=1,
                             is_cached=lambda nid: nid == NYGARD)
    calls = [u for u in calls if not u.endswith(f"/{STENBERG}/landing")]
    assert res["targets"] == 3 and res["reused"] == 1 and res["fetched"] == 1 and res["cached"] == 1
    assert res["deferred"] == 0 and len(calls) == 2
    assert re_.history_for(a) and re_.history_for(b) and re_.history_for(c) and not re_.history_for(vet)
    re_.clear_registry()
    res = re_.load_histories(ctx, cl, {}, limit=1, is_cached=lambda nid: False)
    assert res["fetched"] == 1 and res["deferred"] == 2


def test_enrich_pedigree_collects_landings():
    from fantasy_manager.providers.enrich import _pedigree

    a = sk("a", nhl_id=STENBERG, career=None)
    ctx = ctx_with(mine=[a])
    got = {}
    _pedigree(ctx, client(), ctx.all_players(), 10, None, got)
    assert set(got) == {STENBERG} and got[STENBERG].season_totals and a.draft_overall == 2


# --------------------------------------------------------------------------- alerts

@pytest.mark.parametrize("kind,conf,strength", [("pp1", 0.8, 6.5), ("pp1", 1.0, 7.0), ("top_line", 0.6, 5.5),
                                                ("top_line", 1.0, 6.5), ("nhl_roster", 0.6, 5.0),
                                                ("nhl_roster", 0.9, 5.75), ("pp2", 0.9, None),
                                                ("ahl_demotion", 0.9, None)])
def test_rookie_signal_strength(kind, conf, strength):
    d = {"kind": kind, "direction": 1 if strength or kind != "ahl_demotion" else -1, "confidence": conf}
    got = rookie_signal_strength(d)
    assert got == (pytest.approx(strength) if strength is not None else None)
    if got is not None:
        assert ROOKIE_ALERT_STRENGTH[0] <= got <= ROOKIE_ALERT_STRENGTH[1]


def test_rookie_role_alerts_cover_mine_rivals_fas_and_negatives_for_mine_only():
    mine = sk("mine", career=0, draft=2)
    theirs = sk("theirs", career=30)
    fa = sk("fa", career=0)
    vet = sk("vet", career=500, prior_gp=82)
    my_down = sk("my_down", career=0)
    fa_down = sk("fa_down", career=0)
    old = sk("old", career=0)
    ctx = ctx_with(mine=[mine, my_down], theirs=[theirs, vet], fas=[fa, fa_down, old])
    signals = {
        "mine": [sig("top_line", 1, quote="skating on the top line with Celebrini")],
        "theirs": [sig("pp1", 1, conf=0.8, quote="on the first power-play unit")],
        "fa": [sig("nhl_roster", 1, conf=0.85, quote="named to the opening-night roster")],
        "vet": [sig("pp1", 1)],
        "my_down": [sig("ahl_demotion", -1, quote="assigned to AHL San Jose")],
        "fa_down": [sig("ahl_demotion", -1)],
        "old": [sig("top_line", 1, days_ago=15)],
    }
    recs = {r.subjects[0].cid: r for r in recommend_rookie_role_alerts(ctx, signals=signals)}
    assert set(recs) == {"mine", "theirs", "fa", "my_down"}
    assert recs["mine"].title == ("Rookie role signal: mine — skating on the top line with Celebrini "
                                  "(rotowire, 2026-09-27)")
    assert (recs["mine"].strength, recs["theirs"].strength, recs["fa"].strength) == \
        (pytest.approx(6.0), pytest.approx(6.5), pytest.approx(5.625, abs=0.01))
    assert recs["my_down"].strength == 5.0 and "assigned to AHL San Jose" in recs["my_down"].title
    assert (recs["theirs"].counterparty, recs["fa"].counterparty, recs["mine"].counterparty) == ("Rivals", "FA", "Mine")
    assert all(r.kind == "alert" and 5.0 <= r.strength <= 7.0 for r in recs.values())
    assert {x.code for x in recs["mine"].reasons} >= {"ROLE_NEWS", "UNPROVEN"}


def test_role_alerts_include_registered_rookie_signals():
    fa = sk("fa", career=0)
    ctx = ctx_with(fas=[fa])
    assert recommend_role_alerts(ctx) == []
    re_.register_signals("fa", [sig("pp1", 1)])
    recs = recommend_role_alerts(ctx)
    assert [r.title.split(":")[0] for r in recs] == ["Rookie role signal"]


# --------------------------------------------------------------------------- player page

def test_player_page_shows_rookie_signals():
    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    from fantasy_manager.web.app import create_app
    from tests.test_web import make_result

    res = make_result()
    p = res.values["a"].player
    prior = rookie_prior(sk("s", draft=2), hist(STENBERG), age=19.0)
    res.values["a"].rookie = rookie_value(p, prior, BASE, news_signals=[sig("top_line", 1, quote="skated on the top line")],
                                          as_of=AS_OF)
    h = TestClient(create_app(lambda league: res)).get("/player/a").text
    assert 'id="h-rookie"' in h and "Rookie signals" in h
    assert "SHL" in h and "Frölunda HC" in h and "J20 Nationell" in h and "&times;0.575" in h
    assert "#2 overall 2026 (top-3 pick)" in h and "skated on the top line" in h and "Games share" in h
    assert 'id="h-rookie"' not in TestClient(create_app(lambda league: make_result())).get("/player/a").text
