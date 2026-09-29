import json
import sys

import pytest

from rich.console import Console
from typer.testing import CliRunner

from fantasy_manager import cli
from fantasy_manager.providers import espn

from .test_waivers import league, mk

runner = CliRunner()


def _fake_ctx():
    ctx = league([mk("fa_c", ["C"], 1.0, gp=20), mk("fa_d", ["D"], 0.2)],
                 [("C", mk("my_c", ["C"], 0.2)), ("LW", mk("my_lw", ["LW"], 0.4)),
                  ("IR", mk("my_ir", ["C"], 0.0, status="ir"))])
    return ctx


def _patch(monkeypatch, tmp_path):
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("ESPN_LEAGUE_ID", "1")
    monkeypatch.setenv("FM_OFFLINE", "1")  # enrichment degrades to warnings; never touches the network
    cli.get_settings.cache_clear()
    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: _fake_ctx())
    monkeypatch.setattr(cli, "console", Console(width=250))


def test_help():
    res = runner.invoke(cli.app, ["--help"])
    assert res.exit_code == 0 and "waivers" in res.output


def test_backtest_is_mounted():
    res = runner.invoke(cli.app, ["backtest", "--help"])
    assert res.exit_code == 0
    for cmd in ("run", "fit", "archive", "grade", "report", "data"):
        assert cmd in res.output, cmd
    assert "backtest" in runner.invoke(cli.app, ["--help"]).output


