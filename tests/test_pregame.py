"""Pre-game afternoon run (fantasy_manager.pregame, `fm harness pregame`) and the league-failure
notification of `fm harness daily`. Everything is faked: no network, no real data dir."""
from __future__ import annotations

import json
from datetime import date, datetime
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fantasy_manager import cli_backtest, cli_harness, pregame
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig
from fantasy_manager.providers.base import ProviderError
from fantasy_manager.providers.dailyfaceoff import GoalieStart
from fantasy_manager.report import notify as notify_mod
from fantasy_manager.valuation.valuate import PlayerValue

DAY = date(2026, 9, 29)
MORNING = datetime(2026, 9, 29, 8, 5)
AFTERNOON = datetime(2026, 9, 29, 17, 2)
GAMES = [("FLA", "CAR"), ("MTL", "TOR"), ("NYR", "BOS"), ("VAN", "EDM"), ("CHI", "VGK")]


# ------------------------------------------------------------------------------ fakes

def P(cid, name, pos, team, **kw) -> Player:
    return Player(cid=f"espn:{cid}", name=name, name_norm=name.lower(), ids={}, team=team, positions=pos, **kw)


def V(p: Player, fpg: float, share: float | None = None) -> PlayerValue:
    return PlayerValue(player=p, fpg=fpg, fpg_season=fpg, fpg_week=fpg, vorp=0.0, proj_week=fpg * 3,
                       games_next7=3, start_share=share)


def make_league(*, jarvis_status="healthy", suzuki_status="dtd", jarvis_line="f1", jarvis_pp="pp1",
                bussi_start=None, fa_change=None, as_of=DAY):
    """My team: Jarvis (C, CAR) starting, Suzuki (C, MTL) bench, Bussi (G, CAR) starting,
    Stolarz (G, TOR) bench. Free agents: Soderblom (G, CHI), Nazar (C, CHI)."""
    jarvis = P(1, "Seth Jarvis", ["C"], "CAR", status=jarvis_status, line=jarvis_line, pp_unit=jarvis_pp)
    suzuki = P(2, "Nick Suzuki", ["C"], "MTL", status=suzuki_status, line="f1", pp_unit="pp1")
    bussi = P(3, "Brandon Bussi", ["G"], "CAR", line="g", confirmed_start=bussi_start,
              start_source=None if bussi_start is None else "DFO Confirmed: x starts FLA@CAR (src, time)")
    stolarz = P(4, "Anthony Stolarz", ["G"], "TOR", line="g")
    sod = P(5, "Arvid Soderblom", ["G"], "CHI")
    nazar = P(6, "Frank Nazar", ["C"], "CHI", line="f2", pp_unit="pp1" if fa_change else "pp2", line_change=fa_change)
    me = FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=[
        RosterSlot(slot="C", player=jarvis, starting=True), RosterSlot(slot="BN", player=suzuki, starting=False),
        RosterSlot(slot="G", player=bussi, starting=True), RosterSlot(slot="BN", player=stolarz, starting=False)])
    teams = {t for g in GAMES for t in g}
    ctx = LeagueContext(provider="espn", league_id="1", season=2027, name="L",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}),
                        roster_shape={"C": 1, "G": 1, "BN": 4}, teams=[me], free_agents=[sod, nazar],
                        matchup_period=1, as_of=as_of, schedule={t: [as_of] for t in teams})
    values = {jarvis.cid: V(jarvis, 3.0), suzuki.cid: V(suzuki, 2.5), bussi.cid: V(bussi, 4.0, 0.6),
              stolarz.cid: V(stolarz, 3.5, 0.5), sod.cid: V(sod, 2.0, 0.4), nazar.cid: V(nazar, 1.5)}
    return ctx, values


def starts(**strengths) -> list[GoalieStart]:
    """Tonight's goalie page. kwargs: TEAM=(goalie, strength)."""
    names = {"CAR": ("Brandon Bussi", "Likely"), "FLA": ("Sergei Bobrovsky", "Likely"),
             "TOR": ("Anthony Stolarz", "Likely"), "MTL": ("Sam Montembeault", "Likely"),
             "CHI": ("Arvid Soderblom", "Likely"), "VGK": ("Adin Hill", "Likely")}
    names.update(strengths)
    out = []
    for away, home in GAMES:
        for team, opp, is_home in ((away, home, False), (home, away, True)):
            if team in names:
                g, s = names[team]
                out.append(GoalieStart(game=f"{away}@{home}", team=team, opponent=opp, home=is_home,
                                       goalie_name=g, strength=s))
    return out


