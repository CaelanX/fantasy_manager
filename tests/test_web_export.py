"""Static dashboard snapshot (web.export): file names, link rewriting, the crawl."""
from types import SimpleNamespace

from fantasy_manager.web.export import export_site, page_file, rewrite

PAGE = """<html><head><link href="/static/style.css" rel="stylesheet">
<script src="https://cdnjs.cloudflare.com/ajax/libs/htmx/2.0.4/htmx.min.js" defer></script></head>
<body hx-boost="true"><a href="/roster?league=espn&amp;team=3#top">r</a> <a href="/player/espn:42?league=espn">p</a>
<a href="/schedule?league=espn&amp;week=2">next week</a> <a href="/api/recs.json?league=espn">json</a>
<a href="https://www.nhl.com/">nhl</a> <form method="post" action="/refresh?league=espn"></form></body></html>"""


def test_page_file_names():
    assert page_file("/", "league=espn") == "index__league-espn.html"
    assert page_file("/roster", "team=3&league=espn") == "roster__league-espn_team-3.html"
    assert page_file("/player/espn:42", "league=espn") == "player-espn-42__league-espn.html"


def test_rewrite_points_links_at_files_and_drops_server_bits():
    out, found = rewrite(PAGE, "Tue Oct 27, 7:30 AM")
    assert 'href="static/style.css"' in out
    assert 'href="roster__league-espn_team-3.html#top"' in out
    assert 'href="player-espn-42__league-espn.html"' in out
    assert found == ["/roster?league=espn&team=3", "/player/espn:42?league=espn"]
    assert 'href="https://www.nhl.com/"' in out                      # external links untouched
    assert out.count('href="#"') == 2                                # week arrows and the API
    assert 'action="#"' in out and "htmx" not in out and "hx-boost" not in out
    assert "Read-only snapshot, updated Tue Oct 27, 7:30 AM" in out


def test_export_crawls_reachable_pages(tmp_path):
    pages = {"/": '<html><head></head><body><a href="/news?league=espn">n</a></body></html>',
             "/news": '<html><head></head><body><a href="/?league=espn">home</a></body></html>'}

    class Client:
        def get(self, url):
            path = url.split("?")[0]
            body = pages.get(path, "<html><head></head><body>other</body></html>")
            return SimpleNamespace(status_code=200, text=body, headers={"content-type": "text/html"})

    res = export_site(tmp_path / "site", leagues=("espn",), client_factory=Client)
    site = tmp_path / "site"
    assert (site / "index.html").read_text(encoding="utf-8") == (site / "index__league-espn.html").read_text(encoding="utf-8")
    assert 'href="news__league-espn.html"' in (site / "index.html").read_text(encoding="utf-8")
    assert (site / "static" / "style.css").exists()
    assert 'url("/static/' not in (site / "static" / "style.css").read_text(encoding="utf-8")
    assert res.pages == 9 and not res.errors and not res.truncated   # "/" + the 8 seed pages, each once
