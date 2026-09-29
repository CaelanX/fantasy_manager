"""Match recommendation episodes to what I actually did.

``match_rows`` is pure (plain dataclasses in, updates out) so every rule is unit-testable;
``match_episodes`` loads the ledger rows, runs it and writes the results back. Statuses are
recomputed from scratch on every run (the inputs are append-only), so matching is idempotent.

Rules (window = first_seen .. window_end; only my own transactions count):

* waiver   window_end = last_seen + 3d. My ADD of an ``add`` player: ``followed`` when the
           drops of that transaction (same group, else same day) equal the rec's drops,
           else ``partial``.
* trade    last_seen + 7d. A completed trade (one group) sharing at least one give and one
           get with the rec: ``followed`` when give and get sets are equal, else ``partial``.
           Only a matching proposal of mine: ``proposed`` (not terminal).
* lineup   graded on the first lockable day: ESPN the day it was issued (its box score; the
           next day as a fallback when that one is missing), Fantrax the next Monday (the rec's
           own day when it is a Monday) using the first roster snapshot taken after that lock
           (Tuesday..Sunday; a Monday-morning snapshot predates the lock).
           ``followed`` when every ``add`` starts and every ``drop`` does not, ``partial``
           when only one side did, else ``expired`` once the lineup is known.
* injury   last_seen + 2d. The subject (IR target) in IR (an IR transaction or an IR slot in
           my lineup) and, if the rec adds someone, my ADD of him: both ``followed``, one
           ``partial``. "Activate X from IR" recs: X out of the IR slot (or an ACTIVATE).
* flags    last_seen + 14d. sell_high: I traded the player away; buy_low: I traded for him
           (``proposed`` when only offered).
* alert    (:func:`alert_category`) a free-agent alert (counterparty "FA", "free agent" in the
           title, or a subject on no fantasy roster that day per the lineup rows), last_seen + 3d:
           my ADD of a subject is ``followed`` (match detail ``added`` / ``dropped``: the drops of
           that transaction group, else of that day; graded like a waiver add). A negative alert
           about my own player (role loss, off PP1, out of the lineup / scratched, a negative
           rookie news signal), last_seen + 7d: my DROP or TRADE_OUT of him is ``followed``
           (``dropped`` / ``added``: what came back in that group). Every other alert is
           informational: it expires as before and is never graded (no hit-rate denominator).

Unmatched episodes turn ``expired`` once ``today`` is past ``window_end``. Every matched
episode becomes a decision (origin = its status); my transaction groups that no episode
consumed become ``decisions(origin=user_only)``. Other teams' moves stay in
``transactions`` as league-wide samples.
"""
from __future__ import annotations

import json
import re
from collections import Counter
from dataclasses import dataclass, field
from datetime import date, timedelta
from typing import Any, Iterable

from .ledger import Ledger, now_iso

WINDOW_DAYS = {"waiver": 3, "trade": 7, "injury": 2, "sell_high": 14, "buy_low": 14}
ALERT_FA_DAYS = 3         # a free-agent alert followed by my add within last_seen + 3d
ALERT_MINE_DAYS = 7       # a negative alert on my player followed by my drop / trade within last_seen + 7d
FA_COUNTERPARTY = "FA"
# titles of negative alerts about my own players (recommend.alerts / recommend.flags)
_NEGATIVE_ALERT_RE = re.compile(r"^Role loss:|dropped off PP1|out of the lineup|scratched|demot", re.I)
LINEUP_GRACE = {"espn": 2, "fantrax": 7}
TERMINAL = ("followed", "partial")


@dataclass
class EpisodeRow:
    episode_id: str
    kind: str
    first_seen: date
    last_seen: date
    title: str = ""
    adds: set[str] = field(default_factory=set)
    drops: set[str] = field(default_factory=set)
    subjects: set[str] = field(default_factory=set)
    counterparty: str | None = None
    negative: bool = False          # alerts: a negative signal (role loss, demotion, scratch)


