"""Weekly transaction budgets and the churn guard (recommend.base moves_* / churn helpers, ESPN
acquisitionSettings, Fantrax claim limits, waivers / injuries / alerts / streaming / CLI)."""
import json
from datetime import date, datetime, timedelta
from pathlib import Path

import pytest
from rich.console import Console
from typer.testing import CliRunner

from fantasy_manager import cli
from fantasy_manager.analysis import schedule_grid as sg
from fantasy_manager.models import ActivityItem, RosterSlot
from fantasy_manager.providers import espn
from fantasy_manager.providers.espn import (acquisition_limits, espn_period_bounds, espn_scoring_dates,
                                            parse_espn_activity, transaction_counter)
from fantasy_manager.providers.fantrax import parse_claim_limit, parse_league_rules, rules_claim_limits
from fantasy_manager.recommend.base import (count_adds, move_scarcity_threshold, moves_budget_reason, moves_left,
                                            moves_text, no_moves_text, recent_adds_from, recently_added)
from fantasy_manager.recommend.injuries import recommend_injuries
from fantasy_manager.recommend.waivers import recommend_waivers
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.valuate import valuate_league

from .test_waivers import league, mk

FIX = Path(__file__).parent / "fixtures"
SETTINGS = json.loads((FIX / "espn" / "player_stats_sample.json").read_text(encoding="utf-8"))
ACTIVITY = json.loads((FIX / "espn" / "activity.json").read_text(encoding="utf-8"))
SCORING = PointsScoring({"G": 1.0})


def budget(ctx, limit=4, used=0, start=date(2026, 9, 28), end=date(2026, 10, 4), label="matchup period"):
    ctx.moves_limit_per_period, ctx.moves_used_this_period = limit, used
    ctx.period_start, ctx.period_end, ctx.moves_period_label = start, end, label
    return ctx


# -- ESPN settings.acquisitionSettings -------------------------------------------------------

def test_espn_acquisition_settings_from_the_live_league():
    acq = SETTINGS["acquisitionSettings"]
    lim = acquisition_limits(acq)                    # 0.2857 per scoring period (day) = 2 per week
    assert lim["per_period"] == 2 and lim["season"] is None and lim["faab_budget"] is None
    assert lim["per_scoring_period"] == pytest.approx(2 / 7) and lim["waiver_hours"] == 24
    assert lim["type"] == "WAIVERS_TRADITIONAL"
    assert acquisition_limits(acq, period_days=6)["per_period"] == 2      # short opening week
    assert acquisition_limits(acq, period_days=14)["per_period"] == 4     # two-week (All-Star) matchup


def test_espn_acquisition_settings_variants():
    assert acquisition_limits({"matchupAcquisitionLimit": 3, "matchupLimitPerScoringPeriod": False,
                               "acquisitionLimit": 40})["per_period"] == 3
    assert acquisition_limits({"acquisitionLimit": 40})["season"] == 40
    unlimited = acquisition_limits({"matchupAcquisitionLimit": -1, "acquisitionLimit": -1})
    assert unlimited["per_period"] is None and unlimited["season"] is None
    assert acquisition_limits({})["per_period"] is None
    faab = acquisition_limits({"isUsingAcquisitionBudget": True, "acquisitionBudget": 100})
    assert faab["faab_budget"] == 100.0


def test_espn_transaction_counter():
    team = {"transactionCounter": {**SETTINGS["transactionCounter"],
                                   "matchupAcquisitionTotals": {"3": 2}, "acquisitions": 7,
                                   "acquisitionBudgetSpent": 12}}
    assert transaction_counter(team, 3) == (2, 7, 12.0)
    assert transaction_counter(team, 4) == (0, 7, 12.0)                 # nothing this period yet
    assert transaction_counter({"transactionCounter": SETTINGS["transactionCounter"]}, 1) == (0, 0, 0.0)
    assert transaction_counter({}, 1) == (None, None, None)


