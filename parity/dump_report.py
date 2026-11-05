"""Python goldens for W9-report-pregame: digest md/html/summary, webhook payloads and the pregame
state/diff/summary, all from the parity snapshot with a pinned clock. Nothing is posted anywhere:
webhook calls go through httpx.MockTransport.

Usage: python dump_report.py <legacy-python checkout> <fixtures_dir> <out_dir>
Writes <out_dir>/report.json:
  {"<league>": {"digests": {name: {...}}, "pregame": {...}}, "notify": {...}}
"""
import copy
import datetime as dt
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

wt, fixtures, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
sys.path.insert(0, str(wt))
sys.path.insert(0, str(Path(__file__).resolve().parent))
tmp = Path(tempfile.mkdtemp())
shutil.copytree(fixtures / "_datadir", tmp / "data", ignore=shutil.ignore_patterns("fantrax_cache", "harness.db"))
os.environ["FM_DATA_DIR"] = str(tmp / "data")
os.environ.setdefault("FM_NEWS_ROLES_LLM", "0")

import dump_fixtures as dfx  # noqa: E402
import httpx  # noqa: E402

from fantasy_manager import pregame  # noqa: E402
from fantasy_manager.models import LeagueContext, Recommendation, SourceStatus  # noqa: E402
from fantasy_manager.providers.dailyfaceoff import GoalieStart  # noqa: E402
from fantasy_manager.providers.news import NewsItem  # noqa: E402
from fantasy_manager.report import notify  # noqa: E402
from fantasy_manager.report.digest import build_digest  # noqa: E402
from fantasy_manager.valuation.valuate import PlayerValue  # noqa: E402
from fantasy_manager.valuation.rookie import RookieEstimate  # noqa: E402
from types import SimpleNamespace  # noqa: E402

out.mkdir(parents=True, exist_ok=True)
result: dict = {}


def load(league):
    snap = json.loads((fixtures / league / "snapshot.json").read_text(encoding="utf-8"))
    dfx.freeze_clock(dt.datetime.fromisoformat(snap["now"]))
    for k, v in (snap.get("env") or {}).items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    ctx = LeagueContext.model_validate(snap["ctx"])
    players = {p.cid: p for p in ctx.all_players()}
    vals = {}
    for cid, v in json.loads((fixtures / league / "values.json").read_text(encoding="utf-8")).items():
        v = dict(v)
        v.pop("cid", None)
        v["rookie"] = RookieEstimate.model_validate(v["rookie"]) if v.get("rookie") else None
        vals[cid] = PlayerValue(player=players[cid], **v)
    recs = [Recommendation.model_validate(r) for r in
            json.loads((fixtures / league / "engines" / "advise_merged.json").read_text(encoding="utf-8"))]
    alerts = [Recommendation.model_validate(r) for r in
              json.loads((fixtures / league / "engines" / "line_alerts.json").read_text(encoding="utf-8"))]
    return snap, ctx, vals, recs, alerts


def digest_json(d):
    return {"markdown": d.markdown, "html": d.html, "summary": d.summary, "alert": d.alert,
            "overall": d.health.overall if d.health else None, "league": d.league, "day": d.day.isoformat()}


for league in ("espn", "fantrax"):
    snap, ctx, vals, recs, alerts = load(league)
    gen = dt.datetime.fromisoformat(snap["local_now"])
    mine = ctx.my_team.players
    news = {p.cid: [NewsItem(source="rotowire", player_name=p.name, headline=f"{p.name} update {i}",
                             blurb="Skated in full and is expected back.", url=f"https://example.com/{i}",
                             published=dt.datetime(2026, 11, 3, 12, 0, tzinfo=dt.timezone.utc))]
            for i, p in enumerate(mine[:4])}
    entry: dict = {"digests": {}}
    entry["digests"]["plain"] = digest_json(build_digest(ctx, vals, recs, news, gen))
    plan = ["This week vs Rival: win 66% -> 71% (moves left 2)", "Quick lineup each day; also do:", "Today: add X"]
    entry["digests"]["full"] = digest_json(build_digest(
        ctx, vals, recs, news, gen, model_headline="forwards beat season-to-date by 12%",
        model_health=["Projection MAE: 0.55", "Hit rate: 60%", "Params: packaged (hash abc)"], week_plan=plan))
    bad = copy.deepcopy(ctx)
    bad.sources = list(bad.sources) + [
        SourceStatus(name="Fantrax", ok=False, severity="fail", feeds_valuation=True,
                     detail="login expired - refresh FANTRAX_COOKIE"),
        SourceStatus(name="Odds", ok=False, severity="fail", feeds_valuation=False, detail="HTTP 500 https://x.com/a?b=1")]
    entry["digests"]["failing"] = digest_json(build_digest(bad, vals, recs, news, gen))
    entry["digests"]["empty"] = digest_json(build_digest(ctx, vals, [], {}, gen))
    long = [r.model_copy(update={"narrative": "x " * 400}) for r in recs[:10]] * 20
    entry["digests"]["long"] = digest_json(build_digest(ctx, vals, long, news, gen))

    # ---- pregame: state / diff / summary on the snapshot
    now = gen
    goalies = [p for p in mine if p.is_goalie]
    fa_goalies = [p for p in ctx.free_agents if p.is_goalie][:6]

    def starts(strength):
        out_ = []
        for i, p in enumerate(goalies + fa_goalies):
            team = pregame.normalize_team(p.team) or "NYR"
            out_.append(GoalieStart(game=f"OPP{i}@{team}", team=team, opponent=f"OPP{i}", home=True,
                                    goalie_name=p.name, strength=strength if i % 2 == 0 else "Likely"))
        return out_

    morning = pregame.build_state(league, ctx, vals, starts("Likely"), alerts=alerts[:3], source="daily", now=now)
    ctx2 = copy.deepcopy(ctx)
    changed = [p for p in ctx2.my_team.players if not p.is_goalie][:2]
    changed[0].status = "out"
    if len(changed) > 1:
        changed[1].line = "f4"
        changed[1].pp_unit = "pp2"
    cur = pregame.build_state(league, ctx2, vals, starts("Confirmed"), alerts=alerts[:8], source="pregame",
                              now=now + dt.timedelta(hours=9))
    changes = pregame.diff_states(morning, cur)
    lineup = pregame.today_lineup(ctx2, vals)
    entry["pregame"] = {
        "morning": morning, "state": cur, "lineup": lineup,
        "changes": [c.model_dump() for c in changes],
        "summary": pregame.summarize(changes, lineup, now + dt.timedelta(hours=9)),
        "summary_nolineup": pregame.summarize(changes, None, now + dt.timedelta(hours=9)),
        "summary_none": pregame.summarize([], lineup, now),
        "snapshot_json": json.dumps(cur, indent=1, sort_keys=True, default=str),
        "goalies": pregame.goalies_state(starts("Confirmed")),
    }
    result[league] = entry

