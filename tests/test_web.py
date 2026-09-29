from datetime import datetime, timezone

import pytest

pytest.importorskip("fastapi")
pytest.importorskip("jinja2")
pytest.importorskip("httpx")

from fastapi.testclient import TestClient  # noqa: E402

from fantasy_manager.models import Reason, StatLine  # noqa: E402
from fantasy_manager.providers.base import ProviderError  # noqa: E402
from fantasy_manager.providers.news import NewsItem  # noqa: E402
from fantasy_manager.valuation.valuate import PlayerValue  # noqa: E402
from fantasy_manager.web.app import LoadResult, create_app, ResultCache  # noqa: E402
from tests.test_digest import make_league, make_recs  # noqa: E402


def make_result(league: str = "espn") -> LoadResult:
    ctx, _ = make_league()
    by = {p.cid: p for p in ctx.all_players()}
    by["a"].lines = {
        "season": StatLine(split="season", gp=10, stats={"G": 6, "A": 8, "SOG": 30, "GP": 10}),
        "last7": StatLine(split="last7", gp=3, stats={"G": 2, "A": 3, "SOG": 10}),
    }
    by["g"].lines = {"season": StatLine(split="season", gp=5, stats={"W": 3, "GAA": 2.41, "SVPCT": 0.918})}
    values = {}
    for i, p in enumerate(ctx.all_players()):
        values[p.cid] = PlayerValue(
            player=p, fpg=2.0 + i * 0.1, fpg_season=1.9 + i * 0.1, fpg_week=1.5, vorp=0.5 - i * 0.2,
            proj_week=6.0 + i, games_next7=3 + i % 2, rates={"G": 0.4, "A": 0.6, "SOG": 3.1},
            reasons=[Reason(code="BASELINE", text=f"Baseline for {p.name}", value=2.0)])
    news = {"a": [NewsItem(source="rotowire", player_name="Tim Stutzle", headline="Stutzle scores twice",
                           blurb="Two goals in a win.", url="https://www.rotowire.com/hockey/x",
                           published=datetime(2026, 10, 6, tzinfo=timezone.utc), tags=["line"])],
            "x": [NewsItem(source="espn", headline="McDavid <b>hat trick</b>", url="javascript:alert(1)",
                           published=datetime(2026, 10, 5, tzinfo=timezone.utc))]}
    return LoadResult(ctx=ctx, values=values, dynasty=None, recs=make_recs(ctx), news_by_cid=news,
                      loaded_at=datetime(2026, 10, 7, 8, 30), warnings=["Injury feed stale"])


class CountingLoader:
    def __init__(self, fail: Exception | None = None):
        self.calls: list[str] = []
        self.fail = fail

    def __call__(self, league: str) -> LoadResult:
        self.calls.append(league)
        if self.fail:
            raise self.fail
        return make_result(league)


@pytest.fixture
def loader():
    return CountingLoader()


@pytest.fixture
def client(loader):
    return TestClient(create_app(loader))


def test_healthz_does_not_load(client, loader):
    r = client.get("/healthz")
    assert r.status_code == 200 and r.json()["status"] == "ok"
    assert loader.calls == []


def test_footer_league_line_shows_roster_limits():
    res = make_result()
    res.ctx.position_limits, res.ctx.max_roster_size = {"G": 3}, 22
    h = TestClient(create_app(lambda league: res)).get("/").text
    assert "roster limits: max G 3 (incl. IR); roster max 22 (excl. IR)" in h
    assert "roster limits:" not in TestClient(create_app(lambda league: make_result())).get("/").text


def test_overview(client):
    r = client.get("/")
    assert r.status_code == 200
    h = r.text
    assert "Test League" in h and "My Team" in h
    assert "Move Jake Sanderson to IR" in h and "Add Free Agent One, drop Jake Sanderson" in h
    assert "Injury alerts" in h and "Jake Sanderson" in h and "Lower body" in h
    assert 'name="viewport"' in h and "/static/style.css" in h
    assert "Injury feed stale" in h  # freshness footer warnings
    assert 'action="/refresh?league=espn' in h
    assert 'href="/?league=fantrax"' in h  # league switcher
    assert "Valuation params: fit 2026-09-28 (espn)" in h  # params provenance in the footer


def test_roster_page(client):
    h = client.get("/roster?league=espn").text
    for name in ("Tim Stützle", "Brady Tkachuk", "Jake Sanderson", "Linus Ullmark"):
        assert name in h
    assert "Connor McDavid" not in h
    assert 'data-label="Proj pts/wk"' in h and "pill-ir" in h
    assert 'data-label="Lineup &Delta;"' in h and "Games next 7" in h
    other = client.get("/roster?team=2").text
    assert "Connor McDavid" in other and "Cale Makar" in other


def test_recommendations_filter(client):
    h = client.get("/recommendations").text
    # one "what to do" line per move (text, not a control)
    for action in ("Move Jake Sanderson to IR", "Propose: Tkachuk for Makar", "Start Ullmark",
                   "Shop Stützle while his value is high", "Add Free Agent One, drop Jake Sanderson"):
        assert f'<h3 class="do">{action}</h3>' in h
    assert "+0.80 FPG over RW replacement" in h and "A clear upgrade." in h
    trades = client.get("/recommendations?kind=trade").text
    assert "Propose: Tkachuk for Makar" in trades and "Start Ullmark" not in trades
    flags = client.get("/recommendations?kind=flags").text
    assert "Shop Stützle" in flags and "Move Jake Sanderson to IR" not in flags


def test_news_page_escapes_and_drops_unsafe_links(client):
    h = client.get("/news").text
    assert "Stutzle scores twice" in h and "https://www.rotowire.com/hockey/x" in h
    assert "McDavid &lt;b&gt;hat trick&lt;/b&gt;" in h
    assert "javascript:alert" not in h
    mine = h.split("Your players")[1].split("Around the league")[0]
    assert "Stutzle scores twice" in mine and "McDavid" not in mine


def test_player_page(client):
    r = client.get("/player/a")
    assert r.status_code == 200
    h = r.text
    assert "Tim Stützle" in h and "Baseline for Tim Stützle" in h
    assert "Season" in h and "Last 7" in h and "SOG" in h
    assert "Stutzle scores twice" in h
    assert "Shop Stützle while his value is high" in h  # moves involving the player
    goalie = client.get("/player/g").text
    assert "SV%" in goalie and "0.918" in goalie
    assert client.get("/player/nope").status_code == 404


def test_json_endpoints(client):
    recs = client.get("/api/recs.json").json()
    assert recs["meta"]["provider"] == "espn" and recs["meta"]["warnings"] == ["Injury feed stale"]
    assert [r["title"] for r in recs["recommendations"]][0] == "Move Jake Sanderson to IR"
    assert len(recs["recommendations"]) == 5
    only = client.get("/api/recs.json?kind=waiver").json()["recommendations"]
    assert [r["kind"] for r in only] == ["waiver"]
    roster = client.get("/api/roster.json").json()
    assert roster["team"]["name"] == "My Team"
    names = [s["player"]["name"] for s in roster["slots"]]
    assert names == ["Tim Stützle", "Brady Tkachuk", "Jake Sanderson", "Linus Ullmark"]
    assert roster["slots"][0]["value"]["proj_week"] == 6.0 and "player" not in roster["slots"][0]["value"]


