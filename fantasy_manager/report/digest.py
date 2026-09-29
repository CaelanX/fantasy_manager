"""Daily digest: markdown + self-contained HTML + short plain-text summary for chat webhooks."""
from __future__ import annotations

import html
from dataclasses import dataclass
from datetime import date, datetime
from pathlib import Path
from typing import Any, Mapping

from ..models import LeagueContext, Player, Recommendation
from ..providers.news import NewsItem
from .news_match import player_news_summary

SUMMARY_MAX = 1500
ALERT_STATUSES = ("dtd", "out", "ir", "ltir", "suspended")
STATUS_LABEL = {"dtd": "day-to-day", "out": "out", "ir": "IR", "ltir": "LTIR", "suspended": "suspended"}

# (section title, rec kinds) in display order after the headline and injury sections
REC_SECTIONS: list[tuple[str, tuple[str, ...]]] = [
    ("Lineup", ("lineup",)),
    ("Waivers", ("waiver",)),
    ("Trades", ("trade",)),
    ("Flags", ("sell_high", "buy_low")),
]
KIND_LABEL = {"lineup": "Lineup", "waiver": "Waiver", "trade": "Trade", "sell_high": "Sell high",
              "buy_low": "Buy low", "injury": "Injury"}


@dataclass
class Digest:
    markdown: str
    html: str
    summary: str
    generated_at: datetime | None = None

    @property
    def day(self) -> date:
        return (self.generated_at or datetime.now()).date()


# --------------------------------------------------------------------------- data gathering

@dataclass
class _Alert:
    player: Player
    slot: str | None
    fpg: float | None
    rec: Recommendation | None = None


def _ranked(recs: list[Recommendation]) -> list[Recommendation]:
    return sorted(recs, key=lambda r: -r.score)


def _fpg(values: Mapping[str, Any], cid: str) -> float | None:
    v = values.get(cid)
    x = getattr(v, "fpg", None) if v is not None else None
    return float(x) if isinstance(x, (int, float)) else None


def _injury_alerts(ctx: LeagueContext, values: Mapping[str, Any], recs: list[Recommendation]
                   ) -> tuple[list[Recommendation], list[_Alert]]:
    """Injury recs, plus rostered players with a non-healthy status not covered by one."""
    inj_recs = _ranked([r for r in recs if r.kind == "injury"])
    covered = {p.cid for r in inj_recs for p in (*r.add, *r.drop)}
    alerts = []
    for s in ctx.my_team.slots:
        p = s.player
        if p is not None and p.status in ALERT_STATUSES and p.cid not in covered:
            alerts.append(_Alert(p, s.slot, _fpg(values, p.cid)))
    alerts.sort(key=lambda a: (-(a.fpg or 0.0), a.player.name))
    return inj_recs, alerts


def _news_for_me(ctx: LeagueContext, news_by_cid: Mapping[str, list[NewsItem]]
                 ) -> list[tuple[Player, list[NewsItem]]]:
    rows = [(p, list(news_by_cid.get(p.cid, []))) for p in ctx.my_team.players]
    return sorted([(p, items) for p, items in rows if items], key=lambda t: t[0].name)


def _alert_text(a: _Alert) -> str:
    bits = [f"{a.player.name} ({'/'.join(a.player.positions)}, {a.player.team or 'FA'})",
            STATUS_LABEL.get(a.player.status, a.player.status)]
    if a.player.status_note:
        bits[-1] += f": {a.player.status_note}"
    extra = []
    if a.slot:
        extra.append(f"slot {a.slot}")
    if a.fpg is not None:
        extra.append(f"{a.fpg:.2f} FPG healthy")
    return " - ".join(bits) + (f" ({', '.join(extra)})" if extra else "")


# --------------------------------------------------------------------------- markdown

def _md_rec(r: Recommendation, level: str = "###") -> list[str]:
    cp = f" (with {r.counterparty})" if r.counterparty else ""
    out = [f"{level} {r.title}{cp}", "", f"*{KIND_LABEL.get(r.kind, r.kind)} - score {r.score:.2f}*", ""]
    if r.narrative:
        out += [f"> {r.narrative}", ""]
    out += [f"- {reason.text}" for reason in r.reasons]
    return out + [""]


def _md_headline(r: Recommendation, i: int) -> str:
    why = r.narrative or (r.reasons[0].text if r.reasons else "")
    return f"{i}. **{r.title}** ({KIND_LABEL.get(r.kind, r.kind)}, score {r.score:.2f})" + (f" - {why}" if why else "")


def _model_line(model: str | None) -> str | None:
    """The harness headline ("Model: ..."), or None while nothing is trustworthy."""
    if not model:
        return None
    model = model.strip()
    return model if model.startswith("Model:") else f"Model: {model}"