class FakeDfo:
    def __init__(self, goalie_list):
        self.goalie_list = goalie_list
        self.warnings: list[str] = []
        self.calls: list[str] = []

    def starting_goalies(self, day):
        self.calls.append("goalies")
        return list(self.goalie_list)

    def all_lines(self, teams):
        self.calls.append("lines")
        return {}


def no_injuries(url, params=None):
    return {"injuries": []}


def settings(tmp_path, **kw):
    return SimpleNamespace(fm_data_dir=tmp_path, fm_offline=True, **kw)


def run(tmp_path, league_kw, goalie_list, now=AFTERNOON, **kw):
    ctx, values = make_league(**league_kw)
    return pregame.run_pregame("espn", settings(tmp_path), None, as_of=DAY, now=now,
                               loader=lambda lg, s, c: (ctx, values, []), dfo_client=FakeDfo(goalie_list),
                               injuries_fetch=no_injuries, **kw)


def morning(tmp_path, league_kw=None, goalie_list=None):
    """The morning baseline, as `fm harness daily` would leave it (source "daily")."""
    ctx, values = make_league(**(league_kw or {}))
    state = pregame.build_state("espn", ctx, values, goalie_list or starts(), source="daily", now=MORNING)
    return pregame.save_snapshot(tmp_path, "espn", DAY, state)


def kinds(res):
    return [(c.kind, c.scope) for c in res.changes]


# ------------------------------------------------------------------------------ diff per change type

def test_goalie_confirmed_mine(tmp_path):
    morning(tmp_path)
    res = run(tmp_path, {"bussi_start": True}, starts(CAR=("Brandon Bussi", "Confirmed")))
    assert res.baseline == "daily" and not res.nothing_changed
    mine = [c for c in res.changes if c.kind == "goalie_confirmed" and c.scope == "mine"]
    assert [c.text for c in mine] == ["Bussi CONFIRMED (CAR vs FLA)"]
    assert res.summary_text.startswith("Pre-game 5:02 PM: Bussi CONFIRMED (CAR vs FLA).")
    assert res.summary_text.endswith("No lineup changes needed.")


def test_goalie_mine_sits_and_lineup_change(tmp_path):
    morning(tmp_path)
    res = run(tmp_path, {"bussi_start": False}, starts(CAR=("Frederik Andersen", "Confirmed")))
    texts = [c.text for c in res.changes]
    assert "Bussi sits: Andersen CONFIRMED for CAR vs FLA" in texts
    lc = [c.text for c in res.changes if c.kind == "lineup_change"]
    assert lc == ["Start Stolarz (sat this morning)", "Sit Bussi (was starting this morning)"]
    assert res.lineup_recs["today"]["to_start"] == ["Anthony Stolarz"]
    assert res.summary_text.endswith("Lineup: start Stolarz; bench Bussi.")


def test_goalie_confirmed_opponent_and_streamer(tmp_path):
    morning(tmp_path)
    res = run(tmp_path, {}, starts(FLA=("Sergei Bobrovsky", "Confirmed"), CHI=("Arvid Soderblom", "Confirmed"),
                                   VGK=("Adin Hill", "Confirmed")))
    by_scope = {c.scope: c.text for c in res.changes if c.kind == "goalie_confirmed"}
    assert by_scope["opponent"] == "Your CAR skaters face Bobrovsky, CONFIRMED for FLA"
    assert by_scope["streamer"] == "Streamer: Soderblom (free agent) CONFIRMED (CHI vs VGK)"
    assert len(by_scope) == 2          # Adin Hill: not mine, not facing my skaters, not a free agent


def test_status_changes_and_injury_new(tmp_path):
    morning(tmp_path)
    res = run(tmp_path, {"jarvis_status": "out", "suzuki_status": "healthy"}, starts())
    got = {c.kind: c.text for c in res.changes if c.kind in ("injury_new", "my_player_status_change")}
    assert got["injury_new"] == "Jarvis now OUT: bench him"
    assert got["my_player_status_change"] == "Suzuki cleared: DTD -> healthy"
    lc = [c.text for c in res.changes if c.kind == "lineup_change"]
    assert "Start Suzuki (sat this morning)" in lc and "Sit Jarvis (was starting this morning)" in lc
    # most urgent first: statuses before lineup changes
    assert [c.kind for c in res.changes].index("injury_new") < [c.kind for c in res.changes].index("lineup_change")