def test_invalid_league_rejected(client):
    assert client.get("/?league=yahoo").status_code == 422


def test_setup_page_on_provider_error():
    app = create_app(CountingLoader(ProviderError("ESPN_LEAGUE_ID is not set.")))
    c = TestClient(app)
    r = c.get("/")
    assert r.status_code == 200
    assert "Setup needed" in r.text and "ESPN_LEAGUE_ID is not set." in r.text
    assert "ESPN_S2" in r.text and "ESPN_SWID" in r.text
    fx = c.get("/roster?league=fantrax").text
    assert "FANTRAX_LEAGUE_ID" in fx and "FANTRAX_COOKIE" in fx
    j = c.get("/api/recs.json")
    assert j.status_code == 503 and "ESPN_LEAGUE_ID" in j.json()["error"]


def test_unexpected_error_is_friendly():
    c = TestClient(create_app(CountingLoader(RuntimeError("boom"))), raise_server_exceptions=False)
    r = c.get("/news")
    assert r.status_code == 500 and "Something went wrong" in r.text and "boom" in r.text


def test_cache_and_refresh(client, loader):
    client.get("/")
    client.get("/roster")
    client.get("/api/recs.json")
    assert loader.calls == ["espn"]
    client.get("/?league=fantrax")
    assert loader.calls == ["espn", "fantrax"]
    r = client.post("/refresh?league=espn&next=/roster", follow_redirects=False)
    assert r.status_code == 303 and r.headers["location"] == "/roster?league=espn"
    client.get("/")
    client.get("/?league=fantrax")
    assert loader.calls == ["espn", "fantrax", "espn"]
    evil = client.post("/refresh?next=//evil.example", follow_redirects=False)
    assert evil.headers["location"] == "/?league=espn"


def test_refresh_reloads_the_params_layer(client, monkeypatch):
    from fantasy_manager.valuation import params as VP

    calls = []
    monkeypatch.setattr(VP, "reload", lambda: calls.append(1) or {})
    client.post("/refresh?league=espn", follow_redirects=False)
    assert calls == [1]


def test_cache_ttl_expires():
    now = [0.0]
    loader = CountingLoader()
    cache = ResultCache(loader, ttl=600, clock=lambda: now[0])
    cache.get("espn")
    now[0] = 599
    cache.get("espn")
    assert len(loader.calls) == 1
    now[0] = 601
    cache.get("espn")
    assert len(loader.calls) == 2


def test_module_app_import_is_offline():
    from fantasy_manager.web import app as mod

    assert mod.app is not None
    assert mod.app.state.cache.age("espn") is None


# -- dynasty mode toggle -------------------------------------------------------------------

from fantasy_manager import prefs  # noqa: E402
from fantasy_manager.config import get_settings  # noqa: E402
from fantasy_manager.web.app import BaseLoad, PipelineLoader, safe_next  # noqa: E402