def _ms(d: datetime) -> float:
    return (d - datetime(1970, 1, 1)).total_seconds() * 1000


def test_espn_scoring_dates_and_period_bounds():
    # 7pm ET = 23:00 UTC; a 10:30pm ET start is 02:30 UTC the next day but still the same ET date
    pro = {"settings": {"proTeams": [
        {"id": 1, "proGamesByScoringPeriod": {
            "1": [{"id": 11, "date": _ms(datetime(2026, 9, 29, 23))}],
            "2": [{"id": 12, "date": _ms(datetime(2026, 10, 1, 2, 30))}]}},
        {"id": 2, "proGamesByScoringPeriod": {
            "1": [{"id": 11, "date": _ms(datetime(2026, 9, 29, 23))}]}}]}}
    dates, gpd = espn_scoring_dates(pro)
    assert dates == {1: date(2026, 9, 29), 2: date(2026, 9, 30)}
    assert gpd == {date(2026, 9, 29): 1, date(2026, 9, 30): 1}
    opening = date(2026, 9, 29)                     # Tue; 194 daily scoring periods, 27 matchups
    # a light (All-Star) week is merged away, so the opening week stands alone
    week = lambda a: {a + timedelta(days=i): 7 for i in range(7)}
    games = {}
    d = date(2026, 9, 28)
    while d <= date(2027, 4, 11):
        games.update(week(d))
        d += timedelta(days=7)
    games.update({date(2027, 2, 1) + timedelta(days=i): 0 for i in range(7)})
    assert espn_period_bounds(opening, 194, 27, date(2026, 9, 29), games) == (date(2026, 9, 29),
                                                                                 date(2026, 10, 4))
    assert espn_period_bounds(opening, 194, 27, date(2026, 10, 7), games) == (date(2026, 10, 5),
                                                                                 date(2026, 10, 11))
    # no game counts: 28 weeks for 27 matchups -> the opening week joins week 2
    assert espn_period_bounds(opening, 194, 27, date(2026, 9, 30)) == (date(2026, 9, 29), date(2026, 10, 11))
    assert espn_period_bounds(opening, 194, 27, date(2026, 9, 20), games) == (date(2026, 9, 29), date(2026, 10, 4))
    # ESPN says matchup 2 on Oct 7: the opening week was not merged after all
    assert espn_period_bounds(opening, 194, 27, date(2026, 10, 7), None, 2) == (date(2026, 10, 5), date(2026, 10, 11))
    assert espn_period_bounds(opening, 194, 27, date(2026, 10, 7), None, 1) == (date(2026, 9, 29), date(2026, 10, 11))


class _Req:
    def __init__(self, topics, pro):
        self.topics, self.pro = topics, pro

    def league_get(self, extend="", params=None, headers=None):
        return {"topics": self.topics}

    def get_pro_schedule(self):
        return self.pro


def test_espn_provider_fills_the_budget_from_settings_counter_and_activity():
    from types import SimpleNamespace
    from fantasy_manager.providers.espn import EspnProvider

    pro = {"settings": {"proTeams": [{"id": 1, "proGamesByScoringPeriod": {
        str(i + 1): [{"id": i, "date": _ms(datetime(2026, 9, 29, 23) + timedelta(days=i))}] for i in range(194)}}]}}
    lg = SimpleNamespace(espn_request=_Req(ACTIVITY["topics"], pro), teams=[], player_map={}, current_week=8,
                         currentMatchupPeriod=2, finalScoringPeriod=194)
    prov = EspnProvider(SimpleNamespace(), cache=None)
    prov._league_obj = lg
    ctx = league([], [("C", mk("my_c", ["C"], 0.2))])
    ctx.as_of = date(2026, 10, 6)
    prov._ctx = ctx
    raw_settings = {"settings": {"acquisitionSettings": SETTINGS["acquisitionSettings"],
                                 "scheduleSettings": {"matchupPeriods": {str(i): [i] for i in range(1, 28)}}},
                    "status": {"currentMatchupPeriod": 2, "finalScoringPeriod": 194}}
    raw_league = {"teams": [{"id": 1, "transactionCounter": {**SETTINGS["transactionCounter"],
                                                             "matchupAcquisitionTotals": {"2": 0}}}]}
    prov._apply_budget(lg, raw_league, raw_settings, "1")
    assert (ctx.period_start, ctx.period_end) == (date(2026, 10, 5), date(2026, 10, 11))
    assert ctx.moves_limit_per_period == 2 and ctx.moves_limit_season is None
    # the counter still says 0, the feed shows Monday's add: the conservative 1 wins, with a note
    assert ctx.moves_used_this_period == 1 and moves_left(ctx) == 1
    assert any("counter shows 0" in n for n in ctx.source_notes)
    assert ctx.recent_adds == {"espn:101": date(2026, 10, 5)} and ctx.faab_remaining is None
    assert moves_text(ctx) == "2 per matchup period (1 used, 1 left, resets Mon Oct 12)"


