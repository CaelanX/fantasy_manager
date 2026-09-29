"""Season availability proportional to the games missed (valuation.adjust.season_availability):
return-date / game-count / duration parsing of status notes, status defaults, the flat fallback,
and the valuate hook (reason RETURN_ESTIMATE; the week horizon stays at 0 while out)."""
from datetime import date, timedelta

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.scoring import from_config
from fantasy_manager.valuation.adjust import (FLOOR, availability_multiplier, expected_return, return_estimate,
                                              season_availability)
from fantasy_manager.valuation.valuate import valuate_league

AS_OF = date(2026, 9, 29)
START = date(2026, 10, 6)
# a WPG-like schedule: 7 games before Oct 17, 82 in all
EARLY = [date(2026, 10, d) for d in (6, 7, 9, 11, 13, 14, 16)]
DATES = EARLY + [date(2026, 10, 17) + timedelta(days=2 * i) for i in range(75)]


def est(status, note, as_of=AS_OF, dates=DATES):
    return return_estimate(status, note, as_of, dates, START)


def test_hellebuyck_style_suspension_is_about_seven_games():
    e = est("suspended", "suspension (est. return 2026-10-17)")
    assert e.return_date == date(2026, 10, 17) and e.games_missed == 7 and e.games_remaining == 82
    assert e.multiplier == pytest.approx(1 - 7 / 82) and 0.9 < e.multiplier < 0.92
    assert "est. return 2026-10-17" in e.text() and "~7 of 82" in e.text()
    assert expected_return("suspended", "suspension (est. return 2026-10-17)", AS_OF, DATES, START) == \
        (date(2026, 10, 17), 7.0)


@pytest.mark.parametrize("status,note,games", [
    ("ir", "Lower Body: Marchand will miss Florida's first 14 regular-season games if he's able to return Nov. 2.", 14),
    ("suspended", "McAvoy is set to serve a six-game suspension to begin the year.", 6),
    ("suspended", "He was suspended three games for boarding.", 3),
    ("dtd", "It's possible Marchenko is forced to miss the first two games of the year.", 2),
    ("suspended", "Suspended", 5),                                    # length unknown: 5 games
])
def test_game_counts(status, note, games):
    e = est(status, note)
    assert e.games_missed == games and e.multiplier == pytest.approx(1 - games / 82)
    assert e.return_date == DATES[games]                              # his first game back


@pytest.mark.parametrize("note,back", [
    ("Gustavsson is expected to be out until at least early November.", date(2026, 11, 5)),
    ("The expectation is for him to rejoin the team sometime in November.", date(2026, 11, 15)),
    ("Gourde won't be available to play until at least December.", date(2026, 12, 1)),
    ("He is expected to return Nov. 2 versus Detroit.", date(2026, 11, 2)),
    ("Expected to be out for another 2-3 months.", START + timedelta(days=round(2.5 * 30.4))),
    ("Tucker is week-to-week and will miss the start of the campaign.", START + timedelta(days=21)),
    ("Dickinson is expected to miss at least the first week of the 2026-27 campaign.", START + timedelta(days=7)),
    ("Jarvis was initially pegged to miss about 4-6 months after having shoulder surgery in June.",
     date(2026, 6, 15) + timedelta(days=round(5 * 30.4))),
])
def test_return_dates_from_notes(note, back):
    e = est("ir", note)
    assert e.return_date == back
    assert e.games_missed == sum(1 for d in DATES if d < back)


def test_past_months_are_not_return_dates():
    # "surgery in January" is when he got hurt: IR without a timetable -> 30 days from opening night
    e = est("ir", "Demko missed the second half of last season after hip surgery in January.")
    assert e.return_date == START + timedelta(days=30) and "30 days assumed" in e.source


def test_status_defaults_and_flat_fallback():
    ir = est("ir", None, as_of=date(2026, 10, 20))                  # in season: 30 days from today
    assert ir.return_date == date(2026, 11, 19)
    assert ir.games_missed == sum(1 for d in DATES if date(2026, 10, 20) <= d < date(2026, 11, 19))
    lt = est("ltir", "Injured Reserve List - LTIR")
    assert lt.multiplier == FLOOR and lt.games_missed == lt.games_remaining
    dtd = est("dtd", "Undisclosed")                                  # day-to-day: 3 days, preseason -> 1 game
    assert dtd.return_date == START + timedelta(days=3) and dtd.multiplier == pytest.approx(1 - 2 / 82)
    back = est("dtd", "Lower Body: day-to-day (est. return 2026-10-02)")   # back before opening night
    assert back.games_missed == 0 and back.multiplier == 1.0
    # nothing to size: the flat value stays
    assert est("out", "OUT") is None and season_availability("out", "OUT", AS_OF)[0] == availability_multiplier("out")
    assert est("healthy", "est. return 2026-12-01") is None and season_availability("healthy", None, AS_OF)[0] == 1.0


def test_without_a_schedule_games_come_from_days():
    e = return_estimate("suspended", "suspension (est. return 2026-10-17)", AS_OF, None, START)
    assert e.games_remaining == 82 and e.games_missed == pytest.approx(11 * 82 / 186)


def mk(cid, status="healthy", note=None, pos=("G",), team="WPG"):
    lines = {"projected": StatLine(split="projected", gp=60, stats={"GP": 60, "GS": 60, "W": 36, "SV": 1600,
                                                                     "GA": 150, "SA": 1750, "SO": 5})} \
        if pos == ("G",) else {"projected": StatLine(split="projected", gp=82, stats={"GP": 82, "G": 30, "A": 40})}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=list(pos), status=status,
                  status_note=note, lines=lines)


def test_valuate_scales_the_season_value_by_games_missed():
    hb = mk("hb", "suspended", "suspension (est. return 2026-10-17)")
    lk = mk("lk")
    sk = mk("sk", "ir", "Marchand will miss Florida's first 14 regular-season games.", pos=("LW",))
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"W": 4.0, "SV": 0.2, "GA": -1.0, "SO": 3.0,
                                                                      "G": 3.0, "A": 2.0}),
                        roster_shape={"G": 1, "LW": 1, "BN": 1},
                        teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True,
                                           slots=[RosterSlot(slot="G", player=hb, starting=True),
                                                  RosterSlot(slot="LW", player=sk, starting=True)])],
                        free_agents=[lk], matchup_period=1, as_of=AS_OF, season_start=START,
                        schedule={"WPG": DATES})
    v = valuate_league(ctx, from_config(ctx.scoring))
    # same projection: the suspended starter keeps ~91% of the healthy goalie's season value
    assert v["hb"].fpg_season == pytest.approx(v["lk"].fpg_season * (1 - 7 / 82), rel=1e-6)
    assert v["hb"].fpg_week == 0.0                                   # week horizon unchanged
    r = next(x for x in v["hb"].reasons if x.code == "RETURN_ESTIMATE")
    assert (r.value, r.baseline) == (7.0, 82.0) and "2026-10-17" in r.text
    assert v["sk"].fpg_season == pytest.approx(v["sk"].fpg * (1 - 14 / 82))
    assert next(x for x in v["sk"].reasons if x.code == "STATUS").value == pytest.approx(1 - 14 / 82)