def alert_category(ep: EpisodeRow) -> str | None:
    """Alerts only: "fa" (a free agent: counterparty "FA" or "free agent" in the title), "mine"
    (a negative alert about my own player) or "info" (anything else; ``match_rows`` still turns
    an "info" alert whose subject is on no fantasy roster that day into "fa")."""
    if ep.kind != "alert":
        return None
    if (ep.counterparty or "").upper() == FA_COUNTERPARTY or "free agent" in (ep.title or "").lower():
        return "fa"
    if ep.negative or _NEGATIVE_ALERT_RE.search(ep.title or ""):
        return "mine"
    return "info"


@dataclass
class TxRow:
    tx_id: str
    action: str
    cid: str
    team_id: str
    day: date
    group_id: str | None = None
    counterparty_id: str | None = None

    @property
    def group(self) -> str:
        return self.group_id or self.tx_id


@dataclass
class LineupRow:
    team_id: str
    day: date
    cid: str
    slot: str
    starting: bool


@dataclass
class MatchResult:
    episodes: list[dict[str, Any]] = field(default_factory=list)   # episode_id, status, acted_on, window_end, match
    decisions: list[dict[str, Any]] = field(default_factory=list)


@dataclass
class MatchSummary:
    league: str
    episodes: int = 0
    by_status: dict[str, int] = field(default_factory=dict)
    by_kind_status: dict[str, dict[str, int]] = field(default_factory=dict)
    decisions: dict[str, int] = field(default_factory=dict)

    def line(self) -> str:
        st = ", ".join(f"{k} {v}" for k, v in sorted(self.by_status.items())) or "none"
        dec = ", ".join(f"{k} {v}" for k, v in sorted(self.decisions.items())) or "none"
        return f"{self.league}: {self.episodes} episodes ({st}); decisions: {dec}"


def first_lockable_day(provider: str, day: date) -> date:
    """ESPN locks per game (the rec's own day); Fantrax lineups lock weekly on Monday."""
    if provider == "fantrax":
        return day + timedelta(days=(7 - day.weekday()) % 7)
    return day


def window_end(ep: EpisodeRow, provider: str) -> date:
    if ep.kind == "alert":
        return ep.last_seen + timedelta(days=ALERT_MINE_DAYS if alert_category(ep) == "mine" else ALERT_FA_DAYS)
    if ep.kind == "lineup":
        return first_lockable_day(provider, ep.first_seen) + timedelta(days=LINEUP_GRACE.get(provider, 2))
    return ep.last_seen + timedelta(days=WINDOW_DAYS.get(ep.kind, 3))


# --------------------------------------------------------------------------- per-kind rules

def _in_window(t: TxRow, ep: EpisodeRow, end: date) -> bool:
    return ep.first_seen <= t.day <= end


def _match_waiver(ep: EpisodeRow, mine: list[TxRow], end: date) -> tuple[str, date, set[str], dict] | None:
    adds = sorted((t for t in mine if t.action == "ADD" and t.cid in ep.adds and _in_window(t, ep, end)),
                  key=lambda t: t.day)
    if not adds:
        return None
    t0 = adds[0]
    same = [t for t in mine if t.action == "DROP" and t.group == t0.group]
    if not same:
        same = [t for t in mine if t.action == "DROP" and t.day == t0.day]
    dropped = {t.cid for t in same}
    status = "followed" if dropped == ep.drops else "partial"
    groups = {t0.group} | {t.group for t in same}
    return status, t0.day, groups, {"added": sorted({t.cid for t in adds}), "dropped": sorted(dropped)}


def _trade_groups(mine: list[TxRow], my_team: str | None, ep: EpisodeRow, end: date, proposed: bool
                  ) -> dict[str, tuple[set[str], set[str], date]]:
    """group -> (players I get, players I give, day) of my trades (or my proposals)."""
    out: dict[str, tuple[set[str], set[str], date]] = {}
    for t in mine:
        if not _in_window(t, ep, end):
            continue
        get = give = False
        if proposed and t.action == "PROPOSED":
            get = t.team_id == my_team
            give = t.counterparty_id == my_team and not get
        elif not proposed and t.action in ("TRADE_IN", "TRADE_OUT"):
            get, give = t.action == "TRADE_IN", t.action == "TRADE_OUT"
        if not (get or give):
            continue
        g = out.setdefault(t.group, (set(), set(), t.day))
        (g[0] if get else g[1]).add(t.cid)
        if t.day < g[2]:
            out[t.group] = (g[0], g[1], t.day)
    return out