# -- Fantrax Rules page "Claims/Drops" ----------------------------------------------------------

def test_fantrax_claim_limits_unlimited_and_numeric():
    content = json.loads((FIX / "fantrax" / "league_rules.json").read_text(encoding="utf-8"))["content"]
    assert rules_claim_limits(parse_league_rules(content)) == (None, None)      # the user's league
    capped = content.replace("Max # of claims per season:</span>Unlimited",
                             "Max # of claims per season:</span> 30") \
        .replace("Max # of claims per week:</span>Unlimited", "Max # of claims per week:</span> 3")
    assert rules_claim_limits(parse_league_rules(capped)) == (30, 3)
    assert parse_claim_limit("Unlimited") is None and parse_claim_limit(None) is None
    assert parse_claim_limit(" 4 claims") == 4
    # the live league's format: a weekly total with per-claim-type sub-limits
    assert parse_claim_limit("3 (Free Agents:Unlimited, Waiver Wire:Unlimited) starting every Monday") == 3
    assert parse_claim_limit("Unlimited (Free Agents:2, Waiver Wire:1)") == 3
    assert parse_claim_limit("Unlimited (Free Agents:2, Waiver Wire:Unlimited)") is None
    assert rules_claim_limits(None) == (None, None)


# -- moves used / recent adds from the activity feed ---------------------------------------------

def test_moves_used_counts_my_adds_in_the_period_only():
    items = parse_espn_activity(ACTIVITY["topics"])
    # team 1: an add on Mon Oct 5 (+ a drop and a trade the same week: neither counts)
    assert count_adds(items, "1", date(2026, 10, 5), date(2026, 10, 11)) == 1
    assert count_adds(items, "1", date(2026, 9, 28), date(2026, 10, 4)) == 0    # only a TRADE_IN then
    # team 3: a waiver claim (type 180) counts as an acquisition, its drop does not
    assert count_adds(items, "3", date(2026, 9, 28), date(2026, 10, 4)) == 1
    assert count_adds(items, "2", None) == 0
    recent = recent_adds_from(items, "1", date(2026, 10, 6))
    assert recent == {"espn:101": date(2026, 10, 5)}
    assert recent_adds_from(items, "1", date(2026, 10, 25)) == {}               # > 14 days ago


# -- threshold table and texts ----------------------------------------------------------------------

@pytest.mark.parametrize("limit,used,expected,injury", [
    (None, None, 0.3, None), (7, 2, 0.3, None), (4, 0, 0.3, None), (4, 1, 0.6, None), (4, 2, 0.6, None),
    (4, 3, 1.0, 0.3), (4, 4, None, None), (2, 5, None, None)])
def test_move_scarcity_threshold_table(limit, used, expected, injury):
    ctx = budget(league([], []), limit, used)
    assert move_scarcity_threshold(ctx) == expected
    assert move_scarcity_threshold(ctx, injury_replacement=True) == (injury if injury is not None else expected)