def _markdown(ctx: LeagueContext, ranked: list[Recommendation], inj_recs, alerts, news_rows,
              generated_at: datetime, model: str | None = None) -> str:
    md = [f"# Fantasy digest - {ctx.name}", "",
          f"*{ctx.provider} league {ctx.league_id} - team {ctx.my_team.name} - "
          f"generated {generated_at:%Y-%m-%d %H:%M}*", ""]
    if _model_line(model):
        md += [f"*{_model_line(model)}*", ""]
    md += ["## Headline", ""]
    md += [_md_headline(r, i) for i, r in enumerate(ranked[:3], 1)] or ["No moves recommended today."]
    md += ["", "## Injury alerts", ""]
    if not inj_recs and not alerts:
        md += ["_No injury concerns on your roster._", ""]
    for r in inj_recs:
        md += _md_rec(r)
    if alerts:
        md += [f"- {_alert_text(a)}" for a in alerts] + [""]
    for title, kinds in REC_SECTIONS:
        md += [f"## {title}", ""]
        items = [r for r in ranked if r.kind in kinds]
        if not items:
            md += ["_None today._", ""]
        for r in items:
            md += _md_rec(r)
    md += ["## News for my players", ""]
    if not news_rows:
        md += ["_No recent news for your players._", ""]
    for p, items in news_rows:
        md += [f"**{p.name}**", ""]
        md += [f"- {line}" for line in player_news_summary(items, limit=3)]
        urls = [n.url for n in items[:3] if n.url]
        if urls:
            md += [f"  - source: <{urls[0]}>"]
        md += [""]
    return "\n".join(md).rstrip() + "\n"


# --------------------------------------------------------------------------- html

_CSS = """
:root{--bg:#f7f7f8;--card:#fff;--fg:#1b1d21;--muted:#5f6670;--accent:#1a5fb4;--border:#dcdfe4;
--warn:#b54708;--warn-bg:#fff4e5}
@media (prefers-color-scheme: dark){:root{--bg:#121417;--card:#1c1f24;--fg:#e8eaed;--muted:#9aa1ab;
--accent:#78aeed;--border:#2e333a;--warn:#f5a454;--warn-bg:#2a2016}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);
font:16px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}
main{max-width:720px;margin:0 auto;padding:16px}
h1{font-size:1.4rem;margin:.2em 0}
h2{font-size:1.1rem;margin:1.6em 0 .6em;padding-bottom:.25em;border-bottom:1px solid var(--border)}
.meta,.empty,.score{color:var(--muted);font-size:.9rem}
.card{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:12px 14px;margin:10px 0}
.card h3{font-size:1rem;margin:0 0 4px}
.narr{margin:6px 0;font-style:italic}
ul{margin:6px 0;padding-left:1.2em}
li{margin:2px 0}
.tag{display:inline-block;font-size:.75rem;padding:1px 8px;border-radius:999px;border:1px solid var(--border);
color:var(--muted);margin-right:6px}
.alert{background:var(--warn-bg);border-color:var(--warn)}
.alert strong{color:var(--warn)}
ol.head{padding-left:1.4em}
ol.head li{margin:6px 0}
"""


def _e(s: Any) -> str:
    return html.escape(str(s), quote=True)


def _html_rec(r: Recommendation, cls: str = "card") -> str:
    cp = f" <span class=\"score\">with {_e(r.counterparty)}</span>" if r.counterparty else ""
    parts = [f'<div class="{cls}">',
             f'<h3>{_e(r.title)}{cp}</h3>',
             f'<div class="score"><span class="tag">{_e(KIND_LABEL.get(r.kind, r.kind))}</span>'
             f'score {r.score:.2f}</div>']
    if r.narrative:
        parts.append(f'<p class="narr">{_e(r.narrative)}</p>')
    if r.reasons:
        parts.append("<ul>" + "".join(f"<li>{_e(x.text)}</li>" for x in r.reasons) + "</ul>")
    parts.append("</div>")
    return "".join(parts)