def _match_trade(ep: EpisodeRow, mine: list[TxRow], my_team: str | None, end: date
                 ) -> tuple[str, date, set[str], dict] | None:
    for proposed in (False, True):
        best = None
        for gid, (gets, gives, day) in _trade_groups(mine, my_team, ep, end, proposed).items():
            if not (gets & ep.adds and gives & ep.drops):
                continue
            exact = gets == ep.adds and gives == ep.drops
            status = "proposed" if proposed else ("followed" if exact else "partial")
            cand = (0 if exact else 1, day, status, gid, gets, gives)
            if best is None or cand[:2] < best[:2]:
                best = cand
        if best is not None:
            _, day, status, gid, gets, gives = best
            return status, day, {gid}, {"got": sorted(gets), "gave": sorted(gives)}
    return None


def _match_flag(ep: EpisodeRow, mine: list[TxRow], my_team: str | None, end: date
                ) -> tuple[str, date, set[str], dict] | None:
    targets = ep.drops if ep.kind == "sell_high" else ep.adds
    action = "TRADE_OUT" if ep.kind == "sell_high" else "TRADE_IN"
    hits = sorted((t for t in mine if t.action == action and t.cid in targets and _in_window(t, ep, end)),
                  key=lambda t: t.day)
    if hits:
        return "followed", hits[0].day, {hits[0].group}, {"traded": sorted({t.cid for t in hits})}
    for t in sorted(mine, key=lambda t: t.day):
        if t.action != "PROPOSED" or t.cid not in targets or not _in_window(t, ep, end):
            continue
        if (ep.kind == "sell_high" and t.counterparty_id == my_team) or (ep.kind == "buy_low" and t.team_id == my_team):
            return "proposed", t.day, {t.group}, {"offered": [t.cid]}
    return None


def _lineup_on(lineups: list[LineupRow], days: Iterable[date]) -> tuple[date, dict[str, LineupRow]] | None:
    by_day: dict[date, dict[str, LineupRow]] = {}
    for r in lineups:
        by_day.setdefault(r.day, {})[r.cid] = r
    for d in days:
        if by_day.get(d):
            return d, by_day[d]
    return None


def _match_lineup(ep: EpisodeRow, lineups: list[LineupRow], provider: str
                  ) -> tuple[str, date, set[str], dict] | None:
    lock = first_lockable_day(provider, ep.first_seen)
    # ESPN: the box score of the lock day itself (pulled the next morning). Fantrax: the first
    # snapshot taken after the Monday lock (a Monday-morning snapshot predates the lock).
    days = (lock + timedelta(days=i) for i in (range(1, 7) if provider == "fantrax" else range(2)))
    found = _lineup_on(lineups, days)
    if found is None:
        return None
    day, rows = found
    activation = ep.title.startswith("Activate")

    def on(c: str) -> bool:
        r = rows.get(c)
        return r is not None and (r.slot != "IR" if activation else r.starting)

    in_ok = all(on(c) for c in ep.adds) if ep.adds else None
    out_ok = all(not (rows.get(c) and rows[c].starting) for c in ep.drops) if ep.drops else None
    parts = [x for x in (in_ok, out_ok) if x is not None]
    detail = {"lock_day": lock.isoformat(), "graded_day": day.isoformat(), "in_ok": in_ok, "out_ok": out_ok}
    if parts and all(parts):
        return "followed", lock, set(), detail
    if any(parts):
        return "partial", lock, set(), detail
    return "expired", lock, set(), {**detail, "reason": "lineup not changed"}