# ---- notify
DISCORD, SLACK = "https://discord.com/api/webhooks/1/SECRET", "https://hooks.slack.com/services/T/B/SECRET"
text_cases = {
    "multi": "\n".join(f"line {i} " + "x" * 90 for i in range(60)),
    "longline": "word " * 1000,
    "blank": "   ",
    "unicode": "\n".join("éè " * 40 for _ in range(80)),
    "crlf": "a\r\nb\rc\x0bd e",
    "nospace": "y" * 5000,
}
notify_out: dict = {"chunks": {k: [notify.chunk_text(v, s) for s in (1900, 1500, 100)] for k, v in text_cases.items()},
                    "text_cases": text_cases, "scenarios": [], "format": {}}
lines = ["  a   b  ", "", "   ", "second\nline", "x" * 5]
for style in ("discord", "slack"):
    notify_out["format"][style] = [notify.format_changes("  Title ", lines, style), notify.format_changes("", lines, style)]

SCEN = [
    ("ok", [{"status": 204}]),
    ("ok_multi", [{"status": 204}] * 4),
    ("http404", [{"status": 404, "text": '{"message":  "Unknown\\nWebhook"}'}]),
    ("timeout", [{"error": "timeout"}]),
    ("connect", [{"error": "connect"}]),
    ("retry_ok", [{"status": 429, "retry_after": "0"}, {"status": 204}]),
    ("retry_too_long", [{"status": 429, "retry_after": "10"}]),
    ("late_error", [{"status": 204}, {"status": 500, "text": "boom"}]),
]


def run_scenario(fn, responses, *args):
    calls, it = [], iter(responses)

    def handler(request):
        calls.append([str(request.url), json.loads(request.content)])
        r = next(it)
        if r.get("error") == "timeout":
            raise httpx.ReadTimeout("slow", request=request)
        if r.get("error") == "connect":
            raise httpx.ConnectError("no route", request=request)
        hdr = {"retry-after": r["retry_after"]} if r.get("retry_after") is not None else {}
        return httpx.Response(r["status"], text=r.get("text", ""), headers=hdr)

    res = fn(*args, client=httpx.Client(transport=httpx.MockTransport(handler)))
    return {"result": res, "calls": calls}


big = text_cases["multi"]
for name, resp in SCEN:
    s = {"name": name, "responses": resp}
    s["discord"] = run_scenario(notify.send_discord, resp, DISCORD, big if name in ("ok_multi", "late_error") else "hi")
    s["slack"] = run_scenario(notify.send_slack, resp, SLACK, "hello")
    s["changes"] = run_scenario(notify.notify_changes, resp * 1, SimpleNamespace(
        discord_webhook_url=DISCORD, slack_webhook_url=None), "Pre-game 5:02 PM - ESPN", ["a", "b"])
    notify_out["scenarios"].append(s)
notify_out["none"] = [notify.notify_all(SimpleNamespace(discord_webhook_url=None, slack_webhook_url=None), "x"),
                      notify.send_discord(None, "x")]
result["notify"] = notify_out
(out / "report.json").write_text(json.dumps(result, ensure_ascii=False, default=str), encoding="utf-8")
print("wrote", out / "report.json")
