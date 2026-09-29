"""League-wide valuation: rates -> FPG -> availability -> schedule -> replacement -> VORP.

Field semantics of :class:`PlayerValue`:

* ``fpg`` - healthy fantasy points per game from shrunk + recency-blended rates.
* ``fpg_season`` - ``fpg`` x rest-of-season availability.
* ``fpg_week`` - per-game value for the coming week. Without a schedule it is ``fpg`` x week
  availability. When ``ctx.schedule`` is loaded it becomes ``proj_week / avg_team_games``:
  still in FPG units (comparable with replacement levels and the waiver thresholds) but
  scaled by how many games (and off-night games / goalie starts) the player gets relative
  to an average NHL team in the same window.
* ``proj_week`` - projected fantasy points over the 7-day window (None without a schedule):
  ``fpg * avail_week * games * (1 + 0.05 * offnight) * start_share``.

In-season only (never the preseason multi-season baseline, where the backtest found no gain):
once a skater has ``XG_MIN_GP`` (5) GP this season and MoneyPuck's ixG describes this season
(``Player.xg_split == "season"``), his season-to-date goal rate is shrunk toward ixG/GP with
``regression.shrink_goals`` (k = 15 games) before the baseline shrink and recency blend
(reason XG_SHRINK; -1.5% MAE on single-season rates in ``backtest.xg_backtest``).

Harness ledger inputs (opt-in, filled by the enrich deployment step only when a team has >= 5
games in the ledger): ``ctx.goalie_actual_starts`` feeds ``start_share(actual=...)`` for goalies
without a projection (START_ACTUAL) and ``ctx.b2b_second_night`` feeds
``schedule_factor(b2b_p=...)`` for the week window (B2B).
"""
from __future__ import annotations

from datetime import date
from typing import Any, Mapping

from pydantic import BaseModel, Field

from . import params as _params

from ..models import LeagueContext, Player, Reason, StatLine
from ..scoring import CategoriesScoring, ScoringSystem, fit_to_context
from .adjust import Horizon, availability_multiplier
from ..matching.normalize import normalize_team
from .blend import (PRIOR_SPLITS, baseline_k, blend_rates, blend_recency, multi_season_baseline,
                    multi_season_sample, projection_blend_weight, projection_k, recency_weights, shrink_baseline,
                    shrink_k, shrink_toward)
from .params import age_factor
from .replacement import DEFAULT_SLOTS, ROSTERED_FALLBACK_PCT, best_slot, replacement_with_fallback, vorp
from .regression import K_GOALS, shrink_goals
from .schedule import context_window, proj_week, schedule_factor, start_share, start_share_parts

PRIOR_TEAM_GAMES = 82
PROJ_SHARE_K = 20
MIN_GP_FOR_MEAN = {"F": 20, "D": 20, "G": 10}
XG_MIN_GP = 5                # season GP before the xG goal shrink applies (regression.MIN_CURRENT_GP)
XG_CREDIT = "ixG: MoneyPuck.com"
MIN_LEDGER_TEAM_GAMES = 5    # ledger team games before actual starts / back-to-back rates are used


class PlayerValue(BaseModel):
    player: Player
    fpg: float                       # healthy per-game value (blended rates)
    fpg_season: float                # fpg * season availability
    fpg_week: float                  # week per-game value (see module docstring)
    vorp: float                      # season horizon
    vorp_week: float = 0.0
    proj_week: float | None = None   # projected points over the 7-day window
    games_next7: int | None = None
    offnight_next7: int | None = None
    start_share: float | None = None  # goalies only
    # goalies: where the share came from ("projection": projected workload, "history": shrunk
    # GS / team GP with the start-share prior / k) and, for "history", its starts and team games
    # (excluded from dumps: only the archive's refit inputs read them)
    share_source: str | None = Field(default=None, exclude=True)
    share_starts: float | None = Field(default=None, exclude=True)
    share_team_games: float | None = Field(default=None, exclude=True)
    horizon_values: dict[str, float] = Field(default_factory=dict)
    rates: dict[str, float] = Field(default_factory=dict)
    reasons: list[Reason] = Field(default_factory=list)

    def fpg_for(self, horizon: Horizon) -> float:
        return self.fpg_week if horizon == "week" else self.fpg_season

    def vorp_for(self, horizon: Horizon) -> float:
        return self.vorp_week if horizon == "week" else self.vorp

    def lineup_value(self, horizon: Horizon) -> float:
        """Value used by the lineup solver: projected week points when a schedule is known."""
        if horizon == "week":
            return self.proj_week if self.proj_week is not None else self.fpg_week
        return self.fpg_season


