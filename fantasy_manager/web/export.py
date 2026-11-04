"""Static snapshot of the dashboard (``fm web-export``) for free static hosting.

Renders the dashboard in-process (``create_app(auth=None)``, no server, no login) and crawls it
from the top pages of both leagues, following same-site links to every GET page it can reach
(weekly plan, overview, rosters, moves, news, schedule, matchup, health, player pages). Each page
is written as one flat ``.html`` file next to a copy of ``static/``, with its links rewritten to
those files, so the folder works from any static host (GitHub Pages in the GitHub Actions
workflow, see docs/hosting.md) or straight from disk.

A snapshot is read-only: htmx is dropped (links become plain page loads) and the buttons that
POST to the server (refresh, mode toggle, "Did it", explain, rollback) are hidden; a banner says
when the snapshot was taken. The harness still records your moves from the league transaction
logs, so "Did it" is not needed for tracking.

The folder holds league data in clear text: the workflow encrypts it (staticrypt) before
publishing.
"""
from __future__ import annotations

import html
import re
import shutil
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from urllib.parse import parse_qsl, urlencode, urlsplit

from .app import HERE, LEAGUES

PAGE_LIMIT = 2500
SEED_PATHS = ("/plan", "/overview", "/roster", "/recommendations", "/news", "/schedule", "/matchup", "/health")
# GET pages worth a file; anything else on the site (API, login, POST targets) becomes "#"
PAGE_PREFIXES = ("/plan", "/overview", "/roster", "/recommendations", "/news", "/schedule", "/matchup",
                 "/health", "/player/")
# query parameters that page through time without end (the schedule's week arrows): the snapshot
# keeps the default (current) view only
SKIP_PARAMS = ("week",)
_ATTR = re.compile(r'(\s(?:href|src|action)=)"([^"]*)"')
_HTMX = re.compile(r'<script[^>]*htmx[^>]*></script>\s*', re.I)
SNAPSHOT_CSS = ('<style>form[method="post"],form[method=post]{display:none!important}'
                '.snapshot-note{font-size:.8rem;opacity:.75;text-align:center;padding:.4rem}</style>')


@dataclass
class ExportResult:
    pages: int = 0
    errors: list[str] = field(default_factory=list)
    truncated: bool = False


def page_file(path: str, query: str) -> str:
    """File name of a site URL: "/roster?league=espn&team=3" -> "roster__league-espn_team-3.html"."""
    name = path.strip("/") or "index"
    name = re.sub(r"[^A-Za-z0-9._-]+", "-", name)
    params = sorted(parse_qsl(query, keep_blank_values=True))
    if params:
        name += "__" + re.sub(r"[^A-Za-z0-9._-]+", "-", "_".join(f"{k}-{v}" for k, v in params))
    return name[:180] + ".html"


def _is_page(path: str) -> bool:
    return path == "/" or any(path.startswith(p) if p.endswith("/") else path == p for p in PAGE_PREFIXES)


def rewrite(body: str, stamp: str) -> tuple[str, list[str]]:
    """(page with site links pointing at the exported files, site URLs it links to)."""
    found: list[str] = []

    def sub(m: re.Match[str]) -> str:
        raw = html.unescape(m.group(2))
        if not raw.startswith("/") or raw.startswith("//"):
            return m.group(0)
        parts = urlsplit(raw)
        frag = f"#{parts.fragment}" if parts.fragment else ""
        if parts.path.startswith("/static/"):
            return f'{m.group(1)}"{html.escape(parts.path[1:])}"'
        if (m.group(1).strip().startswith("action") or not _is_page(parts.path)
                or any(k in SKIP_PARAMS for k, _ in parse_qsl(parts.query))):
            return f'{m.group(1)}"#"'
        found.append(urlsplit(raw)._replace(fragment="").geturl())
        return f'{m.group(1)}"{html.escape(page_file(parts.path, parts.query) + frag)}"'

    out = _ATTR.sub(sub, body)
    out = _HTMX.sub("", out).replace('hx-boost="true"', "")
    out = out.replace("</head>", SNAPSHOT_CSS + "</head>", 1)
    note = f'<div class="snapshot-note">Read-only snapshot, updated {html.escape(stamp)}</div>'
    out = re.sub(r"(<body[^>]*>)", r"\1" + note.replace("\\", "\\\\"), out, count=1)
    return out, found


def export_site(out_dir: Path, leagues: tuple[str, ...] = LEAGUES, limit: int = PAGE_LIMIT,
                client_factory=None) -> ExportResult:
    """Crawl the dashboard into ``out_dir`` (emptied first). ``client_factory()`` returns an
    httpx-style client with ``get(url)`` (default: a TestClient over the live app)."""
    if client_factory is None:
        from fastapi.testclient import TestClient

        from .app import create_app

        def client_factory():
            return TestClient(create_app(auth=None, home="plan"), raise_server_exceptions=False)

    out_dir = Path(out_dir)
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)
    shutil.copytree(HERE / "static", out_dir / "static")
    css = out_dir / "static" / "style.css"     # a project site lives under /<repo>/: no absolute paths
    css.write_text(css.read_text(encoding="utf-8").replace('url("/static/', 'url("'), encoding="utf-8")
    stamp = datetime.now().strftime("%a %b %d, %I:%M %p").replace(" 0", " ")
    res = ExportResult()
    client = client_factory()
    queue: deque[str] = deque()
    seen: set[str] = set()
    for lg in leagues:
        for p in ("/", *SEED_PATHS):
            queue.append(f"{p}?{urlencode({'league': lg})}")
    first: str | None = None
    while queue:
        url = queue.popleft()
        parts = urlsplit(url)
        fname = page_file(parts.path, parts.query)
        if fname in seen:
            continue
        if len(seen) >= limit:
            res.truncated = True
            break
        seen.add(fname)
        try:
            r = client.get(url)
        except Exception as e:  # noqa: BLE001 - one bad page must not sink the snapshot
            res.errors.append(f"{url}: {type(e).__name__}: {e}")
            continue
        if r.status_code >= 400:
            res.errors.append(f"{url}: HTTP {r.status_code}")
        if "text/html" not in r.headers.get("content-type", ""):
            continue
        body, links = rewrite(r.text, stamp)
        (out_dir / fname).write_text(body, encoding="utf-8")
        res.pages += 1
        first = first or fname
        queue.extend(links)
    if first is not None:
        shutil.copyfile(out_dir / first, out_dir / "index.html")
    return res