def test_commands_with_fake_provider(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    for args in (["settings"], ["roster", "--all-teams"], ["waivers", "--horizon", "week"]):
        res = runner.invoke(cli.app, args)
        assert res.exit_code == 0, res.output
    res = runner.invoke(cli.app, ["--json", "waivers"])
    assert res.exit_code == 0, res.output
    recs = json.loads(res.output)
    assert recs[0]["add"][0]["cid"] == "fa_c" and recs[0]["drop"][0]["cid"] == "my_c"


def test_fantrax_unavailable_is_friendly(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    monkeypatch.setitem(sys.modules, "fantasy_manager.providers.fantrax", None)  # import -> ImportError
    res = runner.invoke(cli.app, ["--league", "fantrax", "roster"])
    assert res.exit_code == 2
    assert "not available yet" in res.output


def test_missing_league_id(monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("ESPN_LEAGUE_ID", raising=False)
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    cli.get_settings.cache_clear()
    res = runner.invoke(cli.app, ["settings"])
    assert res.exit_code == 1
    cli.get_settings.cache_clear()


# -- milestone 2 commands -----------------------------------------------------

def _bench_ctx():
    """my_c starts at C while a much better C (bench_c) sits on the bench; my_ir is healthy in IR."""
    ctx = _fake_ctx()
    team = ctx.my_team
    from fantasy_manager.models import RosterSlot
    team.slots.append(RosterSlot(slot="BN", player=mk("bench_c", ["C"], 2.0, gp=20), starting=False))
    ctx.roster_shape = {"C": 1, "LW": 1, "D": 1, "BN": 2, "IR": 1}
    team.slots[2].player.status = "healthy"
    return ctx


def test_lineup_command(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: _bench_ctx())
    res = runner.invoke(cli.app, ["lineup"])
    assert res.exit_code == 0, res.output
    assert "Start bench_c over my_c" in res.output
    res = runner.invoke(cli.app, ["--json", "lineup", "--horizon", "season"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.output)
    assert data["optimal"][0] == {**data["optimal"][0], "slot": "C", "cid": "bench_c", "current_slot": "BN"}
    titles = [r["title"] for r in data["recommendations"]]
    assert "Start bench_c over my_c" in titles and any(t.startswith("Activate my_ir") for t in titles)
    assert data["optimal_total"] > data["current_total"]
    assert data["meta"]["warnings"]          # offline: NHL enrichment reported, not fatal


def test_injuries_command_snapshots(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["--json", "injuries"])
    assert res.exit_code == 0, res.output
    first = json.loads(res.output)
    assert first["first_snapshot"] is True and first["status_changes_recorded"] > 0
    assert [p["cid"] for p in first["injured"]] == ["my_ir"]

    def worse():
        ctx = _fake_ctx()
        ctx.my_team.slots[0].player.status = "out"
        return ctx

    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: worse())
    res = runner.invoke(cli.app, ["--json", "injuries"])
    second = json.loads(res.output)
    assert second["first_snapshot"] is False
    assert any(r["title"] == "my_c now OUT (was healthy)" for r in second["recommendations"])
    res = runner.invoke(cli.app, ["injuries", "--no-record"])
    assert res.exit_code == 0, res.output


def test_sync_review_confirm_refresh(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    from fantasy_manager.matching.crosswalk import Crosswalk
    from fantasy_manager.matching.matcher import Candidate
    from fantasy_manager.models import Player

    xw = Crosswalk(tmp_path)
    xw.resolve([Player(cid="espn:9", name="Elias Pettersson", name_norm="", ids={}, team="VAN", positions=["C"])],
               [Candidate(key=1, name="Elias Pettersson", team="VAN"), Candidate(key=2, name="Elias Pettersson",
                                                                                team="VAN")])
    xw.close()
    res = runner.invoke(cli.app, ["sync", "--review"])
    assert res.exit_code == 0 and "Elias Pettersson" in res.output and "espn:9" in res.output
    res = runner.invoke(cli.app, ["--json", "sync", "--confirm", "espn:9=2"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.output)["confirmed"][0]["nhl_id"] == 2
    res = runner.invoke(cli.app, ["--json", "sync", "--review"])
    assert json.loads(res.output)["pending"] == []
    res = runner.invoke(cli.app, ["sync", "--confirm", "garbage"])
    assert res.exit_code == 1
    res = runner.invoke(cli.app, ["--json", "sync", "--refresh"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.output)["sync"]["players"] == 5


def test_deep_and_json_everywhere(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    for args in (["roster", "--deep"], ["waivers", "--deep"], ["lineup", "--deep"]):
        res = runner.invoke(cli.app, args)
        assert res.exit_code == 0, res.output
    for args in (["settings"], ["roster"], ["waivers"], ["lineup"], ["injuries"], ["sync"]):
        res = runner.invoke(cli.app, ["--json", *args])
        assert res.exit_code == 0, (args, res.output)
        json.loads(res.output)


def test_roster_dynasty_column_and_fantrax_settings(monkeypatch, tmp_path):
    import types

    monkeypatch.setenv("FANTRAX_MODE", "contend")  # beats the default and any .env value
    _patch(monkeypatch, tmp_path)

    class DV:
        def __init__(self, v):
            self.value, self.age, self.age_mult, self.upside, self.reasons = v, 25.0, 1.0, 0.0, []

    fake = types.ModuleType("fantasy_manager.valuation.dynasty")
    fake.apply_dynasty = lambda values, ctx: {cid: DV(7.5) for cid in values}
    monkeypatch.setitem(sys.modules, "fantasy_manager.valuation.dynasty", fake)

    class FakeFantrax:
        warnings = ["free-agent pool unavailable"]

        def load(self):
            ctx = _fake_ctx()
            ctx.provider, ctx.dynasty = "fantrax", True
            ctx.position_limits, ctx.max_roster_size = {"G": 3}, 16
            return ctx

        def describe_settings(self):
            return ["Minor league slots: 5"]

    monkeypatch.setattr(cli, "get_provider", lambda name, settings, cache: FakeFantrax())
    res = runner.invoke(cli.app, ["--league", "fantrax", "roster"])
    assert res.exit_code == 0, res.output
    assert "Dynasty" in res.output and "7.50" in res.output
    assert "free-agent pool unavailable" in res.output
    assert " Age " in res.output and "Dynasty mode: contend (from FANTRAX_MODE)" in res.output
    res = runner.invoke(cli.app, ["--json", "--league", "fantrax", "roster"])
    assert json.loads(res.output)[0]["slots"][0]["dynasty"]["value"] == 7.5
    res = runner.invoke(cli.app, ["--league", "fantrax", "settings"])
    assert res.exit_code == 0 and "Minor league slots: 5" in res.output and "mode contend" in res.output
    assert "Roster maximums: max G 3 (incl. IR); roster max 16 (excl. IR)" in res.output
    res = runner.invoke(cli.app, ["--json", "--league", "fantrax", "settings"])
    data = json.loads(res.output)
    assert data["provider_settings"] == ["Minor league slots: 5"] and data["dynasty"] is True
    assert data["dynasty_mode"] == "contend" and data["dynasty_mode_source"] == "env"
    assert data["position_limits"] == {"G": 3} and data["max_roster_size"] == 16


def test_prospect_marker():
    from datetime import date

    ctx = _fake_ctx()
    kid = mk("kid", ["C"], 0.5)
    kid.birth_date = date(2007, 9, 30)
    assert cli._age(kid, ctx, None) == pytest.approx(19.0, abs=0.02)
    assert cli._is_prospect(kid, 19.0)                      # no NHL games yet
    kid.career_gp = 150
    assert not cli._is_prospect(kid, 19.0)
    kid.career_gp = None
    assert not cli._is_prospect(kid, 23.5)
    assert not cli._is_prospect(kid, None)


# -- milestone 4/5 commands ---------------------------------------------------

def _two_team_ctx():
    """My surplus C (my_c2, benched) for their LW (opp_lw): fair on VORP, both lineups improve.
    my_hot is on a lucky L15 heater (sell high); opp_cold is cold with intact shots (buy low)."""
    from fantasy_manager.models import FantasyTeam, RosterSlot, StatLine

    def form(cid, pos, rate, l15_g, l15_sog, sog_rate):
        p = mk(cid, pos, rate, gp=10)
        p.lines["projected"].stats["SOG"] = sog_rate * 80
        p.lines["season"].stats["SOG"] = sog_rate * 10
        p.lines["last15"] = StatLine(split="last15", gp=6, stats={"G": l15_g, "SOG": l15_sog, "GP": 6})
        return p

    ctx = league([mk("fa_c", ["C"], 0.2), mk("fa_lw", ["LW"], 0.2), mk("fa_d", ["D"], 0.2)],
                 [("C", mk("my_c", ["C"], 2.0)), ("LW", mk("my_lw", ["LW"], 0.5)),
                  ("D", form("my_hot", ["D"], 1.0, l15_g=9, l15_sog=12, sog_rate=3.0)),
                  ("BN", mk("my_c2", ["C"], 3.0)), ("IR", mk("my_ir", ["C"], 0.0, status="ir"))])
    ctx.roster_shape = {"C": 1, "LW": 1, "D": 1, "BN": 2, "IR": 1}
    opp = [("C", mk("opp_c", ["C"], 1.0)), ("LW", mk("opp_lw", ["LW"], 3.0)),
           ("D", form("opp_cold", ["D"], 1.5, l15_g=0, l15_sog=24, sog_rate=4.0)),
           ("BN", mk("opp_lw2", ["LW"], 2.0))]
    ctx.teams.append(FantasyTeam(team_id="2", name="them", owner_is_me=False,
                                 slots=[RosterSlot(slot=s, player=p, starting=s != "BN") for s, p in opp]))
    return ctx


def _patch2(monkeypatch, tmp_path):
    _patch(monkeypatch, tmp_path)
    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: _two_team_ctx())
    for var in ("OPENROUTER_API_KEY", "DISCORD_WEBHOOK_URL", "SLACK_WEBHOOK_URL"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.chdir(tmp_path)  # never read the developer's real .env
    cli.get_settings.cache_clear()


def test_every_command_has_help():
    names = [c.name or c.callback.__name__ for c in cli.app.registered_commands]
    assert {"trades", "flags", "advise", "news", "ask", "report", "notify", "web"} <= set(names)
    for name in names:
        res = runner.invoke(cli.app, [name, "--help"])
        assert res.exit_code == 0, (name, res.output)


def test_trades_command(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["trades"])
    assert res.exit_code == 0, res.output
    assert "Trade proposals" in res.output and "my_c2" in res.output and "opp_lw" in res.output
    assert "expected value" in res.output and "Accept" in res.output
    res = runner.invoke(cli.app, ["--json", "trades", "--limit", "5", "--per-team", "1"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.output)
    assert data["errors"] == [] and len(data["trades"]) == 1 and "sweet_spot" in data
    t = data["trades"][0]
    # my surplus C for their LW (a no-cost throw-in may ride along: it raises their acceptance)
    assert t["counterparty"] == "them" and "my_c2" in [p["cid"] for p in t["drop"]]
    assert [p["cid"] for p in t["add"]] == ["opp_lw"]
    codes = [x["code"] for x in t["reasons"]]
    assert codes[:2] == ["MY_EDGE", "MARKET_VIEW"] and "P_ACCEPT" in codes


def test_flags_command(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["flags"])
    assert res.exit_code == 0, res.output
    assert "Sell high" in res.output and "my_hot" in res.output
    data = json.loads(runner.invoke(cli.app, ["--json", "flags"]).output)
    assert [r["drop"][0]["cid"] for r in data["sell_high"]] == ["my_hot"]
    assert [r["add"][0]["cid"] for r in data["buy_low"]] == ["opp_cold"]


def test_advise_command_and_explain_hint(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["advise", "--explain"])
    assert res.exit_code == 0, res.output
    assert "Trades" in res.output and "Sell high" in res.output
    assert "OPENROUTER_API_KEY" in res.output
    data = json.loads(runner.invoke(cli.app, ["--json", "advise", "--limit", "3"]).output)
    assert len(data["recommendations"]) == 3 and data["errors"] == []
    assert data["recommendations"][0]["score"] == 10.0


def test_advise_explain_with_fake_llm(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)

    class FakeLLM:
        available, model = True, "fake/model"

        def complete(self, system, user, **kw):
            return '{"0": "Swap the benched center for a starting winger."}'

    monkeypatch.setattr(cli, "_llm_client", lambda: FakeLLM())
    data = json.loads(runner.invoke(cli.app, ["--json", "advise", "--explain"]).output)
    assert data["recommendations"][0]["narrative"] == "Swap the benched center for a starting winger."
    assert any("news unavailable" in e for e in data["errors"])  # offline: news skipped, not fatal


def test_recommender_failure_is_reported_not_fatal(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    from fantasy_manager.recommend import trades as trades_mod

    def boom(*a, **k):
        raise RuntimeError("solver exploded")

    monkeypatch.setattr(trades_mod, "recommend_trades", boom)
    res = runner.invoke(cli.app, ["--json", "trades"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.output)
    assert data["trades"] == [] and "solver exploded" in data["errors"][0]
    res = runner.invoke(cli.app, ["trades"])
    assert res.exit_code == 0 and "solver exploded" in res.output
    data = json.loads(runner.invoke(cli.app, ["--json", "advise"]).output)
    assert data["recommendations"] and any("solver exploded" in e for e in data["errors"])


def test_advise_surfaces_engine_errors_logged_by_advise(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    from fantasy_manager.recommend import flags as flags_mod

    def boom(*a, **k):
        raise ValueError("bad form data")

    monkeypatch.setattr(flags_mod, "recommend_flags", boom)
    res = runner.invoke(cli.app, ["--json", "advise"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.output)
    assert data["recommendations"] and any("bad form data" in e for e in data["errors"])


def test_news_offline_warns(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["news", "--all"])
    assert res.exit_code == 0, res.output
    assert "RotoWire news unavailable" in res.output
    data = json.loads(runner.invoke(cli.app, ["--json", "news"]).output)
    assert data["players"] == [] and any("ESPN news unavailable" in e for e in data["errors"])


def test_news_matches_players(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    from datetime import datetime, timezone

    from fantasy_manager.providers import news as news_mod
    from fantasy_manager.providers.news import NewsItem

    def fake_rotowire(fetch=None):
        return [NewsItem(source="rotowire", id="1", player_name="my_hot", headline="Scores twice",
                         blurb="Great night.", published=datetime.now(timezone.utc), tags=["line"]),
                NewsItem(source="rotowire", id="2", player_name="opp_c", headline="Other team news")]

    monkeypatch.setattr(news_mod, "fetch_rotowire", fake_rotowire)
    res = runner.invoke(cli.app, ["news"])
    assert res.exit_code == 0, res.output
    assert "my_hot" in res.output and "Scores twice" in res.output and "[line]" in res.output
    assert "Other team news" not in res.output          # --mine skips other rosters
    data = json.loads(runner.invoke(cli.app, ["--json", "news", "--all"]).output)
    assert {p["cid"] for p in data["players"]} == {"my_hot", "opp_c"}


def test_ask_without_key_fails_with_hint(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["ask", "should I trade my_c2?"])
    assert res.exit_code == 1
    assert "OPENROUTER_API_KEY" in res.output


def test_ask_with_fake_llm(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    seen = {}

    class FakeLLM:
        available, model = True, "fake/model"

        def complete(self, system, user, **kw):
            seen["user"] = user
            return "Yes, trade my_c2 for opp_lw. Confidence: medium."

    monkeypatch.setattr(cli, "_llm_client", lambda: FakeLLM())
    res = runner.invoke(cli.app, ["ask", "should I trade my_c2?"])
    assert res.exit_code == 0, res.output
    assert "Confidence: medium" in res.output and "should I trade my_c2?" in seen["user"]


def test_report_writes_files_and_notify(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    out = tmp_path / "out"
    res = runner.invoke(cli.app, ["report", "--out", str(out), "--explain"])
    assert res.exit_code == 0, res.output
    md = list(out.glob("digest-*.md"))
    html = list(out.glob("digest-*.html"))
    assert len(md) == 1 and len(html) == 1 and "my_c2" in md[0].read_text(encoding="utf-8")
    assert "OPENROUTER_API_KEY" in res.output

    res = runner.invoke(cli.app, ["--json", "report"])        # default dir: <FM_DATA_DIR>/reports
    assert res.exit_code == 0, res.output
    data = json.loads(res.output)
    assert (tmp_path / "reports").is_dir() and data["markdown"].endswith(".md") and data["summary"]

    from fantasy_manager.report import notify as notify_mod
    sent = []
    monkeypatch.setenv("DISCORD_WEBHOOK_URL", "https://discord.test/hook")
    cli.get_settings.cache_clear()
    monkeypatch.setattr(notify_mod, "send_discord",
                        lambda url, text, client=None: sent.append(text) or "discord: sent 1 message(s)")
    res = runner.invoke(cli.app, ["report", "--out", str(out), "--notify"])
    assert res.exit_code == 0, res.output
    assert "discord: sent 1 message(s)" in res.output and sent and "my_c2" in sent[0]


def test_notify_command(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["notify", "hello"])
    assert res.exit_code == 1 and "no webhooks configured" in res.output
    from fantasy_manager.report import notify as notify_mod
    monkeypatch.setenv("SLACK_WEBHOOK_URL", "https://slack.test/hook")
    cli.get_settings.cache_clear()
    monkeypatch.setattr(notify_mod, "send_slack",
                        lambda url, text, client=None: f"slack: sent 1 message(s) {text}")
    res = runner.invoke(cli.app, ["--json", "notify", "hello"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.output)["results"] == ["slack: sent 1 message(s) hello"]


def test_web_without_fastapi_prints_install_hint(monkeypatch):
    monkeypatch.setitem(sys.modules, "fastapi", None)   # import fastapi -> ImportError
    res = runner.invoke(cli.app, ["web", "--port", "9999"])
    assert res.exit_code == 1
    assert "pip install -e .[web]" in res.output


def test_web_calls_run(monkeypatch):
    import types

    calls = []
    fake = types.ModuleType("fantasy_manager.web.app")
    fake.run = lambda host, port, league: calls.append((host, port, league))
    pkg = types.ModuleType("fantasy_manager.web")
    pkg.app = fake
    monkeypatch.setitem(sys.modules, "fantasy_manager.web", pkg)
    monkeypatch.setitem(sys.modules, "fantasy_manager.web.app", fake)
    res = runner.invoke(cli.app, ["--league", "fantrax", "web", "--host", "0.0.0.0", "--port", "9000"])
    assert res.exit_code == 0, res.output
    assert calls == [("0.0.0.0", 9000, "fantrax")]


# -- categories / roto and dynasty ages ------------------------------------------

def _categories_ctx(kind="categories"):
    from fantasy_manager.models import ScoringConfig

    ctx = _two_team_ctx()
    ctx.scoring = ScoringConfig(kind=kind, categories=["G", "SOG"])
    return ctx


def test_categories_values_are_not_zero():
    from fantasy_manager.scoring import from_config
    from fantasy_manager.valuation.valuate import valuate_league

    for kind in ("categories", "roto"):
        ctx = _categories_ctx(kind)
        values = valuate_league(ctx, from_config(ctx.scoring))
        assert any(abs(v.fpg) > 0.1 for v in values.values())
        assert values["opp_lw"].fpg > values["my_lw"].fpg     # 3.0 G/GP beats 0.5 G/GP
        assert any(v.vorp != 0 for v in values.values())


def test_categories_league_commands(monkeypatch, tmp_path):
    _patch2(monkeypatch, tmp_path)
    monkeypatch.setattr(espn.EspnProvider, "load", lambda self: _categories_ctx())
    for args in (["roster"], ["waivers"], ["trades"], ["flags"], ["advise"]):
        res = runner.invoke(cli.app, args)
        assert res.exit_code == 0, (args, res.output)
    res = runner.invoke(cli.app, ["--json", "roster"])
    fpgs = [s["value"]["fpg"] for s in json.loads(res.output)[0]["slots"]]
    assert any(abs(x) > 0.1 for x in fpgs)
    assert "z-score" in runner.invoke(cli.app, ["roster"]).output


def test_dynasty_gets_provider_ages_and_trade_block_wants(monkeypatch, tmp_path):
    import types

    _patch2(monkeypatch, tmp_path)
    got = {}

    def fake_apply(values, ctx, ages=None):
        got.setdefault("ages", ages)     # first call is the CLI's; the trade engine may call again
        return {cid: 1.0 for cid in values}

    fake = types.ModuleType("fantasy_manager.valuation.dynasty")
    fake.apply_dynasty = fake_apply
    monkeypatch.setitem(sys.modules, "fantasy_manager.valuation.dynasty", fake)

    class Block:
        team_id, positions_wanted, players_wanted = "2", ["C"], ["my_hot"]

    class FakeFantrax:
        warnings: list = []
        ages = {"my_c": 23.5}
        trade_blocks = [Block()]

        def load(self):
            ctx = _two_team_ctx()
            ctx.provider, ctx.dynasty = "fantrax", True
            return ctx

    monkeypatch.setattr(cli, "get_provider", lambda name, settings, cache: FakeFantrax())
    res = runner.invoke(cli.app, ["--json", "--league", "fantrax", "trades"])
    assert res.exit_code == 0, res.output
    assert got["ages"] == {"my_c": 23.5}
    assert json.loads(res.output)["trade_block_wants"] == {"2": ["C", "D"]}


# -- dynasty mode (fm mode / --mode) ------------------------------------------------------

def _patch_mode(monkeypatch, tmp_path):
    """Fake dynasty Fantrax league; no real .env (chdir) and FANTRAX_MODE unset."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FANTRAX_MODE", raising=False)
    _patch(monkeypatch, tmp_path)
    seen: list[str] = []

    class FakeFantrax:
        warnings: list[str] = []

        def load(self):
            ctx = _fake_ctx()
            ctx.provider, ctx.dynasty = "fantrax", True
            return ctx

    real = cli._dynasty

    def spy(lc, values, provider=None):
        seen.append(lc.dynasty_mode)
        return real(lc, values, provider)

    monkeypatch.setattr(cli, "get_provider", lambda name, settings, cache: FakeFantrax())
    monkeypatch.setattr(cli, "_dynasty", spy)
    return seen


def test_mode_command_get_set_clear(monkeypatch, tmp_path):
    from fantasy_manager import prefs

    _patch_mode(monkeypatch, tmp_path)
    res = runner.invoke(cli.app, ["mode"])
    assert res.exit_code == 0, res.output
    assert "Dynasty mode: balanced (from default)" in res.output
    res = runner.invoke(cli.app, ["mode", "rebuild"])
    assert res.exit_code == 0 and "Dynasty mode set to rebuild" in res.output
    assert prefs.get_pref("dynasty_mode", data_dir=tmp_path) == "rebuild"
    data = json.loads(runner.invoke(cli.app, ["--json", "mode"]).output)
    assert (data["mode"], data["source"], data["source_label"]) == ("rebuild", "prefs", "dashboard/prefs")
    assert data["changed"] is False
    monkeypatch.setenv("FANTRAX_MODE", "contend")      # a saved mode beats FANTRAX_MODE
    cli.get_settings.cache_clear()
    assert "rebuild (from dashboard/prefs)" in runner.invoke(cli.app, ["mode"]).output
    assert runner.invoke(cli.app, ["mode", "tank"]).exit_code == 2
    assert runner.invoke(cli.app, ["mode", "contend", "--clear"]).exit_code == 1
    res = runner.invoke(cli.app, ["mode", "--clear"])
    assert res.exit_code == 0 and "now contend (from FANTRAX_MODE)" in res.output
    assert prefs.get_pref("dynasty_mode", data_dir=tmp_path) is None


def test_saved_mode_reaches_commands_and_mode_option_does_not_persist(monkeypatch, tmp_path):
    from fantasy_manager import prefs

    seen = _patch_mode(monkeypatch, tmp_path)
    runner.invoke(cli.app, ["mode", "rebuild"])
    res = runner.invoke(cli.app, ["--league", "fantrax", "roster"])
    assert res.exit_code == 0, res.output
    assert seen == ["rebuild"] and "Dynasty mode: rebuild (from dashboard/prefs)" in res.output
    for cmd in (["roster"], ["waivers"], ["trades"], ["advise"], ["report", "--no-record"]):
        res = runner.invoke(cli.app, ["--league", "fantrax", *cmd, "--mode", "contend"])
        assert res.exit_code == 0, (cmd, res.output)
        assert seen[-1] == "contend", cmd
        assert "Dynasty mode: contend (from --mode, this run only)" in res.output, cmd
    assert prefs.get_pref("dynasty_mode", data_dir=tmp_path) == "rebuild"   # --mode never saved
    runner.invoke(cli.app, ["--league", "fantrax", "waivers"])
    assert seen[-1] == "rebuild"
    data = json.loads(runner.invoke(cli.app, ["--json", "--league", "fantrax", "settings"]).output)
    assert (data["dynasty_mode"], data["dynasty_mode_source"]) == ("rebuild", "prefs")
    assert runner.invoke(cli.app, ["--league", "fantrax", "roster", "--mode", "tank"]).exit_code == 2