def position_group(p: Player) -> str:
    if p.is_goalie:
        return "G"
    if "D" in p.positions and not set(p.positions) & {"C", "LW", "RW", "F"}:
        return "D"
    return "F"


def positional_means(players: list[Player]) -> dict[str, dict[str, float]]:
    """Mean prior-season per-game rates by position group (F/D/G)."""
    sums: dict[str, dict[str, float]] = {}
    counts: dict[str, int] = {}
    for p in players:
        prior = p.lines.get("prior")
        g = position_group(p)
        if not prior or prior.gp < MIN_GP_FOR_MEAN[g]:
            continue
        counts[g] = counts.get(g, 0) + 1
        acc = sums.setdefault(g, {})
        for k, v in prior.per_game().items():
            acc[k] = acc.get(k, 0.0) + v
    return {g: {k: v / counts[g] for k, v in acc.items()} for g, acc in sums.items()}


def baseline_line(p: Player) -> StatLine | None:
    """ESPN projection if present, else the prior season (None when neither has games)."""
    proj = p.lines.get("projected")
    if proj and proj.gp > 0:
        return proj
    prior = p.lines.get("prior")
    return prior if prior and prior.gp > 0 else None


def season_start_year(as_of: date) -> int:
    """Start year of the current NHL season (from September on, the upcoming one)."""
    return as_of.year if as_of.month >= 9 else as_of.year - 1


def season_age(p: Player, as_of: date) -> float | None:
    """Age on Oct 1 of the current / upcoming season (the backtest's age convention)."""
    if p.birth_date is None:
        return None
    return round((date(season_start_year(as_of), 10, 1) - p.birth_date).days / 365.25, 3)


def history_lines(p: Player) -> list[StatLine | None]:
    """The player's NHL seasons N-1, N-2, N-3 (``prior``, ``prior2``, ``prior3``)."""
    return [p.lines.get(s) for s in PRIOR_SPLITS]


def history_baseline(p: Player, means: dict[str, dict[str, float]], age: float | None = None
                     ) -> tuple[dict[str, float], int, str]:
    """(rates, prior NHL GP, description) of the multi-season baseline (the backtest's
    ``fm_multi``): seasons N-1..N-3 weighted 5/4/3 by GP, shrunk toward the positional mean with
    ``K_BASELINE[group]``, times the fitted year-over-year age factor from last season (age on
    Oct 1 minus one) to this one. ({}, 0, "") without any prior NHL games."""
    g = position_group(p)
    lines = history_lines(p)
    sample = multi_season_sample(lines)
    if sample is None:
        return {}, 0, ""
    _, n, gp_total = sample
    rates = multi_season_baseline(lines, g, means)
    seasons = sum(1 for ln in lines if ln is not None and ln.gp > 0)
    desc = f"{gp_total} GP over {seasons} season{'s' if seasons != 1 else ''}"
    if seasons > 1:
        desc += " weighted 5/4/3"
    if means.get(g):
        k = baseline_k(g)
        desc += f", shrunk {k / (n + k):.0%} toward {g} mean (k={k:g})"
    age_prev = None if age is None else age - 1.0
    f = age_factor(g, age_prev)
    if abs(f - 1.0) > 1e-9:
        rates = {key: v * f for key, v in rates.items()}
        a0 = int(age_prev)  # type: ignore[arg-type]
        desc += f", aging {a0}->{a0 + 1} x{f:.3f}"
    return rates, gp_total, desc


