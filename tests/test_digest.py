import re
from datetime import date, datetime, timezone
from types import SimpleNamespace

from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot,
                                    ScoringConfig, normalize_name)
from fantasy_manager.providers.news import NewsItem
from fantasy_manager.report.digest import SUMMARY_MAX, build_digest, write_digest


def P(cid, name, team="OTT", pos=("C",), status="healthy", note=None):
    return Player(cid=cid, name=name, name_norm=normalize_name(name), ids={}, team=team,
                  positions=list(pos), status=status, status_note=note)


def make_league():
    mine = [P("a", "Tim Stützle"), P("b", "Brady Tkachuk", pos=("LW",)),
            P("c", "Jake Sanderson", pos=("D",), status="ir", note="Lower body"),
            P("g", "Linus Ullmark", pos=("G",))]
    other = [P("x", "Connor McDavid", "EDM"), P("y", "Cale Makar", "COL", ("D",))]
    fas = [P("f1", "Free Agent One", "SJS", ("RW",)), P("f2", "Free Agent Two", "ANA", ("D",))]
    ctx = LeagueContext(
        provider="espn", league_id="123", season=2027, name="Test League",
        scoring=ScoringConfig(kind="points", weights={"G": 3, "A": 2, "SOG": 0.4}),
        roster_shape={"C": 2, "LW": 2, "D": 4, "G": 2, "BN": 4},
        teams=[FantasyTeam(team_id="1", name="My Team", owner_is_me=True, record=(3, 1, 0),
                           slots=[RosterSlot(slot=s, player=p, starting=s != "IR")
                                  for s, p in zip(["C", "LW", "IR", "G"], mine)]),
               FantasyTeam(team_id="2", name="Rival", owner_is_me=False,
                           slots=[RosterSlot(slot="C", player=other[0], starting=True),
                                  RosterSlot(slot="D", player=other[1], starting=True)])],
        free_agents=fas, matchup_period=1, as_of=date(2026, 10, 7))
    values = {p.cid: SimpleNamespace(fpg=2.0 + i * 0.1, fpg_week=1.5, vorp=0.5 - i * 0.1)
              for i, p in enumerate(ctx.all_players())}
    return ctx, values


def make_recs(ctx):
    by = {p.cid: p for p in ctx.all_players()}
    return [
        Recommendation(kind="waiver", score=2.5, title="Add Free Agent One, drop Jake Sanderson",
                       add=[by["f1"]], drop=[by["c"]],
                       reasons=[Reason(code="VORP", text="+0.80 FPG over RW replacement", value=0.8)],
                       narrative="A clear upgrade."),
        Recommendation(kind="trade", score=1.4, title="Trade Brady Tkachuk for Cale Makar", counterparty="Rival",
                       add=[by["y"]], drop=[by["b"]], reasons=[Reason(code="GAIN", text="Lineup gain 0.9 FPG")]),
        Recommendation(kind="lineup", score=0.9, title="Start Linus Ullmark", add=[by["g"]],
                       reasons=[Reason(code="GAMES", text="4 games this week")]),
        Recommendation(kind="sell_high", score=0.7, title="Sell high on Tim Stützle", drop=[by["a"]],
                       reasons=[Reason(code="FORM", text="L15 form 1.45x season")]),
        Recommendation(kind="injury", score=3.0, title="Move Jake Sanderson to IR", drop=[by["c"]],
                       reasons=[Reason(code="STATUS", text="Out (lower body)")]),
    ]


NEWS = {"a": [NewsItem(source="rotowire", player_name="Tim Stutzle", headline="Scores twice",
                       blurb="Stutzle had two goals.", url="https://www.rotowire.com/hockey/x",
                       published=datetime(2026, 10, 6, tzinfo=timezone.utc))]}
GEN = datetime(2026, 10, 7, 8, 30)


def test_markdown_has_every_section_and_rec_details():
    ctx, values = make_league()
    d = build_digest(ctx, values, make_recs(ctx), NEWS, GEN)
    md = d.markdown
    for section in ("## Headline", "## Injury alerts", "## Lineup", "## Waivers", "## Trades", "## Flags",
                    "## News for my players"):
        assert section in md
    headline = md.split("## Headline")[1].split("## Injury alerts")[0]
    assert headline.index("Move Jake Sanderson to IR") < headline.index("Add Free Agent One")
    assert "Start Linus Ullmark" not in headline  # only top 3
    assert "score 2.50" in md and "- +0.80 FPG over RW replacement" in md
    assert "> A clear upgrade." in md
    assert "(with Rival)" in md
    assert "Sell high on Tim Stützle" in md.split("## Flags")[1]
    assert "Scores twice" in md.split("## News for my players")[1]


def test_injury_alert_for_status_without_rec():
    ctx, values = make_league()
    recs = [r for r in make_recs(ctx) if r.kind != "injury"]
    md = build_digest(ctx, values, recs, {}, GEN).markdown
    alerts = md.split("## Injury alerts")[1].split("## Lineup")[0]
    assert "Jake Sanderson" in alerts and "IR: Lower body" in alerts


def test_empty_sections_render_placeholders():
    ctx, values = make_league()
    d = build_digest(ctx, values, [], {}, GEN)
    assert "No moves recommended today." in d.markdown
    assert d.markdown.count("_None today._") == 5          # lineup, waivers, trades, flags, alerts
    assert "No recent news" in d.html