def test_new_alerts_line_move_and_fa_pp1(tmp_path):
    morning(tmp_path)
    res = run(tmp_path, {"jarvis_line": "f2", "jarvis_pp": "pp2", "fa_change": "PP2 -> PP1"}, starts())
    alerts = [(c.scope, c.text) for c in res.changes if c.kind == "new_alert"]
    assert ("mine", "Jarvis moved to F2 (was F1), PP1 -> PP2") in alerts
    fa = [t for s, t in alerts if s == "fa"]
    assert len(fa) == 1 and fa[0].startswith("Frank Nazar (free agent) promoted to PP1")


def test_nothing_changed_and_second_run_diffs_against_first(tmp_path):
    morning(tmp_path)
    first = run(tmp_path, {"bussi_start": True}, starts(CAR=("Brandon Bussi", "Confirmed")))
    assert not first.nothing_changed
    second = run(tmp_path, {"bussi_start": True}, starts(CAR=("Brandon Bussi", "Confirmed")),
                 now=datetime(2026, 9, 29, 17, 45))
    assert second.baseline == "pregame" and second.baseline_at.startswith("2026-09-29T17:02")
    assert second.nothing_changed and second.changes == []
    assert second.summary_text == "Pre-game: no changes. Lineup set." == pregame.NO_CHANGES


def test_no_baseline_uses_status_history_and_reports_confirmations(tmp_path):
    from fantasy_manager.recommend.injuries import StatusHistory

    ctx, _ = make_league()
    hist = StatusHistory(tmp_path)
    hist.record(ctx.my_team.players)            # Jarvis healthy, Suzuki dtd yesterday
    hist.close()
    res = run(tmp_path, {"jarvis_status": "dtd"}, starts(CAR=("Brandon Bussi", "Confirmed")))
    assert res.baseline is None
    assert ("injury_new", None) in kinds(res) and ("goalie_confirmed", "mine") in kinds(res)
    assert not any(c.kind == "lineup_change" for c in res.changes)     # nothing to diff a lineup against


# ------------------------------------------------------------------------------ snapshots

def test_snapshot_write_read_and_daily_baseline(tmp_path):
    from fantasy_manager.providers.lines_enrich import LineSnapshotStore

    ctx, values = make_league()
    LineSnapshotStore(tmp_path).save(DAY, {"date": DAY.isoformat(), "teams": {}, "players": {
        "1": {"name": "Brandon Bussi", "team": "CAR", "goalie_depth": 1}},
        "starts": [s.model_dump(mode="json") for s in starts()]})
    path = pregame.write_daily_snapshot("espn", settings(tmp_path), ctx, values, now=MORNING)
    assert path == tmp_path / "pregame" / f"espn-{DAY.isoformat()}.json"
    snap = pregame.load_snapshot(tmp_path, "espn", DAY)
    assert snap["source"] == "daily" and snap["version"] == pregame.SNAPSHOT_VERSION
    assert snap["goalies"]["CAR"] == {"goalie": "Brandon Bussi", "strength": "Likely", "game": "FLA@CAR",
                                      "opponent": "FLA", "home": True, "depth": 1}
    assert snap["lineup_today"]["start"] == ["espn:1", "espn:3"] and snap["lineup_today"]["set"] is True
    assert snap["my_players"]["espn:2"]["status"] == "dtd"
    assert [g["name"] for g in snap["fa_goalies"]] == ["Arvid Soderblom"]
    json.dumps(snap)                                               # compact and JSON-clean
    # a pre-game run reads it as its baseline and overwrites it with its own state
    res = run(tmp_path, {}, starts())
    assert res.baseline == "daily" and res.nothing_changed
    assert pregame.load_snapshot(tmp_path, "espn", DAY)["source"] == "pregame"
    # an unreadable / old-version file is ignored
    (tmp_path / "pregame" / f"espn-{DAY.isoformat()}.json").write_text('{"version": 0}', encoding="utf-8")
    assert pregame.load_snapshot(tmp_path, "espn", DAY) is None