def _html(ctx: LeagueContext, ranked: list[Recommendation], inj_recs, alerts, news_rows,
          generated_at: datetime, model: str | None = None) -> str:
    b: list[str] = [
        f"<h1>Fantasy digest - {_e(ctx.name)}</h1>",
        f'<div class="meta">{_e(ctx.provider)} league {_e(ctx.league_id)} - team {_e(ctx.my_team.name)}'
        f" - generated {generated_at:%Y-%m-%d %H:%M}</div>",
    ]
    if _model_line(model):
        b.append(f'<div class="meta model">{_e(_model_line(model))}</div>')
    b.append("<h2>Headline</h2>")
    if ranked:
        b.append('<ol class="head">')
        for r in ranked[:3]:
            why = r.narrative or (r.reasons[0].text if r.reasons else "")
            b.append(f"<li><strong>{_e(r.title)}</strong> <span class=\"score\">"
                     f"({_e(KIND_LABEL.get(r.kind, r.kind))}, score {r.score:.2f})</span>"
                     + (f"<br>{_e(why)}" if why else "") + "</li>")
        b.append("</ol>")
    else:
        b.append('<p class="empty">No moves recommended today.</p>')
    b.append("<h2>Injury alerts</h2>")
    if not inj_recs and not alerts:
        b.append('<p class="empty">No injury concerns on your roster.</p>')
    b += [_html_rec(r, "card alert") for r in inj_recs]
    if alerts:
        b.append('<div class="card alert"><ul>' +
                 "".join(f"<li>{_e(_alert_text(a))}</li>" for a in alerts) + "</ul></div>")
    for title, kinds in REC_SECTIONS:
        b.append(f"<h2>{_e(title)}</h2>")
        items = [r for r in ranked if r.kind in kinds]
        b += [_html_rec(r) for r in items] or ['<p class="empty">None today.</p>']
    b.append("<h2>News for my players</h2>")
    if not news_rows:
        b.append('<p class="empty">No recent news for your players.</p>')
    for p, items in news_rows:
        b.append(f'<div class="card"><h3>{_e(p.name)}</h3><ul>' +
                 "".join(f"<li>{_e(line)}</li>" for line in player_news_summary(items, limit=3)) +
                 "</ul></div>")
    title = f"Fantasy digest - {ctx.name} - {generated_at:%Y-%m-%d}"
    return ("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">"
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
            "<meta name=\"color-scheme\" content=\"light dark\">"
            f"<title>{_e(title)}</title><style>{_CSS}</style></head>"
            "<body><main>\n" + "\n".join(b) + "\n</main></body></html>\n")


# --------------------------------------------------------------------------- summary

def _summary(ctx: LeagueContext, ranked: list[Recommendation], inj_recs, alerts, news_rows,
             generated_at: datetime, model: str | None = None) -> str:
    lines = [f"Fantasy digest: {ctx.name} ({generated_at:%Y-%m-%d})"]
    if _model_line(model):
        lines.append(_model_line(model))
    if ranked:
        lines.append("Top moves:")
        for i, r in enumerate(ranked[:3], 1):
            why = r.narrative or (r.reasons[0].text if r.reasons else "")
            lines.append(f"{i}. {r.title} (score {r.score:.2f})" + (f" - {why}" if why else ""))
    else:
        lines.append("No moves recommended today.")
    injured = [r.title for r in inj_recs] + [
        f"{a.player.name} ({STATUS_LABEL.get(a.player.status, a.player.status)})" for a in alerts]
    if injured:
        lines.append(f"Injury alerts ({len(injured)}): " + "; ".join(dict.fromkeys(injured)))
    counts = [f"{title} {sum(r.kind in kinds for r in ranked)}" for title, kinds in REC_SECTIONS]
    lines.append("Counts: " + " | ".join(counts))
    if news_rows:
        lines.append("News: " + ", ".join(p.name for p, _ in news_rows))
    text = "\n".join(lines)
    if len(text) > SUMMARY_MAX:
        text = text[: SUMMARY_MAX - 1].rstrip() + "…"
    return text


# --------------------------------------------------------------------------- public API

def build_digest(ctx: LeagueContext, values: Mapping[str, Any], recs: list[Recommendation],
                 news_by_cid: Mapping[str, list[NewsItem]] | None,
                 generated_at: datetime | None = None, model_headline: str | None = None) -> Digest:
    """Render the daily digest. ``values`` maps cid -> PlayerValue (only ``fpg`` is read).
    ``model_headline`` is the harness line (``harness.metrics.headline``); None hides it."""
    generated_at = generated_at or datetime.now()
    values = values or {}
    news_by_cid = news_by_cid or {}
    ranked = _ranked(list(recs or []))
    inj_recs, alerts = _injury_alerts(ctx, values, ranked)
    news_rows = _news_for_me(ctx, news_by_cid)
    args = (ctx, ranked, inj_recs, alerts, news_rows, generated_at, model_headline)
    return Digest(markdown=_markdown(*args), html=_html(*args), summary=_summary(*args),
                  generated_at=generated_at)


def model_headline(data_dir: str | Path, league: str) -> str | None:
    """The harness headline for ``league`` from ``<data_dir>/harness.db`` (None when there is no
    ledger yet, nothing is trustworthy, or reading fails: the digest never breaks on it)."""
    try:
        from ..harness.ledger import DB_NAME, Ledger
        from ..harness.metrics import headline

        if not (Path(data_dir) / DB_NAME).exists():
            return None
        with Ledger(data_dir) as led:
            return headline(led, league)
    except Exception:
        return None


def write_digest(digest: Digest, out_dir: str | Path) -> tuple[Path, Path]:
    """Write ``digest-YYYY-MM-DD.md`` and ``.html`` into ``out_dir`` (created if needed)."""
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    stem = f"digest-{digest.day:%Y-%m-%d}"
    md_path, html_path = out / f"{stem}.md", out / f"{stem}.html"
    md_path.write_text(digest.markdown, encoding="utf-8")
    html_path.write_text(digest.html, encoding="utf-8")
    return md_path, html_path


__all__ = ["Digest", "build_digest", "model_headline", "write_digest", "SUMMARY_MAX"]
