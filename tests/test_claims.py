"""Claim planner (recommend/claims.py): order with dependent drops, legality at processing,
clear-date games and FAAB bid bounds."""
from datetime import date, timedelta

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.recommend.claims import FAAB_MAX_SHARE, faab_bid, plan_claims
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.valuate import valuate_league

MON = date(2026, 10, 5)
DAYS = [MON + timedelta(days=k) for k in range(7)]


def mk(cid, goals_pg, team="ALL", waiver_until=None):
    lines = {"projected": StatLine(split="projected", gp=80, stats={"G": goals_pg * 80, "GP": 80}),
             "season": StatLine(split="season", gp=20, stats={"G": goals_pg * 20, "GP": 20})}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=["C"], status="healthy",
                  lines=lines, waiver_until=waiver_until)


def ctx_for(mine, fas, faab=None, moves=None):
    slots = [RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR")) for s, p in mine]
    sched = {**{f"Z{i}": DAYS for i in range(4)}, "ALL": DAYS}
    return LeagueContext(provider="test", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}),
                         roster_shape={"C": 1, "BN": 2, "IR": 1},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=fas, matchup_period=1, as_of=MON, period_end=DAYS[-1], schedule=sched,
                         games_per_day={d: 8 for d in DAYS}, moves_limit_per_period=moves,
                         moves_used_this_period=0, lineup_lock="daily", faab_remaining=faab)


def claims_for(ctx, **kw):
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    return plan_claims(ctx, values, **kw)


FAS = [mk("x", 3.0), mk("y", 2.5), mk("z", 0.5)]


def test_order_with_dependent_drops():
    """The second claim's drop is chosen with the first already processed: never the same player."""
    ctx = ctx_for([("C", mk("my_c", 3.5)), ("BN", mk("weak1", 0.1)), ("BN", mk("weak2", 0.2))], FAS)
    cp = claims_for(ctx)
    pairs = [(c.add.cid, c.drop.cid if c.drop else None) for c in cp.claims[:2]]
    assert pairs == [("x", "weak1"), ("y", "weak2")]
    assert [c.order for c in cp.claims[:2]] == [1, 2]


def test_roster_legal_at_processing_after_an_open_spot_fills():
    """One open spot: the first claim takes it, the next one must drop someone."""
    ctx = ctx_for([("C", mk("my_c", 1.0)), ("BN", mk("weak1", 0.1))], FAS)
    cp = claims_for(ctx)
    assert cp.claims[0].add.cid == "x" and cp.claims[0].drop is None
    assert cp.claims[1].add.cid == "y" and cp.claims[1].drop is not None and cp.claims[1].drop.cid == "weak1"


def test_claims_capped_by_moves_left():
    ctx = ctx_for([("C", mk("my_c", 3.5)), ("BN", mk("weak1", 0.1)), ("BN", mk("weak2", 0.2))], FAS, moves=1)
    assert len(claims_for(ctx).claims) == 1
    ctx0 = ctx.model_copy(update={"moves_used_this_period": 1})
    cp0 = claims_for(ctx0)
    assert cp0.claims == [] and any("No moves left" in n for n in cp0.notes)


def test_waiver_claim_counts_from_the_day_after_it_clears():
    tue = DAYS[1]
    ctx = ctx_for([("C", mk("my_c", 0.2)), ("BN", mk("weak1", 0.1)), ("BN", mk("weak2", 0.15))],
                  [mk("w", 3.0, waiver_until=tue), mk("z", 0.5), mk("q", 0.4)])
    cp = claims_for(ctx)
    c = cp.claims[0]
    assert c.add.cid == "w" and c.kind == "claim" and c.clears == tue
    assert c.effective == DAYS[2]
    assert c.games and min(c.games) >= DAYS[2]          # no Mon / Tue games: still on waivers
    assert c.games_7d == 5                               # Wed-Sun in the 7 days from Wed (week ends Sun)


def test_free_agent_counts_today():
    ctx = ctx_for([("C", mk("my_c", 0.2)), ("BN", mk("weak1", 0.1)), ("BN", mk("weak2", 0.15))], FAS)
    c = claims_for(ctx).claims[0]
    assert c.kind == "free agent" and c.effective == MON and c.clears is None
    assert MON in c.games


def test_faab_bid_bounds():
    assert faab_bid(5.0, 100.0) == pytest.approx(100.0 * FAAB_MAX_SHARE)    # capped share
    assert 1.0 <= faab_bid(0.01, 100.0) <= 100.0                             # at least the minimum bid
    assert faab_bid(1.0, 0.0) == 0.0                                         # nothing left
    assert faab_bid(5.0, 3.0) <= 3.0                                         # never above what's left
    assert faab_bid(1.0, 100.0, change=10.0) > faab_bid(1.0, 100.0)          # risers cost more


def test_faab_bid_on_claims_only_with_a_budget():
    mine = [("C", mk("my_c", 0.2)), ("BN", mk("weak1", 0.1)), ("BN", mk("weak2", 0.15))]
    assert all(c.bid is None for c in claims_for(ctx_for(mine, FAS)).claims)
    bids = [c.bid for c in claims_for(ctx_for(mine, FAS, faab=50.0)).claims]
    assert bids and all(b is not None and 1.0 <= b <= 50.0 for b in bids)


def test_espn_waiver_until_from_raw():
    from datetime import datetime

    from fantasy_manager.providers.espn import waiver_until_from_raw

    ms = int(datetime(2026, 10, 28, 9, 0).timestamp() * 1000)
    assert waiver_until_from_raw({"status": "WAIVERS", "waiverProcessDate": ms}).isoformat() == "2026-10-28"
    assert waiver_until_from_raw({"status": "FREEAGENT", "waiverProcessDate": ms}) is None
    assert waiver_until_from_raw({"status": "WAIVERS"}) is None


def test_injured_player_is_never_the_drop():
    hurt = mk("hurt", 0.05)
    hurt.status = "ir"
    ctx = ctx_for([("C", mk("my_c", 3.5)), ("BN", hurt), ("BN", mk("weak2", 0.2))], FAS)
    cp = claims_for(ctx)
    assert all(c.drop is None or c.drop.cid != "hurt" for c in cp.claims)
    healthy = ctx_for([("C", mk("my_c", 3.5)), ("BN", mk("ok", 2.0)), ("BN", mk("weak2", 0.2))], FAS)
    hurt2 = healthy.model_copy(deep=True)
    hurt2.teams[0].slots[1].player.status = "ir"            # "ok" is hurt: weak2 is still droppable
    assert [c.drop.cid for c in claims_for(hurt2).claims if c.drop] == ["weak2"][:len(claims_for(hurt2).claims)]