def test_moves_left_texts_and_season_cap():
    ctx = budget(league([], []), 4, 2)
    assert moves_left(ctx) == 2
    assert moves_text(ctx) == "4 per matchup period (2 used, 2 left, resets Mon Oct 5)"
    assert moves_budget_reason(ctx).text == "2 of 4 moves left this week: adds must gain >= 0.6 FPG"
    ctx.moves_used_this_period = 4
    assert no_moves_text(ctx) == "No moves left this week (4/4 used); waiver suggestions resume Mon Oct 5"
    ctx.moves_used_this_period, ctx.moves_limit_season, ctx.moves_used_season = 0, 10, 9
    assert moves_left(ctx) == 1                                  # the season cap binds
    assert moves_budget_reason(ctx).text.startswith("1 of 10 moves left this season")
    free = league([], [])
    assert moves_left(free) is None and moves_text(free) == "unlimited" and moves_budget_reason(free) is None
    free.moves_used_this_period = 3
    assert moves_text(free) == "unlimited (3 adds this week)"


# -- waivers: budget and churn guard -----------------------------------------------------------------

def skinner_league(skinner_status="healthy", lank_gpg=0.9):
    """The complaint: Skinner (ESPN) was added yesterday; today Lankinen-for-Skinner came up."""
    skinner = mk("Skinner", ["G"], 0.5, status=skinner_status)
    lank = mk("Lankinen", ["G"], lank_gpg, gp=20)
    ctx = league([lank], [("G", mk("Hellebuyck", ["G"], 1.8)), ("G", skinner), ("C", mk("my_c", ["C"], 1.0))])
    ctx.recent_adds = {"Skinner": ctx.as_of - timedelta(days=1)}
    return ctx


def test_churn_guard_blocks_dropping_a_player_added_yesterday():
    ctx = skinner_league()
    skinner = next(p for p in ctx.my_team.players if p.cid == "Skinner")
    assert recently_added(ctx, skinner) and not recently_added(ctx, skinner, days=1)
    values = valuate_league(ctx, SCORING)
    debug: list = []
    recs = recommend_waivers(ctx, values, debug=debug)
    assert not [r for r in recs if r.add[0].cid == "Lankinen"], [r.title for r in recs]
    guard = [b for b in debug if b["reason"].code == "CHURN_GUARD"]
    assert guard and guard[0]["drop"] == "Skinner" and "added 1 day ago" in guard[0]["reason"].text
    # without the recent add the swap is proposed (the old behaviour)
    ctx.recent_adds = {}
    assert [r.title for r in recommend_waivers(ctx, values)] == ["Add Lankinen, drop Skinner"]
    # a week later the guard no longer applies
    ctx.recent_adds = {"Skinner": ctx.as_of - timedelta(days=7)}
    assert [r.title for r in recommend_waivers(ctx, values)] == ["Add Lankinen, drop Skinner"]


def test_churn_guard_lifts_when_the_recent_add_is_injured_or_the_gain_is_big():
    ctx = skinner_league(skinner_status="out")
    recs = recommend_waivers(ctx, valuate_league(ctx, SCORING))
    r = next(r for r in recs if r.add[0].cid == "Lankinen")
    assert r.title == "Add Lankinen, drop Skinner"
    guard = next(x for x in r.reasons if x.code == "CHURN_GUARD")
    assert "added 1 day ago" in guard.text and "he is out" in guard.text
    ctx = skinner_league(lank_gpg=2.2)                             # +1.7 FPG >= 1.5
    recs = recommend_waivers(ctx, valuate_league(ctx, SCORING))
    assert [r.title for r in recs] == ["Add Lankinen, drop Skinner"]


def test_churn_guard_falls_back_to_the_next_drop_with_a_note():
    fa = mk("fa_c", ["C"], 1.5, gp=20)
    new, old = mk("new_c", ["C"], 0.2), mk("old_c", ["C"], 0.4)
    ctx = league([fa], [("C", new), ("BN", old)])
    ctx.recent_adds = {"new_c": ctx.as_of - timedelta(days=2)}
    r = recommend_waivers(ctx, valuate_league(ctx, SCORING))[0]
    assert [p.cid for p in r.drop] == ["old_c"]
    note = next(x for x in r.reasons if x.code == "CHURN_GUARD")
    assert "Keeping new_c (added 2 days ago)" in note.text