def test_refresh_bypasses_cache_for_goalies_and_injuries():
    class Cache:
        def __init__(self):
            self.text, self.json = [], []

        def get_text(self, url, params=None, headers=None, ttl=0):
            self.text.append((url, ttl))
            if "/starting-goalies/" in url:
                raise RuntimeError("offline")
            raise RuntimeError("offline")

        def get_json(self, url, params=None, ttl=0):
            self.json.append((url, ttl))
            return {"injuries": []}

    c = Cache()
    got, info = pregame.refresh_sources(c, DAY)
    assert got == [] and c.text[0][1] == 0 and "/starting-goalies/2026-09-29" in c.text[0][0]
    assert c.json == [(c.json[0][0], 0)] and info["injuries"] == "0 reports"
    assert any("starting goalies refresh failed" in w for w in info["warnings"])
    # with goalies known, only tonight's teams' line pages are refetched, and only when older than 6 h
    from fantasy_manager.providers.dailyfaceoff import make_refresh_fetch_text
    c2 = Cache()
    fetch = make_refresh_fetch_text(c2)
    for url in ("https://www.dailyfaceoff.com/teams/carolina-hurricanes/line-combinations",
                "https://www.dailyfaceoff.com/starting-goalies/2026-09-29"):
        with pytest.raises(RuntimeError):
            fetch(url, None)
    assert [t for _, t in c2.text] == [6 * 3600.0, 0.0]
    fake = FakeDfo(starts())
    got, info = pregame.refresh_sources(None, DAY, dfo_client=fake, injuries_fetch=no_injuries)
    assert fake.calls == ["goalies", "lines"] and len(got) == 6 and info["goalies"] == "0 confirmed / 6 listed"


# ------------------------------------------------------------------------------ summary + notify

def test_summary_length_is_capped():
    changes = [pregame.Change(kind="new_alert", text=f"Player number {i} moved to the first line tonight "
                                                     "after a long practice report", priority=3) for i in range(80)]
    text = pregame.summarize(changes, {"text": "Lineup: start A; bench B", "set": False}, AFTERNOON)
    assert len(text) <= pregame.SUMMARY_LIMIT
    assert text.startswith("Pre-game 5:02 PM:") and "more)" in text and text.endswith("Lineup: start A; bench B.")
    assert pregame.summarize([], None, AFTERNOON) == pregame.NO_CHANGES
    assert pregame.summarize([], {"text": "Lineup: start A", "set": False}, AFTERNOON) == \
        "Pre-game: no changes. Lineup: start A."


def test_quiet_if_unchanged_suppresses_notify(tmp_path):
    calls = []

    def notifier(s, title, lines):
        calls.append((title, lines))
        return ["discord: sent 1 message(s)"]

    morning(tmp_path)
    quiet = run(tmp_path, {}, starts(), notify=True, quiet_if_unchanged=True, notifier=notifier)
    assert quiet.nothing_changed and quiet.notify is None and calls == []
    loud = run(tmp_path, {}, starts(), notify=True, notifier=notifier)
    assert loud.notify == ["discord: sent 1 message(s)"] and calls == [("Pre-game 5:02 PM - ESPN", [pregame.NO_CHANGES])]
    changed = run(tmp_path, {"bussi_start": True}, starts(CAR=("Brandon Bussi", "Confirmed")), notify=True,
                  quiet_if_unchanged=True, notifier=notifier)
    assert not changed.nothing_changed and calls[-1][1][0] == "Bussi CONFIRMED (CAR vs FLA)"
    assert calls[-1][1][-1] == "No lineup changes needed"


def test_notify_changes_formats_bullets_and_chunks():
    sent = []

    class Client:
        def post(self, url, json=None, timeout=None):
            sent.append((url, json))
            return SimpleNamespace(status_code=204, text="", headers={})

    s = SimpleNamespace(discord_webhook_url="https://discord.invalid/hook", slack_webhook_url="https://slack.invalid/x")
    lines = [f"line {i} " + "x" * 90 for i in range(30)]
    res = notify_mod.notify_changes(s, "Pre-game 5:02 PM - ESPN", lines, client=Client())
    assert res[0].startswith("discord: sent") and res[1].startswith("slack: sent")
    discord = [p["content"] for u, p in sent if "discord" in u]
    slack = [p["text"] for u, p in sent if "slack" in u]
    assert len(discord) >= 2 and all(len(c) <= 1500 for c in discord + slack)
    assert discord[0].startswith("**Pre-game 5:02 PM - ESPN**\n• line 0") and slack[0].startswith("*Pre-game")
    assert notify_mod.notify_changes(SimpleNamespace(), "t", ["a"]) == \
        ["no webhooks configured (set DISCORD_WEBHOOK_URL and/or SLACK_WEBHOOK_URL)"]


