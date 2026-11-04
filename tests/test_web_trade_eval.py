"""Web: the Trade evaluator page and the overview's free-agent filters."""
import pytest

pytest.importorskip("fastapi")
pytest.importorskip("jinja2")
pytest.importorskip("httpx")

from fastapi.testclient import TestClient  # noqa: E402

from fantasy_manager.models import Player  # noqa: E402
from fantasy_manager.recommend.trades import evaluate_offer, pick_assets  # noqa: E402
from fantasy_manager.web import views  # noqa: E402
from fantasy_manager.web.app import LoadResult, create_app  # noqa: E402

from . import test_picks as tp  # noqa: E402
from . import test_trades as tt  # noqa: E402


def _result():
    ctx, values = tt.build()
    return LoadResult(ctx=ctx, values=values, dynasty=None, recs=[])


def _client(res):
    return TestClient(create_app(lambda league: res))


# -- trade evaluator -------------------------------------------------------------------------

def test_trade_eval_form_lists_both_rosters_without_a_result():
    h = _client(_result()).get("/trade-eval?team=A").text
    assert "Trade evaluator" in h and 'name="give" value="m_c2"' in h and 'name="get" value="a_d1"' in h
    assert 'name="get" value="b_c"' not in h                     # only the chosen team's roster
    assert "te-result" not in h
    assert 'href="/trade-eval?league=espn"' in h                  # in the nav


def test_trade_eval_shows_the_same_verdict_as_the_cli_evaluator():
    res = _result()
    h = _client(res).get("/trade-eval?league=espn&team=A&give=m_c2&get=a_d1").text
    o = evaluate_offer(res.ctx, res.values, ["m_c2"], ["a_d1"], team_id="A")
    assert f"Verdict: {o.verdict.capitalize()}" in h and f"te-{o.verdict}" in h
    assert f"{o.ev.delta_me:+.2f} pts/game" in h and f"{o.ev.delta_them:+.2f} pts/game (our model)" in h
    assert "Fairness (model)" in h and "Every reason" in h
    assert 'value="m_c2" checked' in h and 'value="a_d1" checked' in h


def test_trade_eval_infers_team_and_drops_stale_players_on_switch():
    c = _client(_result())
    h = c.get("/trade-eval?give=m_c2&get=b_c").text                 # no team: the owner of b_c
    assert 'value="B" selected' in h and "te-result" in h
    h = c.get("/trade-eval?team=A&give=m_c2&get=b_c&switch=1").text  # switched to A: b_c dropped, no result
    assert 'value="A" selected' in h and "te-result" not in h and 'value="b_c"' not in h


def test_trade_eval_error_is_shown_not_raised():
    res = _result()
    page = views.trade_eval_page(res, "A", ["a_c"], [])            # not my player
    assert page["result"] is None and "your roster" in page["error"]
    h = _client(res).get("/trade-eval?team=A&give=a_c").text
    assert "flash-warn" in h and "your roster" in h


def test_trade_eval_with_draft_picks():
    ctx, values, dyn = tp.build()
    res = LoadResult(ctx=ctx, values=values, dynasty=dyn, recs=[])
    theirs = sorted(pick_assets(ctx, values, dyn)["strong"], key=lambda a: -a.value)[0]
    me, them = ctx.my_team.players[5].cid, ctx.teams[0].players[2].cid
    page = views.trade_eval_page(res, "strong", [me], [them], get_picks=[theirs.key])
    assert page["their_picks"] and any(a["checked"] for a in page["their_picks"])
    assert page["result"]["get_picks"][0]["key"] == theirs.key and page["result"]["dynasty_delta"] is not None
    h = _client(res).get(f"/trade-eval?team=strong&give={me}&get={them}&get_pick={theirs.key}").text
    assert 'name="get_pick"' in h and "Dynasty value" in h and "pick" in h


# -- overview free agents --------------------------------------------------------------------

def _fa_result():
    from .test_trades import pv

    res = _result()
    extra = [Player(cid="fa_jh", name="Jack Hughes", name_norm="jack hughes", ids={}, team="NJD",
                    positions=["C"], status="healthy"),
             Player(cid="fa_lw", name="Left Winger", name_norm="left winger", ids={}, team="EDM",
                    positions=["LW"], status="healthy")]
    res.ctx.free_agents.extend(extra)
    res.values["fa_jh"] = pv(extra[0], 0.5)
    res.values["fa_lw"] = pv(extra[1], 0.7)
    res.values["fa_lw"].fpg_week = 9.0                                 # best this week, low season VORP
    return res


def test_free_agents_filters_and_sorts():
    res = _fa_result()
    by_vorp = [r["cid"] for r in views.free_agents(res)]
    assert by_vorp[:2] == ["fa_g", "fa_c"]                             # 1.0, 0.9 FPG
    assert [r["cid"] for r in views.free_agents(res, pos="C")] == ["fa_c", "fa_jh"]
    assert {r["cid"] for r in views.free_agents(res, pos="F")} == {"fa_c", "fa_jh", "fa_lw"}
    assert [r["cid"] for r in views.free_agents(res, q="hughes")] == ["fa_jh"]
    assert views.free_agents(res, sort="fpg_week")[0]["cid"] == "fa_lw"
    assert len(views.free_agents(res, sort="games")) == len(res.ctx.free_agents)
    assert [r["cid"] for r in views.free_agents(res, pos="bogus", sort="bogus")] == by_vorp   # ignored
    assert len(views.free_agents(res, limit=2)) == 2


def test_overview_free_agent_form():
    c = _client(_fa_result())
    h = c.get("/overview").text
    assert 'name="fa_pos"' in h and 'name="fa_q"' in h and 'name="fa_sort"' in h
    assert "top 5 by season VORP" in h and "Clear filters" not in h.split('id="free-agents"')[1]
    sec = c.get("/overview?fa_pos=C&fa_q=hughes&fa_sort=fpg_week").text.split('id="free-agents"')[1]
    assert "Jack Hughes" in sec and "Left Winger" not in sec and "by FPG this week" in sec
    assert "Clear filters" in sec and 'value="C" selected' in sec
    none = c.get("/?fa_q=zzzz").text.split('id="free-agents"')[1]     # "/" is the overview here
    assert "No free agents match these filters." in none