def test_waivers_respect_the_move_budget():
    fa_big, fa_small = mk("big", ["C"], 1.6, gp=20), mk("small", ["LW"], 0.8, gp=20)
    ctx = league([fa_big, fa_small], [("C", mk("my_c", ["C"], 0.2)), ("LW", mk("my_lw", ["LW"], 0.2))])
    values = valuate_league(ctx, SCORING)
    assert {r.add[0].cid for r in recommend_waivers(ctx, values)} == {"big", "small"}    # unlimited
    budget(ctx, limit=4, used=0)
    recs = recommend_waivers(ctx, values)
    assert {r.add[0].cid for r in recs} == {"big", "small"}
    mb = [next(x for x in r.reasons if x.code == "MOVE_BUDGET") for r in recs]
    assert all(x.text == "4 of 4 moves left this week" and x.value == 4 for x in mb)
    budget(ctx, limit=4, used=3)                                   # last move: +1.0 FPG needed
    recs = recommend_waivers(ctx, values)
    assert [r.add[0].cid for r in recs] == ["big"]
    assert next(x for x in recs[0].reasons if x.code == "MOVE_BUDGET").text.startswith("1 of 4 moves left")
    budget(ctx, limit=4, used=4)
    assert recommend_waivers(ctx, values) == []


def test_last_move_still_allowed_for_an_injury_replacement():
    fa, my_c = mk("fa_c", ["C"], 0.9, gp=20), mk("my_c", ["C"], 0.2)
    hurt = mk("hurt", ["LW"], 0.9, status="ir")             # +0.7 FPG: under the last-move bar of 1.0
    ctx = budget(league([fa], [("C", my_c), ("LW", hurt)], shape={"C": 1, "LW": 1, "IR": 1}), limit=2, used=1)
    recs = recommend_waivers(ctx, valuate_league(ctx, SCORING))
    assert [r.title for r in recs] == ["Add fa_c, move hurt to IR"]           # replaces an injured player
    ctx2 = budget(league([fa], [("C", my_c), ("LW", hurt)], shape={"C": 1, "LW": 1}), limit=2, used=1)
    assert recommend_waivers(ctx2, valuate_league(ctx2, SCORING)) == []       # a plain swap is not worth it
    ctx2.moves_used_this_period = 0
    assert [r.title for r in recommend_waivers(ctx2, valuate_league(ctx2, SCORING))] == ["Add fa_c, drop my_c"]
    # dropping an OUT player may use the last move; a suspended one is a normal swap (the live
    # "add Lankinen, drop the suspended Hellebuyck" case)
    for status, titles in (("out", ["Add fa_c, drop my_c"]), ("suspended", [])):
        c3 = budget(league([fa], [("C", mk("my_c", ["C"], 0.2, status=status))], shape={"C": 1}), limit=2, used=1)
        assert [r.title for r in recommend_waivers(c3, valuate_league(c3, SCORING))] == titles, status


# -- injuries: IR chain and activation ----------------------------------------------------------------

def test_ir_chain_add_respects_the_budget():
    hurt = mk("hurt", ["C"], 1.0, status="ir")
    fa_c = mk("fa_c", ["C"], 0.6)
    ctx = league([fa_c], [("C", hurt), ("LW", mk("other", ["LW"], 0.4))], shape={"C": 1, "LW": 1, "BN": 1, "IR": 1})
    values = valuate_league(ctx, SCORING)
    budget(ctx, limit=2, used=1)                                   # one left: the replacement may use it
    move = next(r for r in recommend_injuries(ctx, values, {}) if r.title.startswith("Move hurt"))
    assert move.title == "Move hurt to IR and add fa_c"
    assert "1 of 2 moves left" in next(x.text for x in move.reasons if x.code == "MOVE_BUDGET")
    budget(ctx, limit=2, used=2)                                   # none left: the IR move alone
    move = next(r for r in recommend_injuries(ctx, values, {}) if r.title.startswith("Move hurt"))
    assert move.title == "Move hurt to IR" and move.add == []
    assert "No moves left this week (2/2 used)" in next(x.text for x in move.reasons if x.code == "NO_MOVES")


