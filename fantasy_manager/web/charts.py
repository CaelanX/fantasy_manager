"""Server-rendered inline-SVG sparklines for the /health page (no JavaScript, no chart library).

``line_chart`` draws one small line chart: thin (dashed) baseline lines, one bold main line
with a dot per point (each with a native ``<title>`` tooltip), an optional confidence band
and a zero reference line. Colours come from CSS classes (``.ch-*`` in ``static/style.css``),
so the chart follows the page's light / dark tokens. The SVG has a fixed ``viewBox`` and
scales to the width of its container. Every chart carries ``<title>`` / ``<desc>`` for
assistive tech; the page adds a data-table fallback next to it.
"""
from __future__ import annotations

import math
import re
from dataclasses import dataclass
from typing import Callable, Sequence

from markupsafe import Markup, escape

WIDTH, HEIGHT = 360, 132
PAD_L, PAD_R, PAD_T, PAD_B = 40, 10, 10, 22
DASHES = ("", "5 3", "1.5 2.5", "7 2 1.5 2")       # baseline lines: solid, dashed, dotted, dash-dot

Point = tuple[float, float]


@dataclass
class Line:
    key: str
    label: str
    values: Sequence[float | None]
    role: str = "base"          # main | base
    dash: str = ""


def _finite(v: float | None) -> bool:
    return v is not None and isinstance(v, (int, float)) and math.isfinite(v)


def domain(values: Sequence[float | None], include_zero: bool = False) -> tuple[float, float]:
    """(lo, hi) covering every finite value with 8% headroom; a flat or empty series gets a
    band around its value (0..1 when empty)."""
    vs = [float(v) for v in values if _finite(v)]
    if include_zero:
        vs.append(0.0)
    if not vs:
        return 0.0, 1.0
    lo, hi = min(vs), max(vs)
    if hi - lo < 1e-12:
        d = abs(lo) * 0.1 or 1.0
        return lo - d, hi + d
    pad = (hi - lo) * 0.08
    return lo - pad, hi + pad


def x_positions(n: int, width: float = WIDTH, pad_l: float = PAD_L, pad_r: float = PAD_R) -> list[float]:
    """Evenly spaced x for ``n`` points (one point: centred)."""
    if n <= 0:
        return []
    if n == 1:
        return [pad_l + (width - pad_l - pad_r) / 2]
    step = (width - pad_l - pad_r) / (n - 1)
    return [pad_l + i * step for i in range(n)]


def scale_points(values: Sequence[float | None], lo: float, hi: float, width: float = WIDTH,
                 height: float = HEIGHT, pad: tuple[float, float, float, float] = (PAD_L, PAD_R, PAD_T, PAD_B)
                 ) -> list[Point | None]:
    """Data values -> SVG coordinates (y grows downwards); None for missing values."""
    pl, pr, pt, pb = pad
    xs = x_positions(len(values), width, pl, pr)
    span = (hi - lo) or 1.0
    out: list[Point | None] = []
    for x, v in zip(xs, values):
        if not _finite(v):
            out.append(None)
            continue
        y = pt + (hi - float(v)) / span * (height - pt - pb)
        out.append((round(x, 2), round(y, 2)))
    return out


def _fmt(p: Point) -> str:
    return f"{p[0]:g},{p[1]:g}"


def sparkline_path(points: Sequence[Point | None]) -> str:
    """SVG path data: a polyline through the points, broken (new ``M``) at every gap."""
    parts: list[str] = []
    pen_down = False
    for p in points:
        if p is None:
            pen_down = False
            continue
        parts.append(("L" if pen_down else "M") + _fmt(p))
        pen_down = True
    return " ".join(parts)


def band_path(lo_points: Sequence[Point | None], hi_points: Sequence[Point | None]) -> str:
    """Closed polygon(s) between two point series (a CI band), one per contiguous run where both
    ends are known; a single-point run becomes a thin vertical bar."""
    runs: list[list[tuple[Point, Point]]] = [[]]
    for a, b in zip(lo_points, hi_points):
        if a is None or b is None:
            if runs[-1]:
                runs.append([])
            continue
        runs[-1].append((a, b))
    out = []
    for run in runs:
        if not run:
            continue
        if len(run) == 1:
            (a, b), = run
            run = [((a[0] - 3, a[1]), (b[0] - 3, b[1])), ((a[0] + 3, a[1]), (b[0] + 3, b[1]))]
        top = [b for _, b in run]
        bottom = [a for a, _ in reversed(run)]
        out.append("M" + " L".join(_fmt(p) for p in top + bottom) + " Z")
    return " ".join(out)


def _slug(s: str) -> str:
    return re.sub(r"[^a-z0-9-]+", "-", s.lower()).strip("-") or "chart"