def baseline_rates(p: Player, means: dict[str, dict[str, float]], age: float | None = None,
                   params: Mapping[str, Any] | None = None,
                   history: tuple[dict[str, float], int, str] | None = None
                   ) -> tuple[dict[str, float], StatLine | None, float, str]:
    """(rates, raw baseline line, effective GP, source) of the pre-season baseline.

    * No league projection: the multi-season NHL history (``history_baseline``).
    * League projection (shrunk toward the positional mean with ``K_PROJECTION``, counted as a
      full season unless it projects < 20 GP) blended with the history:
      ``0.6 * projection + 0.4 * history`` once the player has >= 40 prior NHL GP, projection
      only for a player without NHL history (rookies), linear in between.
    * Neither: a player with current-season games uses the group mean itself.

    ``age`` is the age on Oct 1 of the season being projected (``season_age``). ``params`` (a
    full merged params mapping, default the loaded ones) and a precomputed ``history`` (rates,
    GP, description) let the harness replay this exact computation from archived inputs."""
    g = position_group(p)
    mean = means.get(g, {})
    hist, hist_gp, hist_desc = history_baseline(p, means, age) if history is None else history
    proj = p.lines.get("projected")
    if proj is not None and proj.gp > 0:
        prates, n = shrink_baseline(proj, mean, p.is_goalie, params=params)
        src = f"projection ({proj.gp} GP"
        if n != proj.gp:
            src += f", counted as {n:.0f}"
        src += ")"
        if mean:
            k = projection_k(p.is_goalie, params)
            src += f" shrunk {k / (n + k):.0%} toward {g} mean (k={k:g})"
        if hist:
            w = projection_blend_weight(hist_gp, params)
            return blend_rates(prates, hist, w), proj, n, f"{w:.0%} {src} + {1 - w:.0%} NHL history ({hist_desc})"
        return prates, proj, n, src
    if hist:
        if history is not None:
            return hist, None, float(hist_gp), f"NHL history ({hist_desc})"
        line = next((ln for ln in history_lines(p) if ln is not None and ln.gp > 0), None)
        n = multi_season_sample(history_lines(p))[1]  # type: ignore[index]
        return hist, line, n, f"NHL history ({hist_desc})"
    # only a player with current-season games borrows the mean as his prior; one with
    # no stats at all stays at NO_DATA rather than being handed an average line
    if mean and p.gp("season") > 0:
        return dict(mean), None, 0.0, f"{g} positional mean (no baseline line)"
    return {}, None, 0.0, ""


def player_rates(p: Player, means: dict[str, dict[str, float]], age: float | None = None,
                 params: Mapping[str, Any] | None = None,
                 history: tuple[dict[str, float], int, str] | None = None) -> dict[str, float]:
    """The healthy per-game rates behind ``PlayerValue.fpg`` (baseline -> season-to-date shrink ->
    recency blend), without reasons. Pure: the harness replays archived inputs through it with
    candidate ``params`` (``harness.refit.inseason_projection``), so the two cannot drift."""
    return _rates_for(p, means, None, age, params=params, history=history, explain=False)[0]


def _rates_for(p: Player, means: dict[str, dict[str, float]], scoring: ScoringSystem | None,
               age: float | None = None, params: Mapping[str, Any] | None = None,
               history: tuple[dict[str, float], int, str] | None = None,
               explain: bool = True) -> tuple[dict[str, float], list[Reason]]:
    reasons: list[Reason] = []
    season = p.lines.get("season")
    base_rates, base_line, _, base_src = baseline_rates(p, means, age, params=params, history=history)
    gp = season.gp if season else 0
    k = shrink_k(p.is_goalie, params)
    xg: tuple[float, float, float] | None = None
    if gp > 0:
        season_rates = season.per_game()
        if xg_applies(p, gp) and "G" in season_rates:
            before = season_rates["G"]
            season_rates = shrink_goals(season_rates, p.ixg_per_game, gp, K_GOALS)
            xg = (before, float(p.ixg_per_game), season_rates["G"])  # type: ignore[arg-type]
        shrunk = shrink_toward(season_rates, gp, base_rates, k)
    else:
        shrunk = dict(base_rates)
    l30, l15, l7 = (p.lines.get(s) for s in ("last30", "last15", "last7"))
    rates = blend_recency(shrunk, l30, l15, l7, params=params)
    if not explain or scoring is None:
        return rates, reasons
    if base_rates:
        base_fpg = scoring.value(base_rates)
        raw = f" (raw {scoring.value(base_line.per_game()):.2f})" if base_line is not None else ""
        reasons.append(Reason(code="BASELINE", text=f"Baseline from {base_src}: {base_fpg:.2f} FPG{raw}",
                              value=base_fpg))
    if xg is not None:
        g_rate, ixg_rate, new_g = xg
        reasons.append(Reason(code="XG_SHRINK",
                              text=f"Goals {g_rate:.2f}/GP vs ixG {ixg_rate:.2f}/GP over {gp} GP this season: goal "
                                   f"rate shrunk to {new_g:.2f}/GP (k={K_GOALS:g} games; {XG_CREDIT})",
                              value=g_rate, baseline=ixg_rate))
    if gp > 0:
        w = gp / (gp + k) if base_rates else 1.0
        reasons.append(Reason(code="SHRINK_GP",
                              text=f"{gp} GP this season carry {w:.0%} weight vs baseline (k={k:g})",
                              value=float(gp), baseline=float(k)))
    w = recency_weights(*(ln.gp if ln else 0 for ln in (l30, l15, l7)), params=params)
    if w["season"] < 0.999:
        reasons.append(Reason(code="RECENCY",
                              text="Recency weights season {season:.2f} / L30 {last30:.2f} / L15 {last15:.2f} / L7 {last7:.2f}"
                                   " (recent form is weighted lightly: in the backtest it barely predicts the rest of "
                                   "the season)".format(**w),
                              value=1.0 - w["season"]))
    if not rates:
        reasons.append(Reason(code="NO_DATA", text="No current, projected or prior stats available"))
    return rates, reasons