def test_ir_activation_does_not_drop_a_recent_add():
    back = mk("back", ["C"], 1.0)
    weak, other = mk("weak", ["C"], 0.1), mk("other", ["LW"], 0.5)
    ctx = league([], [("IR", back), ("C", weak), ("LW", other)], shape={"C": 1, "LW": 1, "IR": 1})
    ctx.recent_adds = {"weak": ctx.as_of - timedelta(days=1)}
    rec = next(r for r in recommend_injuries(ctx, valuate_league(ctx, SCORING), {}) if r.title.startswith("Activate"))
    assert [p.cid for p in rec.drop] == ["other"]
    assert "weak (added 1 day ago)" in next(x.text for x in rec.reasons if x.code == "CHURN_GUARD")


# -- streaming and alerts -------------------------------------------------------------------------

def test_streaming_targets_carry_the_moves_note():
    from .test_schedule_grid import _p, _pv, make_ctx
    ctx = make_ctx(free_agents=[_p("fa1", "AAA"), _p("fa4", "AAA", ("G",))])
    values = {"fa1": _pv(2.0), "fa4": _pv(4.0, share=0.5)}
    plan = sg.streaming_targets(ctx, values, date(2026, 10, 5), today=date(2026, 10, 5))
    assert plan.moves_note is None and plan.by_slot["F"][0].note is None
    budget(ctx, limit=2, used=1, start=date(2026, 10, 5), end=date(2026, 10, 11))
    plan = sg.streaming_targets(ctx, values, date(2026, 10, 5), today=date(2026, 10, 5))
    t = plan.by_slot["F"][0]
    assert t.note == "1 of 2 moves left this week" and t.needs_drop is None
    ctx.my_team.slots += [RosterSlot(slot="BN", player=_p(f"g{i}", "BBB", ("G",)), starting=False) for i in range(3)]
    ctx.position_limits = {"G": 3}
    plan = sg.streaming_targets(ctx, values, date(2026, 10, 5), today=date(2026, 10, 5))
    assert plan.by_slot["G"][0].needs_drop == "G limit 3 reached: drop a G; 1 of 2 moves left this week"
    ctx.moves_used_this_period = 2
    plan = sg.streaming_targets(ctx, values, date(2026, 10, 5), today=date(2026, 10, 5))
    assert plan.by_slot["F"][0].needs_drop == "no moves left this week (2/2 used)"


def test_streamer_alert_appends_the_moves_note():
    from fantasy_manager.recommend.alerts import recommend_line_alerts
    from .test_schedule_grid import make_ctx
    ctx = make_ctx()
    g = mk("fa_g", ["G"], 0.5)
    g.team, g.confirmed_start, g.start_source = "AAA", True, "DFO Confirmed: starts AAA@XXX"
    ctx.free_agents = [g]
    ctx.opponents["AAA"][ctx.as_of] = "XXX"
    budget(ctx, limit=2, used=1, start=date(2026, 10, 5), end=date(2026, 10, 11))
    recs = recommend_line_alerts(ctx, weak_teams={"XXX": 2.1})
    r = next(r for r in recs if r.subjects[0].cid == "fa_g")
    assert next(x.text for x in r.reasons if x.code == "MOVE_BUDGET").startswith("1 of 2 moves left this week")
    ctx.moves_used_this_period = 2
    r = next(r for r in recommend_line_alerts(ctx, weak_teams={"XXX": 2.1}) if r.subjects[0].cid == "fa_g")
    assert next(x.text for x in r.reasons if x.code == "MOVE_BUDGET").startswith("No moves left this week (2/2 used)")