def line_chart(chart_id: str, x: Sequence[str], lines: Sequence[Line], *, title: str, desc: str,
               band: tuple[Sequence[float | None], Sequence[float | None]] | None = None,
               y_fmt: Callable[[float], str] = lambda v: f"{v:.2f}", zero_line: bool = False,
               y_domain: tuple[float, float] | None = None, width: int = WIDTH, height: int = HEIGHT) -> Markup:
    """One inline SVG line chart. ``x`` are the category labels (weeks); every line has one value
    per label. Returns ``Markup`` (safe to drop into a Jinja template)."""
    cid = _slug(chart_id)
    allv: list[float | None] = [v for ln in lines for v in ln.values]
    if band:
        allv += list(band[0]) + list(band[1])
    lo, hi = y_domain if y_domain else domain(allv, include_zero=zero_line)
    pad = (PAD_L, PAD_R, PAD_T, PAD_B)
    sp = lambda vals: scale_points(vals, lo, hi, width, height, pad)  # noqa: E731
    bottom, top = height - PAD_B, PAD_T
    e = escape
    svg = [f'<svg class="spark" id="{cid}" viewBox="0 0 {width} {height}" role="img" '
           f'aria-labelledby="{cid}-t {cid}-d" xmlns="http://www.w3.org/2000/svg" preserveAspectRatio="xMidYMid meet">',
           f'<title id="{cid}-t">{e(title)}</title><desc id="{cid}-d">{e(desc)}</desc>',
           '<g class="ch-axis" aria-hidden="true">',
           f'<line x1="{PAD_L}" x2="{width - PAD_R}" y1="{top}" y2="{top}" class="ch-grid"/>',
           f'<line x1="{PAD_L}" x2="{width - PAD_R}" y1="{bottom}" y2="{bottom}" class="ch-grid"/>',
           f'<text x="{PAD_L - 5}" y="{top + 4}" text-anchor="end">{e(y_fmt(hi))}</text>',
           f'<text x="{PAD_L - 5}" y="{bottom + 1}" text-anchor="end">{e(y_fmt(lo))}</text>']
    if zero_line and lo < 0 < hi:
        (_, y0), = [p for p in sp([0.0]) if p]
        svg.append(f'<line x1="{PAD_L}" x2="{width - PAD_R}" y1="{y0:g}" y2="{y0:g}" class="ch-zero"/>')
    if x:
        xs = x_positions(len(x), width)
        svg.append(f'<text x="{xs[0]:g}" y="{height - 5}" text-anchor="{"middle" if len(x) == 1 else "start"}">'
                   f'{e(_short(x[0]))}</text>')
        if len(x) > 1:
            svg.append(f'<text x="{xs[-1]:g}" y="{height - 5}" text-anchor="end">{e(_short(x[-1]))}</text>')
    svg.append("</g>")
    if band:
        d = band_path(sp(band[0]), sp(band[1]))
        if d:
            svg.append(f'<path class="ch-band" d="{d}" aria-hidden="true"/>')
    for ln in sorted(lines, key=lambda ln: ln.role == "main"):       # main line drawn last (on top)
        pts = sp(ln.values)
        d = sparkline_path(pts)
        if not d:
            continue
        cls = "ch-main" if ln.role == "main" else "ch-base"
        dash = f' stroke-dasharray="{ln.dash}"' if ln.dash else ""
        svg.append(f'<path class="{cls}" d="{d}"{dash} data-key="{e(ln.key)}" aria-hidden="true"/>')
        if ln.role == "main":
            for i, p in enumerate(pts):
                if p is None:
                    continue
                lab = f"{x[i] if i < len(x) else i}: {ln.label} {y_fmt(float(ln.values[i]))}"
                if band and i < len(band[0]) and _finite(band[0][i]) and _finite(band[1][i]):
                    lab += f" (95% CI {y_fmt(float(band[0][i]))} to {y_fmt(float(band[1][i]))})"
                svg.append(f'<g class="ch-pt"><circle class="ch-hit" cx="{p[0]:g}" cy="{p[1]:g}" r="9"/>'
                           f'<circle class="ch-dot" cx="{p[0]:g}" cy="{p[1]:g}" r="3.5"/>'
                           f'<title>{e(lab)}</title></g>')
    svg.append("</svg>")
    return Markup("".join(svg))


def legend(lines: Sequence[Line], band_label: str | None = None) -> Markup:
    """A small HTML legend (line samples drawn as SVG so dashes match the chart)."""
    items = []
    for ln in sorted(lines, key=lambda ln: ln.role != "main"):
        cls = "ch-main" if ln.role == "main" else "ch-base"
        dash = f' stroke-dasharray="{ln.dash}"' if ln.dash else ""
        items.append(f'<li><svg viewBox="0 0 22 8" width="22" height="8" aria-hidden="true">'
                     f'<line x1="1" x2="21" y1="4" y2="4" class="{cls}"{dash}/></svg>{escape(ln.label)}</li>')
    if band_label:
        items.append('<li><svg viewBox="0 0 22 8" width="22" height="8" aria-hidden="true">'
                     f'<rect x="1" y="0" width="20" height="8" class="ch-band"/></svg>{escape(band_label)}</li>')
    return Markup('<ul class="ch-legend">' + "".join(items) + "</ul>")


def _short(week: str) -> str:
    """'2026-10-05' -> 'Oct 5' (anything else unchanged)."""
    m = re.match(r"^(\d{4})-(\d{2})-(\d{2})$", str(week))
    if not m:
        return str(week)
    months = ("Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec")
    return f"{months[int(m.group(2)) - 1]} {int(m.group(3))}"


__all__ = ["DASHES", "Line", "band_path", "domain", "legend", "line_chart", "scale_points", "sparkline_path",
           "x_positions"]