def xg_applies(p: Player, gp: int | None = None) -> bool:
    """True when the in-season xG goal shrink applies: a skater with >= XG_MIN_GP games this
    season whose MoneyPuck ixG describes this season (``xg_split`` "season"; when unknown, the
    ``xg_enrich`` rule: this season once it has XG_MIN_GP games). Never the prior-season baseline."""
    if p.is_goalie or p.ixg_per_game is None:
        return False
    gp = p.gp("season") if gp is None else gp
    if gp < XG_MIN_GP:
        return False
    return p.xg_split in (None, "season")


def team_games_played(ctx: LeagueContext) -> dict[str, int]:
    """Current-season games played per NHL team: scheduled dates before as_of, or (when that
    is lower, e.g. no schedule) the most season GP of any player on that team."""
    out: dict[str, int] = {}
    for team, dates in ctx.schedule.items():
        out[team] = sum(1 for d in set(dates) if d < ctx.as_of)
    for p in ctx.all_players():
        team = normalize_team(p.team)
        if team:
            out[team] = max(out.get(team, 0), p.gp("season"))
    return out


def goalie_team_games(p: Player, team_gp: Mapping[str, int]) -> dict[str, float]:
    """Start-share denominators per split: this season's team GP, a full season otherwise."""
    team = normalize_team(p.team)
    out: dict[str, float] = {"prior": float(PRIOR_TEAM_GAMES), "projected": float(PRIOR_TEAM_GAMES)}
    if team and team in team_gp:
        out["season"] = float(team_gp[team])
    return out


def projected_workload(p: Player, team_gp: Mapping[str, int] | None = None
                       ) -> tuple[float, float | None, str] | None:
    """Goalie workload from a full-season projection: (start share, projected fantasy points
    per team game or None, reason text), or None without a usable projection.

    share = projected GS (else GP) / 82. Once the season has started, this season's starts
    are blended in: (GS_season + k*share_proj) / (team GP + k) with k = PROJ_SHARE_K. The
    per-team-game value is the projection's own FPts / 82 (Fantrax), only before the
    goalie's season has started (otherwise the blended per-game rate x share is used)."""
    if not p.is_goalie:
        return None
    proj = p.lines.get("projected")
    if proj is None or proj.gp <= 0:
        return None
    starts = float(proj.stats.get("GS", proj.gp))
    share = min(1.0, starts / PRIOR_TEAM_GAMES)
    text = f"Projected start share {share:.0%} ({starts:.0f} projected starts / {PRIOR_TEAM_GAMES})"
    season = p.lines.get("season")
    team = normalize_team(p.team)
    tg = float((team_gp or {}).get(team, 0)) if team else 0.0
    if season is not None and season.gp > 0:
        gs = float(season.stats.get("GS", season.gp))
        tg = max(tg, float(season.gp))
        blended = (gs + PROJ_SHARE_K * share) / (tg + PROJ_SHARE_K)
        text += f", blended with {gs:.0f} GS / {tg:.0f} team GP this season (k={PROJ_SHARE_K}) -> {blended:.0%}"
        return blended, None, text
    fpts = proj.stats.get("FPTS")
    per_team_game = float(fpts) / PRIOR_TEAM_GAMES if fpts is not None else None
    if per_team_game is not None:
        text += f"; projected {fpts:.0f} FPts / {PRIOR_TEAM_GAMES} team games"
    return share, per_team_game, text