# -- CLI ------------------------------------------------------------------------------------------

runner = CliRunner()


def _cli(monkeypatch, tmp_path, ctx_fn):
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("ESPN_LEAGUE_ID", "1")
    monkeypatch.setenv("FM_OFFLINE", "1")
    cli.get_settings.cache_clear()
    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: ctx_fn())
    monkeypatch.setattr(cli, "console", Console(width=250))


def _budget_ctx(used):
    ctx = league([mk("fa_c", ["C"], 1.6, gp=20)], [("C", mk("my_c", ["C"], 0.2))])
    return budget(ctx, limit=4, used=used)


def test_cli_waivers_with_no_moves_left(monkeypatch, tmp_path):
    _cli(monkeypatch, tmp_path, lambda: _budget_ctx(4))
    res = runner.invoke(cli.app, ["waivers"])
    assert res.exit_code == 0, res.output
    assert "No moves left this week (4/4 used); waiver suggestions resume Mon Oct 5." in res.output
    assert "Moves: 4 per matchup period (4 used, 0 left, resets Mon Oct 5)" in res.output
    res = runner.invoke(cli.app, ["--json", "waivers"])
    assert json.loads(res.output) == []


def test_cli_settings_waivers_roster_show_the_moves_line(monkeypatch, tmp_path):
    _cli(monkeypatch, tmp_path, lambda: _budget_ctx(2))
    line = "Moves: 4 per matchup period (2 used, 2 left, resets Mon Oct 5)"
    for args in (["settings"], ["waivers"], ["roster"]):
        res = runner.invoke(cli.app, args)
        assert res.exit_code == 0, res.output
        assert line in res.output, args
    res = runner.invoke(cli.app, ["--json", "settings"])
    moves = json.loads(res.output)["moves"]
    assert moves["limit_per_period"] == 4 and moves["left"] == 2 and moves["period_end"] == "2026-10-04"
    _cli(monkeypatch, tmp_path, lambda: league([], [("C", mk("my_c", ["C"], 0.2))]))
    assert "Moves: unlimited" in runner.invoke(cli.app, ["settings"]).output


def test_fantrax_provider_fills_the_budget(tmp_path):
    from .test_fantrax import FakeSession, fx, provider

    prov = provider(tmp_path)                            # Rules page without a Claims/Drops section
    ctx = prov.load()
    assert ctx.moves_limit_per_period is None and ctx.moves_used_this_period == 0
    assert (ctx.period_start, ctx.period_end) == (date(2026, 9, 28), date(2026, 10, 4))
    assert ctx.recent_adds == {"fantrax:p4": date(2026, 9, 21)}          # my FA claim a week ago
    assert moves_text(ctx) == "unlimited (0 adds this week)"
    content = fx("league_rules")["content"].replace(
        "Max # of claims per week:</span>Unlimited", "Max # of claims per week:</span> 3")
    prov = provider(tmp_path / "capped", session=FakeSession(
        overrides={"getLeagueRulesOld": {"newJoin": False, "content": content}}))
    ctx = prov.load()
    assert ctx.moves_limit_per_period == 3 and ctx.moves_limit_season is None and moves_left(ctx) == 3
    assert ctx.moves_period_label == "week"
    assert any(line.startswith("Claims: max per season Unlimited, per week 3") for line in prov.rule_summary())


def test_web_footer_league_line_shows_the_moves():
    from fastapi.testclient import TestClient
    from fantasy_manager.web.app import create_app
    from .test_web import make_result

    res = make_result()
    budget(res.ctx, limit=2, used=1)
    h = TestClient(create_app(lambda league: res)).get("/").text
    assert "moves: 2 per matchup period (1 used, 1 left, resets Mon Oct 5)" in h
    assert "moves:" not in TestClient(create_app(lambda league: make_result())).get("/").text