def _match_injury(ep: EpisodeRow, mine: list[TxRow], lineups: list[LineupRow], end: date
                  ) -> tuple[str, date, set[str], dict] | None:
    groups: set[str] = set()
    days: list[date] = []
    if ep.title.startswith("Activate") and not ep.subjects:
        for t in mine:
            if t.action == "ACTIVATE" and t.cid in ep.adds and _in_window(t, ep, end):
                return "followed", t.day, {t.group}, {"activated": [t.cid]}
        for r in sorted(lineups, key=lambda r: r.day):
            if r.cid in ep.adds and r.slot != "IR" and ep.first_seen <= r.day <= end:
                return "followed", r.day, set(), {"activated": [r.cid]}
        return None
    parts: list[bool] = []
    detail: dict[str, Any] = {}
    if ep.subjects:
        ir_tx = [t for t in mine if t.action == "IR" and t.cid in ep.subjects and _in_window(t, ep, end)]
        ir_slot = [r for r in lineups if r.cid in ep.subjects and r.slot == "IR" and ep.first_seen <= r.day <= end]
        ok = bool(ir_tx or ir_slot)
        parts.append(ok)
        detail["ir"] = ok
        groups |= {t.group for t in ir_tx}
        days += [t.day for t in ir_tx] + [r.day for r in ir_slot]
    if ep.adds:
        adds = [t for t in mine if t.action == "ADD" and t.cid in ep.adds and _in_window(t, ep, end)]
        parts.append(bool(adds))
        detail["added"] = bool(adds)
        groups |= {t.group for t in adds}
        days += [t.day for t in adds]
    if not parts or not any(parts):
        return None
    return ("followed" if all(parts) else "partial"), min(days), groups, detail


def _rostered_on(lineups: list[LineupRow], cids: set[str], day: date) -> bool | None:
    """Whether any of ``cids`` is on a fantasy roster on ``day`` per the lineup rows of every
    team (the latest lineup day on or before ``day``, at most 3 days back); None without rows."""
    days = sorted({r.day for r in lineups if day - timedelta(days=3) <= r.day <= day})
    if not days:
        return None
    last = days[-1]
    return any(r.cid in cids for r in lineups if r.day == last)


def _match_alert(ep: EpisodeRow, mine: list[TxRow], end: date, category: str
                 ) -> tuple[str, date, set[str], dict] | None:
    targets = ep.subjects | ep.adds
    if category == "fa":
        adds = sorted((t for t in mine if t.action == "ADD" and t.cid in targets and _in_window(t, ep, end)),
                      key=lambda t: t.day)
        if not adds:
            return None
        t0 = adds[0]
        same = [t for t in mine if t.action == "DROP" and t.group == t0.group]
        if not same:
            same = [t for t in mine if t.action == "DROP" and t.day == t0.day]
        groups = {t0.group} | {t.group for t in same}
        return "followed", t0.day, groups, {"alert": "fa", "added": sorted({t.cid for t in adds if t.day == t0.day}),
                                            "dropped": sorted({t.cid for t in same})}
    if category == "mine":
        outs = sorted((t for t in mine if t.action in ("DROP", "TRADE_OUT") and t.cid in targets
                       and _in_window(t, ep, end)), key=lambda t: t.day)
        if not outs:
            return None
        t0 = outs[0]
        back = [t for t in mine if t.action in ("ADD", "TRADE_IN") and t.group == t0.group]
        if not back and t0.action == "DROP":
            back = [t for t in mine if t.action == "ADD" and t.day == t0.day]
        groups = {t0.group} | {t.group for t in back}
        return "followed", t0.day, groups, {"alert": "mine", "via": "trade" if t0.action == "TRADE_OUT" else "drop",
                                            "dropped": sorted({t.cid for t in outs if t.day == t0.day}),
                                            "added": sorted({t.cid for t in back})}
    return None


# --------------------------------------------------------------------------- driver

def _decision_kind(actions: set[str]) -> str:
    if actions & {"TRADE_IN", "TRADE_OUT"}:
        return "trade"
    if actions & {"IR", "ACTIVATE"} and not actions & {"ADD", "DROP"}:
        return "injury"
    return "waiver"