@pytest.fixture
def isolated(monkeypatch, tmp_path):
    """No real .env / prefs.json: FANTRAX_MODE unset, data dir under tmp."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FANTRAX_MODE", raising=False)
    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path / "data"))
    get_settings.cache_clear()
    yield tmp_path / "data"
    get_settings.cache_clear()


def dynasty_result(league: str = "fantrax") -> LoadResult:
    res = make_result(league)
    res.ctx.provider, res.ctx.dynasty = league, True
    return res


def test_mode_control_only_for_dynasty_leagues(isolated):
    c = TestClient(create_app(CountingLoader()))
    espn = c.get("/?league=espn").text
    assert 'action="/mode"' not in espn and "Dynasty mode:" not in espn
    fx = c.get("/?league=fantrax").text                      # fantrax league: control even if not dynasty
    assert fx.count('action="/mode"') == 3
    assert fx.count('aria-pressed="true"') == 1
    pressed = fx.split('aria-pressed="true"')[1].split("</button>")[0]
    assert "Balanced" in pressed                               # default mode
    assert 'name="next" value="/?league=fantrax"' in fx
    assert "Dynasty mode: <strong>balanced</strong> (from default)" in fx
    assert 'class="flash"' not in fx

    class DynLoader(CountingLoader):
        def __call__(self, league):
            self.calls.append(league)
            return dynasty_result("espn")

    dyn = TestClient(create_app(DynLoader())).get("/?league=espn").text   # espn but dynasty ctx
    assert 'action="/mode"' in dyn and "dynasty, <span" in dyn


def test_post_mode_sets_pref_clears_cache_and_redirects(isolated):
    loader = CountingLoader()
    c = TestClient(create_app(loader))
    c.get("/?league=fantrax")
    c.get("/?league=espn")
    assert loader.calls == ["fantrax", "espn"]
    r = c.post("/mode", data={"mode": "rebuild", "league": "fantrax", "next": "/roster?league=fantrax&team=2"},
               follow_redirects=False)
    assert r.status_code == 303
    assert r.headers["location"] == "/roster?league=fantrax&team=2&msg=mode-rebuild"
    assert prefs.get_pref("dynasty_mode") == "rebuild"
    assert prefs.dynasty_mode_info() == ("rebuild", "prefs")
    page = c.get(r.headers["location"]).text
    assert loader.calls == ["fantrax", "espn", "fantrax"]       # only that league reloaded
    assert '<p class="flash" role="status">Recalculating with rebuild mode' in page
    pressed = page.split('aria-pressed="true"')[1].split("</button>")[0]
    assert "Rebuild" in pressed
    assert "(from dashboard/prefs)" in page
    assert 'value="/roster?league=fantrax&amp;team=2"' in page  # msg is not carried into next
    c.get("/?league=espn")
    assert loader.calls == ["fantrax", "espn", "fantrax"]       # espn cache untouched


@pytest.mark.parametrize("nxt, expected", [
    ("//evil.example/x", "/?league=fantrax&msg=mode-contend"),
    ("https://evil.example/", "/?league=fantrax&msg=mode-contend"),
    ("/\\evil.example", "/?league=fantrax&msg=mode-contend"),
    ("/\t/evil.example", "/?league=fantrax&msg=mode-contend"),
    ("/news?msg=old", "/news?league=fantrax&msg=mode-contend"),
    ("", "/?league=fantrax&msg=mode-contend"),
])
def test_post_mode_redirects_same_site_only(isolated, nxt, expected):
    c = TestClient(create_app(CountingLoader()))
    r = c.post("/mode", data={"mode": "contend", "league": "fantrax", "next": nxt}, follow_redirects=False)
    assert r.status_code == 303 and r.headers["location"] == expected


def test_post_mode_rejects_bad_input(isolated):
    c = TestClient(create_app(CountingLoader()))
    for data in ({"mode": "tank", "league": "fantrax"}, {"league": "fantrax"},
                 {"mode": "rebuild", "league": "yahoo"}):
        r = c.post("/mode", data=data, follow_redirects=False)
        assert r.status_code == 422, data
    assert prefs.get_pref("dynasty_mode") is None
    # query params work too (curl -X POST '/mode?mode=Contend&league=espn')
    r = c.post("/mode?mode=Contend&league=espn", follow_redirects=False)
    assert r.status_code == 303 and prefs.get_pref("dynasty_mode") == "contend"


def test_api_mode_json(isolated, monkeypatch):
    c = TestClient(create_app(CountingLoader()))
    j = c.get("/api/mode.json").json()
    assert j["mode"] == "balanced" and j["source"] == "default" and j["modes"] == ["contend", "balanced", "rebuild"]
    assert j["computed"] == {"espn": None, "fantrax": None}
    monkeypatch.setenv("FANTRAX_MODE", "contend")
    get_settings.cache_clear()
    j = c.get("/api/mode.json").json()
    assert (j["mode"], j["source"], j["source_label"]) == ("contend", "env", "FANTRAX_MODE")
    c.get("/?league=espn")
    c.post("/mode", data={"mode": "rebuild", "league": "fantrax"})
    j = c.get("/api/mode.json").json()
    assert (j["mode"], j["source"], j["source_label"]) == ("rebuild", "prefs", "dashboard/prefs")
    assert j["computed"]["espn"]["mode"] == "contend"          # test loader leaves the ctx mode as is
    assert j["computed"]["espn"]["stale"] is False             # (it does not resolve modes)


def test_safe_next_helper():
    assert safe_next("/roster", "espn") == "/roster?league=espn"
    assert safe_next("/roster?league=fantrax", "espn") == "/roster?league=fantrax"
    assert safe_next("//x.example", "espn") == "/?league=espn"
    assert safe_next(None, "espn", msg="mode-balanced") == "/?league=espn&msg=mode-balanced"


def test_pipeline_loader_reuses_base_on_mode_change(isolated, monkeypatch):
    """A mode change reruns only dynasty + advise; the provider/NHL/news half is reused."""
    import fantasy_manager.recommend.advise as advise_mod

    seen: list[str] = []

    def fake_advise(ctx, values, dynasty_values=None, history=None, **kw):
        seen.append(ctx.dynasty_mode)
        return make_recs(ctx)

    monkeypatch.setattr(advise_mod, "advise", fake_advise)
    bases: list[str] = []

    def base_loader(league):
        bases.append(league)
        res = dynasty_result(league)
        return BaseLoad(ctx=res.ctx, values=res.values, provider=None, news_by_cid=res.news_by_cid,
                        loaded_at=res.loaded_at, seconds=5.0)

    now = [0.0]
    loader = PipelineLoader(ttl=600, clock=lambda: now[0], base_loader=base_loader)
    c = TestClient(create_app(loader, clock=lambda: now[0]))
    first = c.get("/?league=fantrax").text
    assert bases == ["fantrax"] and seen == ["balanced"]
    assert 'dynasty, <span class="mode-tag">balanced mode</span>' in first
    assert "computed in 5.0s" in first
    before = c.app.state.cache.peek("fantrax")
    r = c.post("/mode", data={"mode": "rebuild", "league": "fantrax", "next": "/"})
    assert r.status_code == 200
    assert bases == ["fantrax"] and seen == ["balanced", "rebuild"]   # no provider reload
    assert "rebuild mode</span>" in r.text and "(mode recalculation only)" in r.text
    assert "cached league data reused" in r.text
    after = c.app.state.cache.peek("fantrax")
    assert after.mode == "rebuild" and after.recalc_only and after.ctx is not before.ctx
    assert before.ctx.dynasty_mode == "balanced"                       # old result not mutated
    dyn_before = {k: v.value for k, v in before.dynasty.items()}
    dyn_after = {k: v.value for k, v in after.dynasty.items()}
    assert dyn_before.keys() == dyn_after.keys() and dyn_before != dyn_after   # values recomputed
    # a mode changed elsewhere (e.g. `fm mode contend`) is picked up on the next load
    prefs.set_pref("dynasty_mode", "contend")
    c.get("/roster?league=fantrax")
    assert seen[-1] == "contend" and bases == ["fantrax"]
    # /refresh drops the base too
    c.post("/refresh?league=fantrax")
    assert bases == ["fantrax", "fantrax"]


# -- redesign: summary, chips, moves, dense sections ------------------------------------------

import re  # noqa: E402
from datetime import date, timedelta  # noqa: E402
from types import SimpleNamespace  # noqa: E402

from fantasy_manager.models import Recommendation  # noqa: E402
from fantasy_manager.web.app import action_line, overview_summary, rec_key  # noqa: E402
from fantasy_manager.web import views  # noqa: E402


def rich_result(league: str = "espn") -> LoadResult:
    """make_result plus a schedule with opponents, a birth date, status history and a rec with
    many reasons (for the "Why?" disclosure)."""
    res = make_result(league)
    ctx = res.ctx
    d0 = ctx.as_of
    ctx.schedule = {"OTT": [d0, d0 + timedelta(days=2), d0 + timedelta(days=9)], "SJS": [d0 + timedelta(days=1)]}
    ctx.opponents = {"OTT": {d0: "@TOR", d0 + timedelta(days=2): "BOS", d0 + timedelta(days=9): "@MTL"},
                     "SJS": {d0 + timedelta(days=1): "ANA"}}
    ctx.games_per_day = {d0: 12, d0 + timedelta(days=1): 3, d0 + timedelta(days=2): 5, d0 + timedelta(days=9): 10}
    by = {p.cid: p for p in ctx.all_players()}
    by["a"].birth_date = date(2004, 1, 1)          # 22, no career GP line -> prospect
    by["a"].career_gp = 40
    many = [Reason(code=f"R{i}", text=f"reason number {i}", value=float(i), baseline=0.5) for i in range(6)]
    res.recs[1] = res.recs[1].model_copy(update={"reasons": many})   # the trade
    res.status_log = {"c": [SimpleNamespace(cid="c", status="dtd", note="Day to day", seen_at=1.0e9),
                            SimpleNamespace(cid="c", status="ir", note="Lower body", seen_at=1.1e9)]}
    return res


class RichLoader(CountingLoader):
    def __call__(self, league):
        self.calls.append(league)
        return rich_result(league)


@pytest.fixture
def rich():
    return TestClient(create_app(RichLoader(), llm=lambda: StubLLM(available=False)))


class StubLLM:
    def __init__(self, reply="", exc=None, available=True, model="openrouter/free"):
        self.reply, self.exc, self.available, self.model = reply, exc, available, model
        self.calls = []

    def complete(self, system, user, max_tokens=800, temperature=0.3, json_mode=False, **kw):
        self.calls.append(user)
        if self.exc:
            raise self.exc
        return self.reply


def test_summary_sentence_is_built_in_python():
    res = make_result()
    team = res.ctx.my_team
    alerts = [(s.slot, s.player) for s in team.slots if s.player and s.player.status == "ir"]
    text = str(overview_summary(res.recs, alerts))
    assert text == ("<b>5 moves</b> are on the board. The strongest is to move Jake Sanderson to IR. "
                    "One player needs attention: <b>Jake Sanderson</b> is on IR.")
    assert str(overview_summary([], [])) == "No moves are recommended right now. Nobody on your roster is hurt."
    two = [("C", team.slots[0].player.model_copy(update={"name": "A<b>", "status": "dtd"})),
           ("D", team.slots[2].player)]
    out = str(overview_summary(res.recs[:1], two, prospects=2))
    assert "<b>1 move</b> is on the board" in out and "It is to add Free Agent One" in out
    assert "Two players need attention: <b>A&lt;b&gt;</b> (Day-to-day) and <b>Jake Sanderson</b> (IR)." in out
    assert out.endswith("You carry two prospects.")


def test_overview_shows_summary_and_kind_chips(client):
    h = client.get("/?league=espn").text
    lede = h.split('<p class="lede" id="summary">')[1].split("</p>")[0]
    assert "<b>5 moves</b> are on the board" in lede and "<b>Jake Sanderson</b> is on IR" in lede
    chips = h.split('<nav class="chips" aria-label="By type">')[1].split("</nav>")[0]
    assert re.search(r'aria-current="page">All <i>5</i>', chips)
    for label in ("Injury", "Lineup", "Waiver", "Trade", "Flags"):
        assert re.search(rf'kind=\w+#moves">{label} <i>1</i>', chips), label
    trade = client.get("/?league=espn&kind=trade").text
    assert "Propose: Tkachuk for Makar" in trade and "Start Ullmark" not in trade
    assert 'kind=trade#moves" aria-current="page">Trade <i>1</i>' in trade
    assert "Top trade moves" in trade


def test_recommendation_chips_zero_counts_and_filters(client):
    h = client.get("/recommendations?kind=waiver").text
    assert 'aria-current="page">Waiver <i>1</i>' in h
    one = TestClient(create_app(type("L", (CountingLoader,), {"__call__": lambda self, lg: _only_waivers(lg)})()))
    zero = one.get("/recommendations").text
    assert '<span class="chip-f off">Trade <i>0</i></span>' in zero
    # min gain (predicted gain in the move's units) and position filters, server-side
    d = client.get("/recommendations?pos=D").text
    assert "Move Jake Sanderson to IR" in d and "Start Ullmark" not in d
    g = client.get("/recommendations?pos=G").text
    assert "Start Ullmark" in g and "Propose: Tkachuk" not in g
    def with_gain(league):
        res = make_result(league)
        res.recs[0] = res.recs[0].model_copy(update={"predicted_gain": 0.8, "gain_units": "season_fpg"})
        return res

    gc = TestClient(create_app(with_gain))
    gain = gc.get("/recommendations?min_gain=0.5").text
    assert "Add Free Agent One" in gain and "Propose: Tkachuk" not in gain   # trade gain unknown
    assert "+0.8</b> <span class=\"gain-u\">FPG</span>" in gain and "rest of season" in gain
    assert "Add Free Agent One" not in gc.get("/recommendations?min_gain=0.9").text
    assert 'value="0.5"' in gain and "Clear filters" in gain
    assert 'href="/recommendations?league=espn&amp;kind=trade&amp;min_gain=0.5' in gain  # chips keep filters


def _only_waivers(league):
    res = make_result(league)
    res.recs = [r for r in res.recs if r.kind == "waiver"]
    return res


def test_why_disclosure_lists_every_reason_with_numbers(rich):
    h = rich.get("/recommendations?kind=trade").text
    item = h.split('<li class="move"')[1]
    assert item.count("<li><b>r") == 3                       # first three reasons inline
    assert '<summary class="ghost-btn">Why? (6 reasons)</summary>' in item
    body = item.split("Why? (6 reasons)")[1]
    for i in range(6):
        assert f"<code>R{i}</code>" in body and f"reason number {i}" in body
    assert '<td data-label="Value" class="n">5.00</td>' in body
    assert "Rival: weakest starting slots" in body and "Rival: roster" in body   # counterparty
    assert "Connor McDavid" in body


def test_move_comparison_gain_and_strength(rich):
    h = rich.get("/recommendations?kind=waiver").text
    item = h.split('<li class="move"')[1]
    cmp = item.split('<table class="r cmp">')[1].split("</table>")[0]
    assert '<span class="verb in">Add</span>' in cmp and '<span class="verb out">Drop</span>' in cmp
    assert "Free Agent One" in cmp and "Jake Sanderson" in cmp and "Net" in cmp
    for row in ("Pos &middot; NHL", "Age", "Status", "GP season", "FPG season", "FPG healthy", "Proj pts/wk",
                "Games next 7", "VORP (FPG)", "G/GP", "SOG/GP", "PIM/GP"):
        assert row in cmp, row
    assert "ANA" in cmp and "@TOR" in cmp                   # opponents in the games row
    assert "Lower body" in cmp                              # status note
    # derived gain (no predicted_gain on the rec): VORP_DELTA would be used; here none -> fallback
    assert 'class="m-metrics"' in item and "Rank score" in item and "1 of 1 waiver" in item


def test_gain_and_strength_use_engine_fields_when_present():
    res = make_result()
    r = res.recs[0].model_copy(update={"predicted_gain": 0.95, "gain_units": "season_fpg", "horizon_days": None,
                                       "strength": 7.6, "rank_in_kind": 1, "kind_total": 4,
                                       "reasons": [Reason(code="GP", text="3 GP", value=3, baseline=0.3)]})
    g = views.gain_info(r, res)
    assert (g["value"], g["unit"], g["horizon"], g["confidence_label"]) == (0.95, "FPG", "rest of season",
                                                                           "low confidence")
    st = views.strength_info(r, res.recs)
    assert st["is_rank"] is False and st["level"] == "high" and st["rank_text"] == "1 of 4 waivers"
    weak = views.strength_info(r.model_copy(update={"strength": 2.0}), res.recs)
    assert weak["level"] == "low"
    fallback = views.strength_info(res.recs[2], res.recs)   # lineup rec without engine fields
    assert fallback["is_rank"] and fallback["rank_text"] == "1 of 1 lineup move"
    lineup = Recommendation(kind="lineup", score=1, title="Start A over B", add=[res.recs[2].add[0]],
                            reasons=[Reason(code="LINEUP_GAIN", text="+2.00 (week) from this change", value=2.0)])
    assert views.gain_info(lineup, res)["unit"] == "pts/wk"


def test_action_lines():
    res = make_result()
    by = {r.kind: r for r in res.recs}
    assert action_line(by["injury"]) == "Move Jake Sanderson to IR"
    assert action_line(by["trade"]) == "Propose: Tkachuk for Makar"
    assert action_line(by["lineup"]) == "Start Ullmark"
    ctx = res.ctx
    p = {x.cid: x for x in ctx.all_players()}
    r = Recommendation(kind="injury", score=1, title="Move Seth Jarvis to IR and add Eeli Tolvanen")
    assert action_line(r) == "Move Seth Jarvis to IR, add Eeli Tolvanen"
    r = Recommendation(kind="lineup", score=1, title="Start X over Y for this week's lineup (locks Monday)",
                       add=[p["f1"]], drop=[p["b"]])
    assert action_line(r) == "Start One over Tkachuk"
    r = Recommendation(kind="trade", score=1, title="t", add=[p["x"], p["y"]], drop=[p["a"]])
    assert action_line(r) == "Propose: Stützle for McDavid + Makar"


def test_overview_dense_sections(rich):
    h = rich.get("/").text
    week = h.split('id="h-week"')[1].split("</section>")[0]
    for kpi in ("Current lineup", "Optimal lineup", "Available gain", "Starter games", "Lineup changes"):
        assert kpi in week
    assert "pts/wk" in week and "@TOR" in week and "<sup>off</sup>" in week     # opponents + off-night
    stand = h.split('id="h-standings"')[1].split("</section>")[0]
    assert 'class="mine"' in stand and "My Team" in stand and "Rival" in stand and "3-1-0" in stand
    fa = h.split('id="h-fa"')[1].split("</section>")[0]
    assert "Free Agent One" in fa and "Free Agent Two" in fa and "Proj pts/wk" in fa
    linj = h.split('id="h-linj"')[1].split("</section>")[0]
    assert "No injured players on other rosters." in linj


def test_roster_views_and_prospect(rich):
    h = rich.get("/roster").text
    assert "Tim Stützle" in h and ", prospect</span>" in h and "Prospect: age 22 or under" in h
    assert 'aria-current="page">Fantasy</a>' in h
    stats = rich.get("/roster?view=stats").text
    assert "Skaters &middot; 3" in stats and "Goalies &middot; 1" in stats
    for col in ("G<span", "PTS<span", "HIT<span", "PPP<span", "GAA", "SV%", "Start %"):
        assert col in stats, col
    assert 'aria-current="page">Stats per game</a>' in stats


def test_player_page_schedule_history_and_reasons(rich):
    h = rich.get("/player/c").text
    sched = h.split('id="h-sched"')[1].split("</section>")[0]
    assert "at TOR" in sched and "vs BOS" in sched and "at MTL" in sched   # 14-day window
    assert '<span class="chg-in">yes</span>' in sched                     # off-night (5 games)
    hist = h.split('id="h-inj"')[1].split("</section>")[0]
    assert "Day to day" in hist and hist.index("Lower body") < hist.index("Day to day")   # newest first
    assert "<code>BASELINE</code>" in h and "Baseline for Jake Sanderson" in h


def test_player_page_dynasty_breakdown():
    res = make_result("fantrax")
    res.ctx.dynasty = True
    res.dynasty = {p.cid: SimpleNamespace(value=12.3, age=24.5, age_mult=0.97, upside=1.2, pedigree=1.1,
                                          model_value=11.0, market_value=None, mode="balanced", horizon_years=3,
                                          reasons=[Reason(code="AGE_CURVE", text="Age 24.5", value=0.97)])
                   for p in res.ctx.all_players()}
    c = TestClient(create_app(lambda lg: res, llm=lambda: StubLLM(available=False)))
    h = c.get("/player/a?league=fantrax").text
    dyn = h.split('id="h-dyn"')[1].split("</section>")[0]
    for s in ("balanced mode", "3-year horizon", "12.3", "97.0", "&times;1.10", "11.0", "n/a", "1.0 / 0.8 / 0.64",
              "<code>AGE_CURVE</code>"):
        assert s in dyn, s


def _explain_client(stub):
    loader = CountingLoader()
    return TestClient(create_app(loader, llm=lambda: stub)), loader


def test_explain_success_is_cached_and_escaped():
    stub = StubLLM('{"0": "Free Agent One adds <b>0.80</b> FPG over replacement."}')
    c, loader = _explain_client(stub)
    page = c.get("/recommendations").text
    res = c.app.state.cache.peek("espn")
    waiver = next(r for r in res.recs if r.kind == "waiver")
    key = rec_key("espn", waiver)
    assert f'id="rec-{key}"' in page and f'name="rec_key" value="{key}"' in page
    assert 'title="set OPENROUTER_API_KEY"' not in page
    r = c.post("/explain", data={"league": "espn", "rec_key": key, "next": "/recommendations?league=espn"},
               follow_redirects=False)
    assert r.status_code == 303 and r.headers["location"] == f"/recommendations?league=espn#rec-{key}"
    h = c.get("/recommendations").text
    box = h.split(f'id="explain-{key}"')[1].split("</div>\n    </div>")[0]
    assert "adds &lt;b&gt;0.80&lt;/b&gt; FPG" in box and "Explained by openrouter/free" in box
    assert len(stub.calls) == 1 and loader.calls == ["espn"]
    assert waiver.narrative == "A clear upgrade."           # the cached rec itself is not mutated
    assert "adds &lt;b&gt;0.80" in c.get("/").text          # persists across pages until refresh


def test_explain_unavailable_disables_button():
    stub = StubLLM(available=False)
    c, _ = _explain_client(stub)
    h = c.get("/").text
    assert 'disabled title="set OPENROUTER_API_KEY"' in h
    key = rec_key("espn", c.app.state.cache.peek("espn").recs[0])
    c.post("/explain", data={"league": "espn", "rec_key": key, "next": "/"})
    assert stub.calls == [] and "set OPENROUTER_API_KEY" in c.get("/").text


def test_explain_failure_shows_inline_note():
    from fantasy_manager.llm.openrouter import LLMError

    c, _ = _explain_client(StubLLM(exc=LLMError("429 rate limited")))
    c.get("/")
    key = rec_key("espn", c.app.state.cache.peek("espn").recs[0])
    r = c.post("/explain", data={"league": "espn", "rec_key": key, "next": "/"})
    assert r.status_code == 200
    assert ("Explanation unavailable right now (free model rate-limited); numbers above are the full basis."
            in r.text)
    bad = c.post("/explain", data={"league": "espn", "rec_key": "nope", "next": "//evil.example"},
                 follow_redirects=False)
    assert bad.status_code == 303 and bad.headers["location"] == "/?league=espn"


def test_lineup_chain_note_empty_slot_and_moved_subjects():
    from fantasy_manager.web.app import action_note

    res = make_result()
    p = {x.cid: x for x in res.ctx.all_players()}
    r = Recommendation(kind="lineup", score=1, title="Start Free Agent One at C (empty slot)", add=[p["f1"]])
    assert action_line(r) == "Start One" and action_note(r) == "Fills an empty starting slot."
    chain = Recommendation(kind="lineup", score=1,
                           title="Start Free Agent One at UTIL for Tim Stützle; move Tim Stützle to C over "
                                 "Brady Tkachuk (out)",
                           add=[p["f1"]], drop=[p["b"]], subjects=[p["a"]],
                           reasons=[Reason(code="LINEUP_MOVE",
                                           text="Tim Stützle moves to C (replacing Brady Tkachuk)")])
    assert action_line(chain) == "Start One over Tkachuk"
    assert action_note(chain) == "Then move Tim Stützle to C over Brady Tkachuk (out)."
    cmp = views.compare(res, chain)
    assert [(c["side"], c["verb"], c["row"]["cid"]) for c in cmp["cols"]][-1] == ("subj", "Moves", "a")


# -- model health (harness bar) ------------------------------------------------------------

def test_health_without_ledger_is_empty_and_creates_nothing(isolated):
    c = TestClient(create_app(CountingLoader()))
    r = c.get("/api/health.json")
    assert r.status_code == 200 and r.json()["leagues"] == {}
    h = c.get("/health")
    assert h.status_code == 200 and "Model health" in h.text and "fm harness daily" in h.text
    assert 'href="/health?league=espn" aria-current="page"' in h.text          # nav link
    assert not (isolated / "harness.db").exists()


def test_health_reports_the_graded_bar(isolated):
    from fantasy_manager.harness.ledger import Ledger
    from fantasy_manager.harness.metrics import grade_week

    with Ledger(isolated) as led:
        led.upsert("rec_episodes", [{"episode_id": "e1", "league": "espn", "rec_key": "k", "kind": "waiver",
                                     "first_seen": "2026-10-05", "last_seen": "2026-10-05", "n_days": 1}],
                   ("episode_id",))
        grade_week(led, "espn", date(2026, 10, 12))
    loader = CountingLoader()
    c = TestClient(create_app(loader))
    j = c.get("/api/health.json").json()
    L = j["leagues"]["espn"]
    assert L["graded"] and L["week"] == "2026-10-12" and L["headline"] is None
    assert {r["pool"] for r in L["projection"] if r["metric"] == "proj_fpg_mae"} == {"F", "D", "G"}
    assert all(r["trust"] == "hidden" for r in L["projection"] + L["hit_rates"])
    assert {"metric": "hit_rate", "pool": "waiver:followed", "n": 0, "need": 20} in L["not_judgeable"]
    assert j["params"]["version"] == "packaged"
    h = c.get("/health?league=espn").text                                    # one league per page
    assert "ESPN" in h and "Bar as of week 2026-10-12" in h and "Not yet judgeable" in h
    assert 'action="/mode"' not in h and loader.calls == []                  # never loads a league
    fx = c.get("/health?league=fantrax").text                               # unknown to the ledger: no data yet
    assert "Nothing graded yet for FANTRAX" in fx and loader.calls == []


# -- model health: full page (M4) ---------------------------------------------------------

import json as _json  # noqa: E402


def _snap_row(week, league, metric, pool, value, n, trust, detail, lo=None, hi=None):
    return {"snapshot_id": f"{week}|{league}|{metric}|{pool}", "week": week, "league": league, "metric": metric,
            "pool": pool, "value": value, "n": n, "ci_lo": lo, "ci_hi": hi, "trust": trust,
            "detail_json": _json.dumps(detail), "created_at": "2026-11-09T08:00:00"}


def seed_health_ledger(led, league="espn"):
    """Three graded weeks: provisional forward / defense projections, hidden goalies, waiver hit
    rates crossing into provisional, calibration and counterfactual rows, a trade case."""
    from fantasy_manager.harness.metrics import wilson

    snaps = ["2026-10-05", "2026-10-12", "2026-10-19"]
    rows = []
    for gi, week in enumerate(["2026-10-26", "2026-11-02", "2026-11-09"]):
        weeks = snaps[:gi + 1]
        for pool, base_n in (("F", 160), ("D", 150), ("G", 20)):
            n = base_n + 40 * gi
            trust = "hidden" if pool == "G" else "provisional"
            per_week = [{"week": w, "n": 60, "mae": round(0.60 - 0.04 * i, 4)} for i, w in enumerate(weeks)]
            per_base = [{"week": w, "n_to_date": 60,
                         "base_mae": {"to_date": 0.70, "preseason": 0.66, "provider": round(0.64 + 0.01 * i, 4),
                                      "last_season": 0.72},
                         "skill": {"fm": round(1 - (0.60 - 0.04 * i) / 0.70, 4), "preseason": 0.057,
                                   "provider": round(1 - (0.64 + 0.01 * i) / 0.70, 4), "last_season": -0.03}}
                        for i, w in enumerate(weeks)]
            rows.append(_snap_row(week, league, "proj_fpg_mae", pool, per_week[-1]["mae"], n, trust,
                                  {"per_week": per_week, "per_week_base": per_base, "weeks": len(weeks),
                                   "need": max(0, 150 - n), "baselines": {}}))
            rows.append(_snap_row(week, league, "proj_fpg_skill", pool, per_base[-1]["skill"]["fm"], n, trust,
                                  {"need": max(0, 150 - n)}, 0.05, 0.2))
        for origin, n, k in (("followed", 12 + 5 * gi, 7 + 3 * gi), ("ignored", 6 + 2 * gi, 3 + gi),
                             ("partial", 2, 1), ("user_only", 10 + 6 * gi, 5 + 3 * gi)):
            trust = "hidden" if n < 20 else "provisional"
            rows.append(_snap_row(week, league, "hit_rate", f"waiver:{origin}", k / n, n, trust,
                                  {"hits": k, "mean_gain": 1.2, "units": "pts", "need": max(0, 20 - n)},
                                  *wilson(k, n)))
        rows.append(_snap_row(week, league, "hit_rate", "lineup:followed", 0.5, 4, "hidden",
                              {"hits": 2, "need": 16}))
        rows.append(_snap_row(week, league, "calibration", "all", 0.85, 120, "reliable",
                              {"bins": [{"bin": 1, "n": 40, "pred_lo": 0.1, "pred_hi": 1.0, "mean_pred": 0.5,
                                         "mean_real": 0.3, "hit_rate": 0.55},
                                        {"bin": 2, "n": 40, "pred_lo": 1.0, "pred_hi": 2.0, "mean_pred": 1.5,
                                         "mean_real": 1.4, "hit_rate": 0.6},
                                        {"bin": 3, "n": 40, "pred_lo": 2.0, "pred_hi": 5.0, "mean_pred": 3.1,
                                         "mean_real": 2.6, "hit_rate": 0.7}], "binning": "decile"}))
        rows.append(_snap_row(week, league, "counterfactual", "user_only", -0.8, 22, "provisional",
                              {"moves": 25, "my_mean": 1.1, "model_mean": 1.9, "model_better": 0.64, "need": 0}))
        rows.append(_snap_row(week, league, "trade_cases", "trade", None, 1, "cases",
                              {"cases": [{"title": "Trade A for B", "origin": "followed", "day": "2026-10-10",
                                          "window": "28d", "realized_gain": 3.5, "label": "complete"}]}))
    led.upsert("metric_snapshots", rows, ("snapshot_id",))
    led.upsert("rec_episodes", [{"episode_id": f"e{i}", "league": league, "rec_key": f"k{i}", "kind": kind,
                                 "first_seen": "2026-10-05", "last_seen": "2026-10-06", "n_days": 2, "status": st}
                                for i, (kind, st) in enumerate([("waiver", "followed"), ("waiver", "expired"),
                                                                ("lineup", "open"), ("trade", "open")])],
               ("episode_id",))
    led.upsert("transactions", [{"league": league, "tx_id": "t1", "action": "add", "cid": "c1", "team_id": "1",
                                 "ts": "2026-10-06T10:00", "day": "2026-10-06", "is_me": 1, "nhl_id": None}],
               ("league", "tx_id", "action", "cid", "team_id"))


def seed_params_versions(led):
    """v0001 applied then replaced by v0002 (active, parent v0001; v0001 is its shadow)."""
    from fantasy_manager.harness.params_store import ParamsStore

    st = ParamsStore(ledger=led)
    v1 = st.propose({"inseason": {"k_skater": 24.0}}, ["k_inseason.skater"],
                    {"holdout_before": 0.61, "holdout_after": 0.6, "hist_before": 0.5, "hist_after": 0.5012,
                     "n_live": 420})
    st.apply(v1["version"])
    v2 = st.propose({"inseason": {"k_skater": 22.0}}, ["k_inseason.skater"],
                    {"holdout_before": 0.6, "holdout_after": 0.59, "hist_before": 0.5012, "hist_after": 0.502,
                     "n_live": 520})
    st.apply(v2["version"])
    return st


@pytest.fixture
def seeded(isolated):
    from fantasy_manager.harness.ledger import Ledger

    with Ledger(isolated) as led:
        seed_health_ledger(led)
        seed_params_versions(led)
    return isolated


def test_health_page_charts_scorecard_changelog(seeded):
    loader = CountingLoader()
    c = TestClient(create_app(loader))
    h = c.get("/health?league=espn").text
    assert loader.calls == []                                               # never loads a league
    assert h.count('<svg class="spark"') == 5                               # MAE F/D, skill F/D, waiver hits
    assert '<title id="espn-mae-f-t">Forwards: projection MAE, next 28 days</title>' in h
    assert 'id="espn-hit-waiver"' in h and 'class="ch-band"' in h and "95% CI (Wilson)" in h
    assert "Goalies (n=100, needs 50 more player-windows)" in h            # hidden pool: listed, not drawn
    assert "Lineup (n=4, needs 16 more graded recs)" in h
    assert h.count("<summary>Data table</summary>") == 5
    # scorecard: followed = waiver 22/13 + lineup 4/2 (provisional); ignored 10 (hidden)
    assert '<th scope="row" data-label="Recommendations">Followed</th>' in h
    assert re.search(r'data-label="Graded">26</td>\s*<td class="n" data-label="Hits">15</td>', h)
    assert "needs 10 more" in h
    assert "your own waiver moves averaged 1.10 pts against 1.90 pts" in h
    assert "Mean realized" in h and "2.60" in h                            # calibration table
    # changelog: newest first, active highlighted, rollback form with confirm checkbox
    assert h.index('data-label="Version">v0002') < h.index('data-label="Version">v0001')
    assert "0.6000 &rarr; 0.5900" in h and "k_inseason.skater" in h
    assert 'action="/params/rollback"' in h and 'name="confirm" value="1" required' in h
    assert '<option value="v0001" selected>v0001 (parent)</option>' in h and '<option value="packaged">' in h
    assert "Active params" in h and "<strong>v0002</strong>" in h
    assert "Next refit" in h
    # data capture panel
    assert "Data capture" in h and "No daily capture has run yet" in h
    j = c.get("/api/health.json").json()
    L = j["leagues"]["espn"]
    assert {"projection", "hit_rates", "headline", "not_judgeable"} <= set(L)                  # M2 keys kept
    assert L["series"]["mae"]["F"]["x"] == ["2026-10-05", "2026-10-12", "2026-10-19"]
    assert L["series"]["mae"]["F"]["lines"]["provider"] == [0.64, 0.65, 0.66]
    assert L["series"]["hit"]["waiver"]["n_by_week"] == [20, 27, 34]
    assert [r["version"] for r in j["changelog"]["rows"]] == ["v0002", "v0001"]
    assert j["changelog"]["targets"][0] == "v0001" and j["calendar"]["refit_start"] == "2026-11-02"


def test_health_page_mobile_safe_markup(seeded):
    h = TestClient(create_app(CountingLoader())).get("/health?league=espn").text
    tag = h.split('<svg class="spark"')[1].split(">")[0]
    assert 'viewBox="0 0 360 132"' in tag and "width=" not in tag            # scales to its container
    assert 'class="r stack changelog"' in h                                 # stacked cards on phones


def test_rollback_requires_confirmation(seeded):
    from fantasy_manager.harness.params_store import ParamsStore

    c = TestClient(create_app(CountingLoader()))
    r = c.post("/params/rollback", data={"version": "v0001", "league": "espn", "next": "/health?league=espn"},
               follow_redirects=False)
    assert r.status_code == 303 and r.headers["location"] == "/health?league=espn&msg=rollback-unconfirmed"
    assert ParamsStore(data_dir=seeded).active_name() == "v0002"            # nothing changed
    page = c.get(r.headers["location"]).text
    assert 'class="flash flash-warn"' in page and "tick the confirmation box" in page


def test_rollback_post_rolls_back_and_redirects_same_site(seeded):
    from fantasy_manager.harness.params_store import ParamsStore

    loader = CountingLoader()
    c = TestClient(create_app(loader))
    c.get("/?league=espn")
    assert loader.calls == ["espn"]
    r = c.post("/params/rollback", data={"version": "v0001", "confirm": "1", "league": "espn",
                                         "next": "//evil.example/x"}, follow_redirects=False)
    assert r.status_code == 303 and r.headers["location"] == "/?league=espn&msg=rollback-ok-v0002-v0001"
    st = ParamsStore(data_dir=seeded)
    assert st.active_name() == "v0001" and st.get("v0002")["status"] == "rolled_back"
    assert st.get("v0001")["changelog"][-1]["by"] == "dashboard"
    c.get("/?league=espn")
    assert loader.calls == ["espn", "espn"]                                 # cached result dropped
    page = c.get("/health?league=espn&msg=rollback-ok-v0002-v0001").text
    assert "Rolled back the valuation parameters from v0002 to v0001" in page
    for bad in ("v9999", "../x"):                                           # unknown / malformed: no change
        r = c.post("/params/rollback", data={"version": bad, "confirm": "on", "league": "espn",
                                             "next": "/health?league=espn"}, follow_redirects=False)
        assert r.headers["location"].endswith("msg=rollback-unknown")
    assert ParamsStore(data_dir=seeded).active_name() == "v0001"
    r = c.post("/params/rollback", data={"version": "packaged", "confirm": "1", "league": "yahoo"},
               follow_redirects=False)
    assert r.status_code == 422


def test_rollback_when_packaged_is_active(isolated):
    c = TestClient(create_app(CountingLoader()))
    r = c.post("/params/rollback", data={"confirm": "1", "league": "fantrax", "next": "/health"},
               follow_redirects=False)
    assert r.headers["location"] == "/health?league=fantrax&msg=rollback-none"
    assert "Nothing to roll back" in c.get(r.headers["location"]).text


def test_refit_dry_run_shows_the_calendar_lock_and_proposal(seeded, monkeypatch):
    from fantasy_manager.harness import refit as R
    from fantasy_manager.harness.params_store import ParamsStore
    from fantasy_manager.web import app as web_app

    c = TestClient(create_app(CountingLoader()))
    h = c.get("/health?league=espn").text
    assert "Propose refit (dry run)" in h and 'name="refit" value="dry-run"' in h and "Dry-run proposal" not in h

    def run_at(day):
        def fake(force=False):
            from fantasy_manager.harness.ledger import Ledger

            with Ledger(seeded) as led:
                return R.run_refit(led, day, mode="dry-run", force=force).to_dict()
        return fake

    monkeypatch.setattr(web_app, "refit_dry_run", run_at(date(2026, 9, 29)))
    h = c.get("/health?league=espn&refit=dry-run").text
    assert "Dry-run proposal" in h and "locked until 2026-11-02" in h
    h = c.get("/health?league=espn&refit=dry-run&force=1").text            # lock bypassed: the gate reports
    assert "Gate: failed" in h and "insufficient live obs" in h and "nothing was written" in h
    assert ParamsStore(data_dir=seeded).versions()[-1]["version"] == "v0002"   # dry run wrote nothing

    fake = R.RefitResult(as_of="2026-11-16", mode="dry-run", n_live=640, n_goalie=160, weeks=6, n_week=700,
                         rows=[{"param": "k_inseason.skater", "group": "k_inseason", "tier": "A", "current": 25.0,
                                "candidate": 22.5, "bound": "+-20%", "changed": True, "holdout_before": 0.61,
                                "holdout_after": 0.6, "hist_before": 0.5, "hist_after": 0.501}],
                         gate=R.GateResult(True, [], {}), action="dry-run")
    monkeypatch.setattr(web_app, "refit_dry_run", lambda force=False: fake.to_dict())
    h = c.get("/health?league=espn&refit=dry-run").text
    assert "640 matured 28-day observations" in h
    assert '<td class="n" data-label="Candidate">22.5</td>' in h and "0.6100 → 0.6000" in h
    assert "Gate: passed" in h


def test_health_page_and_json_without_ledger_degrade(isolated):
    c = TestClient(create_app(CountingLoader()))
    j = c.get("/api/health.json").json()
    assert j["leagues"] == {} and j["params"]["version"] == "packaged" and j["changelog"]["rows"] == []
    assert j["calendar"]["refit_start"] == "2026-11-02"
    h = c.get("/health?league=espn").text
    assert "Not yet judgeable." in h and "No harness versions yet" in h and '<svg class="spark"' not in h
    assert 'action="/params/rollback"' not in h                             # nothing to roll back
    h = c.get("/health?league=espn&refit=dry-run").text
    assert "no harness ledger yet" in h
    assert not (isolated / "harness.db").exists()


# -- web/charts.py ------------------------------------------------------------------------

from fantasy_manager.web import charts  # noqa: E402


def test_sparkline_path_for_known_points():
    pts = charts.scale_points([0.0, 1.0, 0.5], 0.0, 1.0, width=110, height=60, pad=(10, 0, 10, 0))
    assert pts == [(10.0, 60.0), (60.0, 10.0), (110.0, 35.0)]
    assert charts.sparkline_path(pts) == "M10,60 L60,10 L110,35"
    gap = charts.scale_points([0.0, None, 1.0, 1.0], 0.0, 1.0, width=40, height=10, pad=(10, 0, 0, 0))
    assert charts.sparkline_path(gap) == "M10,10 M30,0 L40,0"                # a gap lifts the pen
    assert charts.x_positions(1, 100, 0, 0) == [50.0]


def test_empty_series_and_domain():
    assert charts.sparkline_path([]) == "" and charts.band_path([], []) == ""
    assert charts.domain([]) == (0.0, 1.0) and charts.domain([None, float("nan")]) == (0.0, 1.0)
    lo, hi = charts.domain([2.0, 2.0])
    assert lo < 2.0 < hi
    assert charts.domain([0.5, 1.0], include_zero=True)[0] < 0
    svg = charts.line_chart("empty", [], [charts.Line("fm", "fm", [], "main")], title="T", desc="D")
    assert '<title id="empty-t">T</title>' in svg and "<path" not in svg


def test_ci_band_and_chart_markup():
    lo = charts.scale_points([0.2, 0.3, None], 0.0, 1.0, width=30, height=10, pad=(10, 0, 0, 0))
    hi = charts.scale_points([0.6, 0.7, 0.8], 0.0, 1.0, width=30, height=10, pad=(10, 0, 0, 0))
    assert charts.band_path(lo, hi) == "M10,4 L20,3 L20,7 L10,8 Z"
    one = charts.band_path([(5.0, 8.0)], [(5.0, 2.0)])                      # single point: thin bar
    assert one == "M2,2 L8,2 L8,8 L2,8 Z"
    svg = charts.line_chart("w <x>", ["2026-10-05", "2026-10-12"],
                            [charts.Line("rate", "Hit rate", [0.5, 0.6], "main"),
                             charts.Line("b", "Base <b>", [0.4, None], "base", "5 3")],
                            title="Hit <rate>", desc="d", band=([0.3, 0.4], [0.7, 0.8]),
                            y_fmt=lambda v: f"{v:.0%}", y_domain=(0.0, 1.0))
    assert svg.startswith('<svg class="spark" id="w-x"') and 'aria-labelledby="w-x-t w-x-d"' in svg
    assert "Hit &lt;rate&gt;" in svg and "<b>" not in svg
    assert svg.index('class="ch-band"') < svg.index('class="ch-base"') < svg.index('class="ch-main"')
    assert 'stroke-dasharray="5 3"' in svg and ">Oct 5<" in svg and ">Oct 12<" in svg
    assert "<title>2026-10-12: Hit rate 60% (95% CI 40% to 80%)</title>" in svg
    assert ">100%<" in svg and ">0%<" in svg
    leg = charts.legend([charts.Line("b", "Base", [], "base", "5 3"), charts.Line("m", "Main", [], "main")], "CI")
    assert leg.index("Main") < leg.index("Base") < leg.index("CI")


def test_trade_acceptance_and_sweet_spot_on_moves_page():
    res = make_result()
    t = res.recs[1]
    assert t.kind == "trade"
    res.recs[1] = t.model_copy(update={"predicted_gain": 0.6, "gain_units": "lineup_fpg", "reasons": [
        Reason(code="MY_EDGE", text="You gain +0.60 pts/game this season", value=0.6),
        Reason(code="MARKET_VIEW", text="Looks even to them by market value (ADP/rostered %); acceptance ~70%",
               value=1.5),
        Reason(code="SWEET_SPOT", text="Sweet spot", value=0.6),
        Reason(code="P_ACCEPT", text="Acceptance 70%", value=0.7)]})
    from fantasy_manager.web import views

    info = views.trade_info(res.recs[1])
    assert info["p_text"] == "~70%" and info["ev"] == pytest.approx(0.42) and info["sweet"]
    assert views.trade_info(res.recs[0]) is None
    client = TestClient(create_app(lambda league: res))
    h = client.get("/recommendations?kind=trade").text
    assert "acceptance ~70%" in h and "expected value +0.42" in h
    assert "Sweet spot trades" in h and "Looks even to them by market value" in h
    assert "Sweet spot trades" not in client.get("/recommendations?kind=waiver").text


def test_trade_gain_units_and_exploit_section_on_moves_page(monkeypatch):
    import fantasy_manager.recommend.trades as tr

    res = make_result()
    t = res.recs[1]
    reasons = [Reason(code="MY_EDGE", text="You gain +0.66 pts/game this season", value=0.66),
               Reason(code="MARKET_VIEW", text="Looks even to them by market value (ADP); acceptance ~50%", value=0.0),
               Reason(code="P_ACCEPT", text="Acceptance 50%", value=0.5),
               Reason(code="GAIN_WEEK", text="+2.3 pts/week", value=2.31),
               Reason(code="GAIN_SEASON", text="+55 pts rest of season", value=54.8),
               Reason(code="DELTA_ME", text="My lineup +0.66 FPG", value=0.66)]
    res.recs[1] = t.model_copy(update={"predicted_gain": 0.66, "gain_units": "lineup_fpg", "reasons": reasons})
    ex = t.model_copy(update={"reasons": reasons[:2] + [Reason(code="EXPLOIT", text="Exploit: Team X has "
                                                               "3 goalies at the G limit 3", value=8.0)] + reasons[2:]})
    calls = []
    monkeypatch.setattr(tr, "exploit_opportunities", lambda *a, **k: calls.append(1) or [ex])
    client = TestClient(create_app(lambda league: res))
    h = client.get("/recommendations?kind=trade").text
    assert "you gain +2.3 pts/week · +55 pts rest of season" in h
    assert "Exploit trades" in h and "Exploit: Team X has 3 goalies at the G limit 3" in h
    assert "you gain +0.66 pts/game · +2.3 pts/week · +55 pts rest of season" in h
    client.get("/recommendations?kind=trade")
    assert len(calls) == 1                                           # memoised per loaded result
    assert "Exploit trades" not in client.get("/recommendations?kind=waiver").text