def _slots(ctx: LeagueContext) -> list[str]:
    return [s for s in ("C", "LW", "RW", "F", "D", "G") if s in ctx.roster_shape] or list(DEFAULT_SLOTS)


def _replacement(ctx: LeagueContext, values: Mapping[str, "PlayerValue"], horizon: Horizon
                 ) -> tuple[dict[str, float], set[str]]:
    fa_ids = {p.cid for p in ctx.free_agents}
    fa = {c: values[c].fpg_for(horizon) for c in fa_ids if c in values}
    rostered = {p.cid: values[p.cid].fpg_for(horizon) for t in ctx.teams for p in t.players
                if p.cid in values and p.cid not in fa_ids}
    players = {c: pv.player for c, pv in values.items()}
    return replacement_with_fallback(fa, rostered, players, _slots(ctx))


def valuate_league(ctx: LeagueContext, scoring: ScoringSystem) -> dict[str, PlayerValue]:
    """Value every rostered player and free agent in the league.

    Categories/roto scoring is relative (z-scores), so an unfitted system is fitted on the
    league's rostered players first; otherwise every value would be 0.
    """
    if isinstance(scoring, CategoriesScoring) and not scoring.fitted:
        fit_to_context(scoring, ctx)
    players = ctx.all_players()
    means = positional_means(players)

    team_gp = team_games_played(ctx)
    window = context_window(ctx)
    if window is not None and window.avg_team_games <= 0:
        window = None
    partial: dict[str, PlayerValue] = {}
    for p in players:
        rates, reasons = _rates_for(p, means, scoring, season_age(p, ctx.as_of))
        fpg = scoring.value(rates) if rates else 0.0
        a_season = availability_multiplier(p.status, "season")
        a_week = availability_multiplier(p.status, "week")
        season = p.lines.get("season")
        raw = f", season raw {scoring.value(season.per_game()):.2f}" if season and season.gp else ""
        reasons.insert(0, Reason(code="FPG_BLEND", text=f"Blended {fpg:.2f} FPG{raw}", value=fpg))
        if p.status not in ("healthy", "unknown"):
            reasons.append(Reason(code="STATUS",
                                  text=f"Status {p.status}{' (' + p.status_note + ')' if p.status_note else ''}: "
                                       f"x{a_week:.2f} this week, x{a_season:.2f} rest of season",
                                  value=a_season, baseline=1.0))
        share = 1.0
        season_value: float | None = None
        share_parts: dict[str, Any] = {}
        if p.is_goalie:
            ss_prior, ss_k = _params.start_share_prior(), _params.start_share_k()
            work = projected_workload(p, team_gp)
            if work is not None:
                share, per_team_game, text = work
                season_value = (per_team_game if per_team_game is not None else fpg * share) * a_season
                share_parts = {"share_source": "projection"}
                reasons.append(Reason(code="START_SHARE",
                                      text=f"{text}: season value {fpg * a_season:.2f} -> {season_value:.2f} FPG",
                                      value=share, baseline=ss_prior))
            else:
                share, gs, tg = start_share_parts(p, team_games=goalie_team_games(p, team_gp))
                actual = ctx.goalie_actual_starts.get(p.cid)
                if actual is not None and actual[1] >= MIN_LEDGER_TEAM_GAMES:
                    line_share = share
                    share = start_share(p, team_games=goalie_team_games(p, team_gp), actual=actual)
                    share_parts = {"share_source": "actual"}
                    reasons.append(Reason(code="START_ACTUAL",
                                          text=f"Start share {share:.0%} from {actual[0]} starts in {actual[1]} team "
                                               f"games this season (ledger), shrunk toward his prior share (k=10; "
                                               f"stat-line estimate {line_share:.0%}): season value "
                                               f"{fpg * a_season:.2f} -> {fpg * a_season * share:.2f} FPG",
                                          value=share, baseline=line_share))
                else:
                    share_parts = {"share_source": "history", "share_starts": gs, "share_team_games": tg}
                    reasons.append(Reason(code="START_SHARE",
                                          text=f"Projected start share {share:.0%} ({gs:.0f} GS / {tg:.0f} team GP, "
                                               f"k={ss_k:g} toward {ss_prior:.0%}): "
                                               f"season value {fpg * a_season:.2f} -> {fpg * a_season * share:.2f} FPG",
                                          value=share, baseline=ss_prior))
        pv = PlayerValue(player=p, fpg=fpg,
                         fpg_season=season_value if season_value is not None else fpg * a_season * share,
                         fpg_week=fpg * a_week * share, vorp=0.0, rates=rates, reasons=reasons,
                         start_share=share if p.is_goalie else None, **share_parts)
        b2b = ctx.b2b_second_night.get(normalize_team(p.team) or "") if p.is_goalie else None
        sf = schedule_factor(p, ctx.schedule, ctx.games_per_day, window, share=share,
                             b2b_p=b2b[0] if b2b else None) if window else None
        if sf is not None and b2b is not None and abs(sf.start_share - share) > 1e-4:
            seen = f"{b2b[1]} seen" + ("" if b2b[1] >= 5 else ", league prior until 5")
            pv.reasons.append(Reason(code="B2B",
                                     text=f"Back-to-back second nights {window.label()}: the #1 starts them with "
                                          f"p={b2b[0]:.2f} ({seen}); week start share {share:.0%} -> "
                                          f"{sf.start_share:.0%}", value=sf.start_share, baseline=share))
        if sf is not None:
            pw = proj_week(fpg, a_week, sf.games, sf.offnight) * sf.start_share
            pv.proj_week, pv.games_next7, pv.offnight_next7 = pw, sf.games, sf.offnight
            pv.fpg_week = pw / window.avg_team_games
            pre = " (preseason: first 7 days from opening night)" if window.preseason else ""
            pv.reasons.append(Reason(code="GAMES_NEXT7",
                                  text=f"{sf.games} games {window.label()}{pre}, {sf.offnight} on off-nights"
                                       f" (avg team {window.avg_team_games:.1f})",
                                  value=float(sf.games), baseline=window.avg_team_games))
            pv.reasons.append(Reason(code="PROJ_WEEK", text=f"Projected {pw:.1f} pts over {window.label()}",
                                  value=pw))
        partial[p.cid] = pv

    repl: dict[str, dict[str, float]] = {}
    fallback: set[str] = set()
    for h in ("season", "week"):
        repl[h], fb = _replacement(ctx, partial, h)
        fallback |= fb
    for pv in partial.values():
        pos = pv.player.positions
        pv.vorp = vorp(pv.fpg_season, pos, repl["season"])
        pv.vorp_week = vorp(pv.fpg_week, pos, repl["week"])
        slot = best_slot(pos, repl["season"])
        pv.horizon_values = {"season": pv.fpg_season, "week": pv.fpg_week,
                             "vorp_season": pv.vorp, "vorp_week": pv.vorp_week}
        if pv.proj_week is not None:
            pv.horizon_values["proj_week"] = pv.proj_week
        if slot is not None:
            pv.reasons.append(Reason(code="VORP",
                                     text=f"{pv.vorp:+.2f} FPG over {slot} replacement ({repl['season'][slot]:.2f})",
                                     value=pv.vorp, baseline=repl["season"][slot]))
            if slot in fallback:
                pv.reasons.append(Reason(
                    code="REPL_FALLBACK",
                    text=f"No free agents at {slot}: replacement is the rostered "
                         f"{int(ROSTERED_FALLBACK_PCT * 100)}th percentile ({repl['season'][slot]:.2f})",
                    value=repl["season"][slot]))
    return partial


def replacement_for(ctx: LeagueContext, values: Mapping[str, PlayerValue], horizon: Horizon = "season"
                    ) -> dict[str, float]:
    """Replacement levels (for display): top-3 free-agent mean per slot, or the rostered
    10th percentile for a slot with no free agents at all."""
    return _replacement(ctx, values, horizon)[0]


def replacement_fallback_slots(ctx: LeagueContext, values: Mapping[str, PlayerValue],
                               horizon: Horizon = "season") -> set[str]:
    """Slots whose replacement level came from the rostered-percentile fallback."""
    return _replacement(ctx, values, horizon)[1]