def match_rows(episodes: Iterable[EpisodeRow], txs: Iterable[TxRow], lineups: Iterable[LineupRow],
               my_team: str | None, provider: str, today: date, league: str = "") -> MatchResult:
    """Pure matching over ledger rows (see module docstring). ``txs`` / ``lineups`` may hold
    every team's rows; only ``my_team``'s are used."""
    txs = list(txs)
    mine = [t for t in txs if t.team_id == my_team or (t.action == "PROPOSED" and t.counterparty_id == my_team)]
    lineups = list(lineups)
    my_lineups = [r for r in lineups if r.team_id == my_team]
    res = MatchResult()
    consumed: set[str] = set()
    for ep in episodes:
        category = alert_category(ep)
        if category == "info" and _rostered_on(lineups, ep.subjects | ep.adds, ep.first_seen) is False:
            category = "fa"          # on no fantasy roster that day: a free agent
            ep = EpisodeRow(**{**ep.__dict__, "counterparty": FA_COUNTERPARTY})
        end = window_end(ep, provider)
        m = None
        if ep.kind == "waiver":
            m = _match_waiver(ep, mine, end)
        elif ep.kind == "trade":
            m = _match_trade(ep, mine, my_team, end)
        elif ep.kind in ("sell_high", "buy_low"):
            m = _match_flag(ep, mine, my_team, end)
        elif ep.kind == "lineup":
            m = _match_lineup(ep, my_lineups, provider)
        elif ep.kind == "injury":
            m = _match_injury(ep, mine, my_lineups, end)
        elif ep.kind == "alert":
            m = _match_alert(ep, mine, end, category or "info")
        if m is None:
            status = "expired" if today > end else "open"
            res.episodes.append({"episode_id": ep.episode_id, "status": status, "acted_on": None,
                                 "window_end": end.isoformat(), "match_json": None})
            continue
        status, day, groups, detail = m
        if status == "proposed" and today > end:
            detail = {**detail, "window_passed": True}
        res.episodes.append({"episode_id": ep.episode_id, "status": status,
                             "acted_on": day.isoformat() if status != "expired" else None,
                             "window_end": end.isoformat(), "match_json": json.dumps(detail, sort_keys=True)})
        if status == "expired":
            continue
        consumed |= groups
        if ep.kind == "alert":
            adds, drops = set(detail.get("added") or []), set(detail.get("dropped") or [])
        else:
            adds, drops = ep.adds, ep.drops
        res.decisions.append({"decision_id": f"ep:{ep.episode_id}", "league": league, "day": day.isoformat(),
                              "origin": status, "kind": ep.kind, "episode_id": ep.episode_id,
                              "group_id": ",".join(sorted(groups)) or None,
                              "adds_json": json.dumps(sorted(adds)), "drops_json": json.dumps(sorted(drops)),
                              "created_at": now_iso()})
    groups_tx: dict[str, list[TxRow]] = {}
    for t in mine:
        if t.action == "PROPOSED" or t.team_id != my_team:
            continue
        groups_tx.setdefault(t.group, []).append(t)
    for gid, ts in groups_tx.items():
        if gid in consumed:
            continue
        actions = {t.action for t in ts}
        res.decisions.append({"decision_id": f"tx:{league}:{gid}", "league": league,
                              "day": min(t.day for t in ts).isoformat(), "origin": "user_only",
                              "kind": _decision_kind(actions), "episode_id": None, "group_id": gid,
                              "adds_json": json.dumps(sorted(t.cid for t in ts if t.action in ("ADD", "TRADE_IN"))),
                              "drops_json": json.dumps(sorted(t.cid for t in ts if t.action in ("DROP", "TRADE_OUT"))),
                              "created_at": now_iso()})
    return res


def my_team_id(ledger: Ledger, league: str) -> str | None:
    row = ledger.query("SELECT value FROM meta WHERE key=?", (f"my_team:{league}",))
    return row[0]["value"] if row else None