# ------------------------------------------------------------------------------ CLI

@pytest.fixture
def cli_env(tmp_path, monkeypatch):
    from rich.console import Console

    s = settings(tmp_path, discord_webhook_url="https://discord.invalid/hook", slack_webhook_url=None)
    monkeypatch.setattr(cli_harness, "_settings", lambda: s)
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    return tmp_path


def test_failure_message_classification():
    assert pregame.failure_message("fantrax", ProviderError("Fantrax says you are not logged in: ...")) == \
        "Fantrax login expired: refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)"
    assert pregame.failure_message("espn", "ESPN denied access: 401") == \
        "ESPN login expired: refresh ESPN_S2 / ESPN_SWID in .env"
    assert pregame.failure_message("espn", "ESPN request failed: boom") == "ESPN not loaded: ESPN request failed: boom"


def test_daily_notifies_league_failure_and_keeps_going(cli_env, monkeypatch):
    from fantasy_manager.harness import realized as R

    def load(lg, s, c, with_recs):
        if lg == "fantrax":
            raise ProviderError("Fantrax says you are not logged in: the saved session or FANTRAX_COOKIE is missing")
        raise RuntimeError("ESPN exploded")

    sent = []
    monkeypatch.setattr(cli_backtest, "_load_league_full", load)
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    monkeypatch.setattr(notify_mod, "notify_all", lambda s, text, client=None: sent.append(text) or ["discord: sent 1 message(s)"])
    res = CliRunner().invoke(cli_harness.harness_app, ["daily", "--league", "all", "--json"])
    assert res.exit_code == 0, res.output
    out = json.loads(res.output)
    assert [lg.get("skipped") is not None for lg in out["leagues"]] == [True, True]
    assert out["failure_notify"] == ["discord: sent 1 message(s)"]
    assert sent == ["fm harness daily: ESPN not loaded: RuntimeError: ESPN exploded\n"
                    "fm harness daily: Fantrax login expired: refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)"]
    assert "lines" in out and "realized" in out                    # the other steps still ran


def test_daily_without_webhooks_sends_nothing(tmp_path, monkeypatch):
    from fantasy_manager.harness import realized as R

    monkeypatch.setattr(cli_harness, "_settings", lambda: settings(tmp_path))
    monkeypatch.setattr(cli_backtest, "_load_league_full",
                        lambda lg, s, c, w: (_ for _ in ()).throw(ProviderError("not logged in")))
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    monkeypatch.setattr(notify_mod, "notify_all", lambda *a, **k: pytest.fail("must not notify"))
    res = CliRunner().invoke(cli_harness.harness_app, ["daily", "--league", "fantrax", "--json"])
    assert res.exit_code == 0 and json.loads(res.output)["failure_notify"] is None


def test_daily_writes_pregame_baseline(cli_env, monkeypatch):
    from fantasy_manager.harness import realized as R

    ctx, values = make_league(as_of=date.today())

    class Prov:
        warnings: list = []

        def activity(self, since=None):
            return []

        def lineup_snapshot(self, day):
            return []
    monkeypatch.setattr(cli_backtest, "_load_league_full", lambda lg, s, c, w: (ctx, values, [], [], Prov()))
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    res = CliRunner().invoke(cli_harness.harness_app, ["daily", "--league", "espn", "--no-archive", "--json"])
    assert res.exit_code == 0, res.output
    assert json.loads(res.output)["leagues"][0]["pregame_snapshot"].startswith("written (espn-")
    assert pregame.load_snapshot(cli_env, "espn", date.today())["source"] == "daily"