def test_html_self_contained_mobile_dark_mode_and_escaped():
    ctx, values = make_league()
    recs = make_recs(ctx)
    recs[0].narrative = "<script>alert(1)</script> upgrade"
    d = build_digest(ctx, values, recs, NEWS, GEN)
    h = d.html
    assert not re.search(r"https?://", h)
    assert "<script" not in h and "&lt;script&gt;" in h
    assert "max-width:720px" in h and "prefers-color-scheme: dark" in h
    assert 'name="viewport"' in h and "<link" not in h
    for section in ("Headline", "Injury alerts", "Lineup", "Waivers", "Trades", "Flags", "News for my players"):
        assert f"<h2>{section}</h2>" in h


def test_summary_is_short_plain_text():
    ctx, values = make_league()
    recs = make_recs(ctx) * 50
    for r in recs:
        r.narrative = "x " * 400
    d = build_digest(ctx, values, recs, NEWS, GEN)
    assert len(d.summary) <= SUMMARY_MAX
    assert d.summary.startswith("Fantasy digest: Test League (2026-10-07)")
    short = build_digest(ctx, values, make_recs(ctx), NEWS, GEN).summary
    assert "Injury alerts" in short and "Counts: Lineup 1 | Waivers 1 | Trades 1 | Flags 1" in short
    assert "#" not in short and "<" not in short


def test_write_digest(tmp_path):
    ctx, values = make_league()
    d = build_digest(ctx, values, make_recs(ctx), NEWS, GEN)
    md_path, html_path = write_digest(d, tmp_path / "reports")
    assert md_path.name == "digest-2026-10-07.md" and html_path.name == "digest-2026-10-07.html"
    assert md_path.read_text(encoding="utf-8") == d.markdown
    assert html_path.read_text(encoding="utf-8").startswith("<!doctype html>")


def test_model_headline_line_hidden_until_given(tmp_path):
    from fantasy_manager.report.digest import model_headline

    ctx, values = make_league()
    plain = build_digest(ctx, values, make_recs(ctx), NEWS, GEN)
    assert "Model:" not in plain.summary and "Model:" not in plain.markdown and "Model:" not in plain.html
    line = "Model: forwards projections beat season-to-date by 12% (provisional, n=180)"
    d = build_digest(ctx, values, make_recs(ctx), NEWS, GEN, model_headline=line)
    assert d.summary.splitlines()[1] == line
    assert f"*{line}*" in d.markdown and 'class="meta model">Model: forwards' in d.html
    assert model_headline(tmp_path, "espn") is None          # no ledger: hidden, and none is created
    assert not (tmp_path / "harness.db").exists()



def test_model_health_block_hidden_until_trustworthy(tmp_path):
    from fantasy_manager.harness.ledger import Ledger
    from fantasy_manager.report.digest import model_health

    ctx, values = make_league()
    plain = build_digest(ctx, values, make_recs(ctx), NEWS, GEN)
    assert "Model health" not in plain.markdown and "Model health" not in plain.html
    assert model_health(tmp_path, "espn") is None and not (tmp_path / "harness.db").exists()
    with Ledger(tmp_path) as led:                                   # graded, but nothing trustworthy
        led.upsert("metric_snapshots", [{"snapshot_id": "a", "week": "2026-11-02", "league": "espn",
                                         "metric": "proj_fpg_mae", "pool": "F", "value": 0.5, "n": 20,
                                         "trust": "hidden", "detail_json": "{}"}], ("snapshot_id",))
    assert model_health(tmp_path, "espn") is None
    with Ledger(tmp_path) as led:
        rows = []
        for week, mae, n in (("2026-11-02", 0.60, 160), ("2026-11-09", 0.55, 200)):
            rows += [{"snapshot_id": f"m{week}", "week": week, "league": "espn", "metric": "proj_fpg_mae",
                      "pool": "F", "value": mae, "n": n, "trust": "provisional", "detail_json": "{}"},
                     {"snapshot_id": f"s{week}", "week": week, "league": "espn", "metric": "proj_fpg_skill",
                      "pool": "F", "value": 0.1, "n": n, "ci_lo": 0.02, "ci_hi": 0.18, "trust": "provisional",
                      "detail_json": "{}"},
                     {"snapshot_id": f"h{week}", "week": week, "league": "espn", "metric": "hit_rate",
                      "pool": "waiver:followed", "value": 0.6, "n": 25, "ci_lo": 0.4, "ci_hi": 0.77,
                      "trust": "provisional", "detail_json": '{"hits": 15}'}]
        led.upsert("metric_snapshots", rows, ("snapshot_id",))
    block = model_health(tmp_path, "espn")
    assert len(block) == 3
    assert block[0] == "Projection MAE (FPG, next 28 days): forwards 0.55 \u2193 (was 0.60)"
    assert block[1] == "Hit rate of the model's recs: waiver 60% (n=25, provisional)"
    assert block[2].startswith("Params: packaged (hash ")
    d = build_digest(ctx, values, make_recs(ctx), NEWS, GEN, model_headline="Model: x", model_health=block)
    assert "## Model health\n\n- Projection MAE" in d.markdown and "- Params: packaged" in d.markdown
    assert '<h2>Model health</h2>\n<ul class="model-health"><li>Projection MAE' in d.html
    assert "Model health" not in d.summary                          # webhook summary stays short
