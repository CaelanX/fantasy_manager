"""Role multipliers from Daily Faceoff lineups and news: shared by the rookie model
(``valuation.rookie``) and the role-change step for established skaters, plus goalie job news on
the start share. Both are applied by ``valuation.valuate.valuate_league``.

Tiers (the rookie model's rule): a top role - first line (F1), top pair (D1) or the first
power-play unit (PP1) - is x1.15 on the offensive scoring rates (once, not stacked); PP2, a second
line / pair or "top-six" x1.05; a depth line (F4, D3, "fourth line") with no positive signal x0.92;
anything else x1.0.

Established skaters (everyone the rookie model doesn't value): their rates already carry their
usual role, so only a CHANGE moves them, by tier(now) / tier(usual):

* usual: his most common Daily Faceoff line and PP unit in the snapshots of the last 30 days
  (``Player.usual_line`` / ``usual_pp_unit``, ``providers.lines_enrich``). Without snapshot history
  the PP part comes from his share of his team's PP time (NHL reports: >= 50% PP1, >= 20% PP2) and
  the line part is taken as unchanged; with neither there is no adjustment.
* now: today's Daily Faceoff line / PP unit. When Daily Faceoff doesn't list him (or lists no PP
  units for his team), the newest usable news signal (<= ``ESTABLISHED_NEWS_MAX_AGE_DAYS`` = 10
  days; same confidence floors as the rookie model) fills that part; else his usual role.
  News of a third pair or a reduced role counts as a depth line; "dropped to the second unit" as PP2.
* The week value gets the full multiplier; the rest of the season moves by
  ``ROLE_SEASON_WEIGHT`` (half) of the change, because a promotion or demotion only sticks part
  of the time. Reasons ROLE_DFO / ROLE_NEWS (the evidence) and ROLE_BOOST / ROLE_DEPTH (the change).
* A healthy player Daily Faceoff no longer lists in his team's lineup after a scratch (the
  snapshot change "scratched", or "healthy scratch" news within ``SCRATCH_NEWS_MAX_AGE_DAYS`` = 7
  days) gets x``SCRATCH_MULT`` (0.85) on the week value (reason ROLE_SCRATCH).

Goalies: job news (``providers.news_roles`` kinds goalie_role / goalie_split, <=
``START_NEWS_MAX_AGE_DAYS`` = 14 days, newest wins) moves the start share part of the way to a
target - won the job 0.70, lost it 0.30, a tandem 0.50 - by ``START_NEWS_WEIGHT`` (0.5) x the
signal's confidence, and only in the signal's direction (reason START_NEWS). It is skipped once the
share comes from ledger starts (START_ACTUAL): actual starts beat news. One-game news ("gets the
nod") stays with Daily Faceoff's confirmed starts in the daily lineup.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date
from typing import Any, Iterable

from ..models import Player, Reason

ROLE_BOOST = 1.15
ROLE_MINOR_BOOST = 1.05
DEPTH_PENALTY = 0.92
SCRATCH_MULT = 0.85
TOP_KINDS = ("top_line", "pp1", "first_line_pairing")
MINOR_KINDS = ("pp2", "extended_role")

ROLE_SEASON_WEIGHT = 0.5             # share of a role change that carries into the rest of the season
ESTABLISHED_NEWS_MAX_AGE_DAYS = 10
SCRATCH_NEWS_MAX_AGE_DAYS = 7
PP1_SHARE, PP2_SHARE = 0.5, 0.2      # share of team PP time behind a usual PP1 / PP2 (no snapshots)

START_NEWS_MAX_AGE_DAYS = 14
START_NEWS_WEIGHT = 0.5
START_NEWS_TARGET = {("goalie_role", 1): 0.70, ("goalie_role", -1): 0.30, ("goalie_split", 1): 0.50}

_LINE_NEWS = ("top_line", "first_line_pairing", "extended_role")
_PP_NEWS = ("pp1", "pp2")


def role_multiplier(signals: Iterable[dict[str, Any]]) -> tuple[float, list[dict[str, Any]]]:
    """(multiplier, the signals behind it) for role signals {kind, direction, ...} (module tiers)."""
    sigs = list(signals)
    top = [d for d in sigs if d["kind"] in TOP_KINDS and d["direction"] > 0]
    minor = [d for d in sigs if d["kind"] in MINOR_KINDS and d["direction"] > 0]
    depth = [d for d in sigs if d["kind"] == "top_line" and d["direction"] < 0]
    if top:
        return ROLE_BOOST, top
    if minor:
        return ROLE_MINOR_BOOST, minor
    if depth:
        return DEPTH_PENALTY, depth
    return 1.0, []


def _line_kinds(line: str | None) -> list[dict[str, Any]]:
    line = (line or "").lower()
    if line == "f1":
        return [{"kind": "top_line", "direction": 1}]
    if line == "d1":
        return [{"kind": "first_line_pairing", "direction": 1}]
    if line in ("f2", "d2"):
        return [{"kind": "extended_role", "direction": 1}]
    if line in ("f4", "d3", "d4"):
        return [{"kind": "top_line", "direction": -1}]
    return []


def _pp_kinds(pp: str | None) -> list[dict[str, Any]]:
    pp = (pp or "").lower()
    return [{"kind": pp, "direction": 1}] if pp in ("pp1", "pp2") else []


def _news_line(d: dict[str, Any]) -> tuple[list[dict[str, Any]], str]:
    if d["direction"] > 0:
        return [{"kind": d["kind"], "direction": 1}], d["kind"].replace("_", " ")
    return [{"kind": "top_line", "direction": -1}], "depth role"


def _news_pp(d: dict[str, Any]) -> tuple[list[dict[str, Any]], str]:
    if d["kind"] == "pp1" and d["direction"] > 0:
        return _pp_kinds("pp1"), "PP1"
    return _pp_kinds("pp2"), "PP2"


@dataclass
class RoleChange:
    mult: float = 1.0                # on the scoring rates (this week's role)
    season_mult: float = 1.0         # what the rest of the season keeps of it
    week_extra: float = 1.0          # scratch: x SCRATCH_MULT on the week value only
    reasons: list[Reason] = field(default_factory=list)


def role_change(p: Player, news_signals: Iterable[Any], as_of: date | None, *, team_pp_listed: bool,
                team_lineup_listed: bool) -> RoleChange | None:
    """The established-skater role change (module docstring), or None when nothing moves.
    ``team_pp_listed`` / ``team_lineup_listed``: Daily Faceoff lists PP units / a lineup for his team."""
    from .rookie import _fmt_signal, dfo_signals, usable_news

    news = usable_news(news_signals, as_of, max_age_days=ESTABLISHED_NEWS_MAX_AGE_DAYS)
    out = RoleChange()
    # usual role
    if p.usual_line:
        usual_line: list[dict[str, Any]] | None = _line_kinds(p.usual_line)
        usual_pp = _pp_kinds(p.usual_pp_unit)
        usual_txt = f"{p.usual_line.upper()}, {(p.usual_pp_unit or 'no PP').upper().replace('NO PP', 'no PP')}"
    elif p.pp_share is not None:
        usual_line = None                                   # line part: unknown -> unchanged
        tier = "pp1" if p.pp_share >= PP1_SHARE else "pp2" if p.pp_share >= PP2_SHARE else None
        usual_pp = _pp_kinds(tier)
        usual_txt = f"{(tier or 'no PP').upper().replace('NO PP', 'no PP')} ({p.pp_share:.0%} of team PP time)"
    else:
        usual_line, usual_pp, usual_txt = None, [], ""
    evidence: list[Reason] = []
    if usual_line is not None or p.pp_share is not None:
        dfo = [d for d in dfo_signals(p) if d["kind"] != "nhl_roster"] if p.line else []
        # line part
        if usual_line is None:
            cur_line: list[dict[str, Any]] = []
            line_txt = None
        elif p.line:
            cur_line, line_txt = _line_kinds(p.line), p.line.upper()
            evidence += [Reason(code="ROLE_DFO", text=d["quote"], value=float(d["direction"]),
                                baseline=float(d["confidence"])) for d in dfo if d["kind"] not in _PP_NEWS]
        else:
            ln = next((d for d in news if d["kind"] in _LINE_NEWS), None)
            if ln is not None:
                cur_line, line_txt = _news_line(ln)
                line_txt += " (news)"
                evidence.append(Reason(code="ROLE_NEWS", text=f"{ln['kind'].replace('_', ' ')} "
                                       f"{'+' if ln['direction'] > 0 else '-'}: {_fmt_signal(ln)}",
                                       value=float(ln["direction"]), baseline=float(ln["confidence"])))
            else:
                cur_line, line_txt = usual_line, None
        # PP part
        if p.line and team_pp_listed:
            cur_pp, pp_txt = _pp_kinds(p.pp_unit), (p.pp_unit or "no PP").upper().replace("NO PP", "no PP")
            evidence += [Reason(code="ROLE_DFO", text=d["quote"], value=float(d["direction"]),
                                baseline=float(d["confidence"])) for d in dfo if d["kind"] in _PP_NEWS]
        else:
            pn = next((d for d in news if d["kind"] in _PP_NEWS), None)
            if pn is not None:
                cur_pp, pp_txt = _news_pp(pn)
                pp_txt += " (news)"
                evidence.append(Reason(code="ROLE_NEWS", text=f"{pn['kind']} {'+' if pn['direction'] > 0 else '-'}: "
                                       f"{_fmt_signal(pn)}", value=float(pn["direction"]),
                                       baseline=float(pn["confidence"])))
            else:
                cur_pp, pp_txt = usual_pp, None
        before, _ = role_multiplier((usual_line or []) + usual_pp)
        after, _ = role_multiplier(cur_line + cur_pp)
        r = after / before
        if abs(r - 1.0) > 1e-9:
            out.mult = r
            out.season_mult = 1.0 + (r - 1.0) * ROLE_SEASON_WEIGHT
            now_txt = ", ".join(t for t in (line_txt, pp_txt) if t)
            out.reasons = evidence + [Reason(
                code="ROLE_BOOST" if r > 1 else "ROLE_DEPTH",
                text=f"Role change: usually {usual_txt}, now {now_txt}: x{r:.2f} on scoring rates this week, "
                     f"x{out.season_mult:.2f} rest of season", value=round(r, 4), baseline=1.0)]
    # scratch: healthy, not in his team's Daily Faceoff lineup, after a scratch
    if p.status in ("healthy", "unknown") and not p.line and team_lineup_listed:
        sc = next((d for d in usable_news(news_signals, as_of, max_age_days=SCRATCH_NEWS_MAX_AGE_DAYS)
                   if d["kind"] == "scratched"), None)
        why = None
        if (p.line_change or "").lower() == "scratched":
            why = "Daily Faceoff: dropped out of his team's lineup"
        elif sc is not None and sc["direction"] < 0:
            why = f"scratched: {_fmt_signal(sc)}"
        if why:
            out.week_extra = SCRATCH_MULT
            out.reasons.append(Reason(code="ROLE_SCRATCH", text=f"{why}; not in the lineup now: week value "
                                                                f"x{SCRATCH_MULT:g}", value=SCRATCH_MULT, baseline=1.0))
    if out.mult == 1.0 and out.week_extra == 1.0:
        return None
    return out


def goalie_news_share(share: float, news_signals: Iterable[Any], as_of: date | None) -> tuple[float, Reason | None]:
    """(start share after job news, START_NEWS reason or None) - module docstring."""
    from .rookie import _fmt_signal, usable_news

    news = [d for d in usable_news(news_signals, as_of, max_age_days=START_NEWS_MAX_AGE_DAYS)
            if (d["kind"], d["direction"]) in START_NEWS_TARGET]
    if not news:
        return share, None
    d = news[0]
    target = START_NEWS_TARGET[(d["kind"], d["direction"])]
    if (d["kind"] == "goalie_role" and (target - share) * d["direction"] <= 0) or abs(target - share) < 1e-9:
        return share, None                    # already past the target in the news' direction
    w = START_NEWS_WEIGHT * float(d["confidence"])
    new = share + w * (target - share)
    label = {("goalie_role", 1): "won the job", ("goalie_role", -1): "lost the job",
             ("goalie_split", 1): "tandem"}[(d["kind"], d["direction"])]
    return new, Reason(code="START_NEWS",
                       text=f"Goalie news ({label}): {_fmt_signal(d)}; start share {share:.0%} -> {new:.0%} "
                            f"({w:.0%} of the way to {target:.0%})", value=round(new, 4), baseline=round(share, 4))


__all__ = ["ROLE_BOOST", "ROLE_MINOR_BOOST", "DEPTH_PENALTY", "SCRATCH_MULT", "RoleChange", "role_multiplier",
           "role_change", "goalie_news_share"]