def test_cli_pregame_quiet_if_unchanged_and_failure(cli_env, monkeypatch):
    ctx, values = make_league(as_of=date.today())

    def load(lg, s, c, with_recs):
        if lg == "fantrax":
            raise ProviderError("Fantrax says you are not logged in")
        return ctx, values, [], [], None

    changes_sent, fail_sent = [], []
    monkeypatch.setattr(cli_backtest, "_load_league_full", load)
    monkeypatch.setattr(notify_mod, "notify_changes",
                        lambda s, title, lines, client=None: changes_sent.append((title, lines)) or ["discord: sent 1 message(s)"])
    monkeypatch.setattr(notify_mod, "notify_all", lambda s, text, client=None: fail_sent.append(text) or ["discord: sent 1 message(s)"])
    runner = CliRunner()
    res = runner.invoke(cli_harness.harness_app, ["pregame", "--league", "espn", "--json"])
    assert res.exit_code == 0, res.output
    first = json.loads(res.output)["leagues"][0]
    assert first["baseline"] is None and first["snapshot"].endswith(f"espn-{date.today().isoformat()}.json")
    res = runner.invoke(cli_harness.harness_app, ["pregame", "--league", "espn", "--notify", "--quiet-if-unchanged"])
    assert res.exit_code == 0, res.output
    assert "Pre-game: no changes. Lineup set." in res.output and "nothing sent" in res.output
    assert changes_sent == []
    res = runner.invoke(cli_harness.harness_app, ["pregame", "--league", "both", "--notify", "--quiet-if-unchanged"])
    assert res.exit_code == 1                                     # fantrax failed; espn still ran
    assert "Fantrax login expired" in res.output and "Pre-game: no changes" in res.output
    assert changes_sent == [] and fail_sent == [
        "fm harness pregame: Fantrax login expired: refresh FANTRAX_COOKIE (or run `fm auth fantrax --login`)"]


def test_opponent_goalies_merge_into_one_phone_line(tmp_path):
    ctx, values = make_league()
    ctx.my_team.slots.append(RosterSlot(slot="BN", player=P(7, "Mika Zibanejad", ["C"], "NYR"), starting=False))
    values["espn:7"] = V(ctx.my_team.slots[-1].player, 2.0)
    morning(tmp_path)
    res = pregame.run_pregame("espn", settings(tmp_path), None, as_of=DAY, now=AFTERNOON,
                              loader=lambda lg, s, c: (ctx, values, []),
                              dfo_client=FakeDfo(starts(FLA=("Sergei Bobrovsky", "Confirmed"),
                                                        BOS=("Jeremy Swayman", "Confirmed"))),
                              injuries_fetch=no_injuries)
    opp = [c for c in res.changes if c.scope == "opponent"]
    assert len(opp) == 2 and all(c.short for c in opp)            # typed data keeps one change per goalie
    assert "Confirmed vs your skaters: Swayman (BOS), Bobrovsky (FLA)." in res.summary_text
    assert res.notify_lines()[0] == "Confirmed vs your skaters: Swayman (BOS), Bobrovsky (FLA)"


def test_today_lineup_starts_bench_players_over_idle_starters():
    ctx, values = make_league()
    ctx.schedule["CAR"] = [date(2026, 9, 30)]                     # Jarvis and Bussi have no game today
    lu = pregame.today_lineup(ctx, values)
    assert lu["to_start"] == ["Nick Suzuki", "Anthony Stolarz"] and lu["to_bench"] == []
    assert lu["text"] == "Lineup: start Suzuki, Stolarz for Jarvis, Bussi (no game)"
    assert lu["start"] == ["espn:2", "espn:4"] and not lu["set"]


def test_quiet_if_unchanged_still_posts_when_todays_lineup_is_unset():
    """An unset lineup at 5 PM is the most useful thing to send, even when nothing changed."""
    from fantasy_manager import pregame as pg
    from datetime import date, datetime
    res = pg.PregameResult(league="espn", day=date(2026, 10, 1), ran_at=datetime(2026, 10, 1, 17, 2),
                           summary_text=pg.NO_CHANGES, nothing_changed=True,
                           lineup_recs={"week": [], "today": {"set": False, "to_start": ["A"], "to_bench": ["B"],
                                                              "start": [], "bench": [], "idle_out": [],
                                                              "optimal_total": 1.0, "current_total": 0.0, "text": "start A for B"}})
    today = res.lineup_recs.get("today") or {}
    assert bool(today) and not today.get("set", True)