def set_my_team_id(ledger: Ledger, league: str, team_id: str) -> None:
    ledger.execute("INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                   (f"my_team:{league}", str(team_id)))
    ledger.commit()


def load_episodes(ledger: Ledger, league: str) -> list[EpisodeRow]:
    eps = ledger.query("SELECT episode_id, rec_key, kind, title, first_seen, last_seen FROM rec_episodes"
                       " WHERE league=? ORDER BY first_seen", (league,))
    players = ledger.query("SELECT as_of, rec_key, side, cid FROM rec_players WHERE league=?", (league,))
    recs = {(r["rec_key"], r["as_of"]): r for r in ledger.query(
        "SELECT rec_key, as_of, counterparty, reasons_json FROM recs WHERE league=? AND kind='alert'", (league,))}
    idx: dict[tuple[str, str], dict[str, set[str]]] = {}
    for p in players:
        idx.setdefault((p["rec_key"], p["as_of"]), {}).setdefault(p["side"], set()).add(p["cid"])
    out = []
    for e in eps:
        sides = idx.get((e["rec_key"], e["first_seen"]), {})
        rec = recs.get((e["rec_key"], e["first_seen"])) or {}
        out.append(EpisodeRow(episode_id=e["episode_id"], kind=e["kind"], title=e["title"] or "",
                              first_seen=date.fromisoformat(e["first_seen"]),
                              last_seen=date.fromisoformat(e["last_seen"]),
                              adds=sides.get("add", set()), drops=sides.get("drop", set()),
                              subjects=sides.get("subject", set()), counterparty=rec.get("counterparty"),
                              negative=_negative_reasons(rec.get("reasons_json"))))
    return out


def _negative_reasons(raw: str | None) -> bool:
    """A negative rookie news signal (ROLE_NEWS with value < 0) among an alert's reasons."""
    try:
        reasons = json.loads(raw) if raw else []
    except ValueError:
        return False
    return any(isinstance(r, dict) and r.get("code") == "ROLE_NEWS" and (r.get("value") or 0) < 0 for r in reasons)


def load_txs(ledger: Ledger, league: str) -> list[TxRow]:
    return [TxRow(tx_id=r["tx_id"], action=r["action"], cid=r["cid"], team_id=r["team_id"],
                  day=date.fromisoformat(r["day"]), group_id=r["group_id"], counterparty_id=r["counterparty_id"])
            for r in ledger.query("SELECT * FROM transactions WHERE league=?", (league,))]


def load_lineups(ledger: Ledger, league: str, team_id: str | None, all_teams: bool = False) -> list[LineupRow]:
    """My lineup rows (every team's with ``all_teams``: free-agent alerts check who was rostered)."""
    if team_id is None:
        return []
    sql, params = "SELECT * FROM lineup_days WHERE league=?", (league,)
    if not all_teams:
        sql, params = sql + " AND team_id=?", (league, team_id)
    return [LineupRow(team_id=r["team_id"], day=date.fromisoformat(r["day"]), cid=r["cid"], slot=r["slot"] or "",
                      starting=bool(r["starting"]))
            for r in ledger.query(sql, params)]


def match_episodes(ledger: Ledger, league: str, today: date | None = None,
                   provider: str | None = None) -> MatchSummary:
    """Match every episode of ``league`` and rewrite its decisions. ``provider`` defaults to
    ``league`` (the ledger keys leagues by provider name)."""
    today = today or date.today()
    team = my_team_id(ledger, league)
    eps = load_episodes(ledger, league)
    res = match_rows(eps, load_txs(ledger, league), load_lineups(ledger, league, team, all_teams=True), team,
                     provider or league, today, league)
    now = now_iso()
    for u in res.episodes:
        ledger.execute("UPDATE rec_episodes SET status=?, acted_on=?, window_end=?, match_json=?, updated_at=?"
                       " WHERE episode_id=?",
                       (u["status"], u["acted_on"], u["window_end"], u["match_json"], now, u["episode_id"]))
    ledger.execute("DELETE FROM decisions WHERE league=?", (league,))
    ledger.commit()
    ledger.upsert("decisions", res.decisions, ("decision_id",))
    summary = MatchSummary(league=league, episodes=len(res.episodes))
    kinds = {e.episode_id: e.kind for e in eps}
    summary.by_status = dict(Counter(u["status"] for u in res.episodes))
    for u in res.episodes:
        summary.by_kind_status.setdefault(kinds[u["episode_id"]], {}).setdefault(u["status"], 0)
        summary.by_kind_status[kinds[u["episode_id"]]][u["status"]] += 1
    summary.decisions = dict(Counter(d["origin"] for d in res.decisions))
    return summary
