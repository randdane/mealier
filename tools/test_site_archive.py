import json
import sqlite3
import zstandard
import httpx
import pytest

import site_archive as sa

HOST = sa.SITE["host"]
BASE = f"https://{HOST}"
SITEMAP = sa.SITE["sitemap_url"]


class Net:
    """Route table over MockTransport. routes: url -> Response | callable(request) | Exception."""

    def __init__(self, routes=None):
        self.routes = dict(routes or {})
        self.requests = []
        self.client = httpx.Client(transport=httpx.MockTransport(self.handle))

    def handle(self, request):
        self.requests.append(request)
        r = self.routes.get(str(request.url))
        if r is None:
            return httpx.Response(404)
        if isinstance(r, Exception):
            raise r
        if callable(r):
            return r(request)
        return r

    def hits(self, url):
        return [q for q in self.requests if str(q.url) == url]


class Clock:
    def __init__(self):
        self.waits = []

    def __call__(self, s):
        self.waits.append(s)

    @property
    def long(self):  # waits that are not ordinary pacing
        return [w for w in self.waits if w >= 60]


def xml_urlset(*urls):
    body = "".join(f"<url><loc>{u}</loc></url>" for u in urls)
    return httpx.Response(200, text=f'<urlset>{body}</urlset>')


def xml_index(*urls):
    body = "".join(f"<sitemap><loc>{u}</loc></sitemap>" for u in urls)
    return httpx.Response(200, text=f'<sitemapindex>{body}</sitemapindex>')


def rows(db, sql, *args):
    c = sqlite3.connect(db)
    try:
        return c.execute(sql, args).fetchall()
    finally:
        c.close()


def test_seed_follows_index_queues_recipes_and_normalizes(tmp_path):
    db = tmp_path / "a.db"
    net = Net({
        SITEMAP: xml_index(f"{BASE}/sm1.xml", f"{BASE}/sm2.xml"),
        f"{BASE}/sm1.xml": xml_urlset(
            f"{BASE}/recipes/1-soup/?utm=x#frag", f"{BASE}/about"),
        f"{BASE}/sm2.xml": xml_urlset(f"HTTPS://{HOST.upper()}/recipes/2-stew"),
    })
    sa.seed(db, net.client, Clock())
    assert rows(db, "select url from queue order by url") == [
        (f"{BASE}/recipes/1-soup",), (f"{BASE}/recipes/2-stew",)]
    sa.seed(db, net.client, Clock())  # safe to run again
    assert len(rows(db, "select * from queue")) == 2


def recipe_html(ld, extra=""):
    return (f'<html><head><script type="application/ld+json">{json.dumps(ld)}</script>'
            f'</head><body>{extra}</body></html>')


def queue(db, *paths):
    with sa.session(db) as c:
        for p in paths:
            c.execute("INSERT OR IGNORE INTO queue VALUES (?,?,?)", (BASE + p, HOST, "t"))
    return [BASE + p for p in paths]


def test_crawl_stores_zstd_html_final_url_and_json_ld_graph_and_list_type(tmp_path):
    db = tmp_path / "a.db"
    u1, u2 = queue(db, "/recipes/1-soup", "/recipes/2-stew")
    graph = {"@graph": [{"@type": "WebPage"}, {"@type": "Recipe", "name": "Soup"}]}
    listed = {"@type": ["Thing", "Recipe"], "name": "Stew"}
    net = Net({
        u1: httpx.Response(200, html=recipe_html(graph)),
        u2: httpx.Response(301, headers={"location": "/recipes/22-stew/"}),
        f"{BASE}/recipes/22-stew/": httpx.Response(200, html=recipe_html(listed)),
    })
    sa.crawl(db, net.client, Clock())
    got = {u: (s, f, zstandard.decompress(h).decode(), json.loads(j), r) for u, s, f, h, j, r in
           rows(db, "select url,status_code,final_url,raw_html,json_ld,is_recipe from pages")}
    assert got[u1][0] == 200 and got[u1][1] == u1 and "<html>" in got[u1][2]
    assert got[u1][3]["name"] == "Soup" and got[u1][4] == 1
    assert got[u2][1] == f"{BASE}/recipes/22-stew" and got[u2][3]["name"] == "Stew"


def test_crawl_non_recipe_200_has_null_json_ld(tmp_path):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    net = Net({u: httpx.Response(200, html="<html>no ld</html>")})
    sa.crawl(db, net.client, Clock())
    assert rows(db, "select status_code, json_ld, is_recipe from pages") == [(200, None, 0)]


def test_crawl_404_is_terminal_with_no_html(tmp_path):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    net = Net({u: httpx.Response(404, html="gone")})
    clock = Clock()
    sa.crawl(db, net.client, clock)
    sa.crawl(db, net.client, clock)
    assert rows(db, "select status_code, raw_html from pages") == [(404, None)]
    assert len(net.requests) == 1 and rows(db, "select * from failures") == []


def test_network_error_retries_once_after_60s_then_failure_row(tmp_path):
    db = tmp_path / "a.db"
    u1, u2 = queue(db, "/recipes/1-x", "/recipes/2-y")
    net = Net({u1: httpx.ConnectError("boom"), u2: httpx.Response(200, html="<p>ok</p>")})
    clock = Clock()
    sa.crawl(db, net.client, clock)
    assert len(net.hits(u1)) == 2 and clock.long == [60]
    assert rows(db, "select url, reason, attempts from failures") == [(u1, "ConnectError", 2)]
    assert rows(db, "select url from pages") == [(u2,)]  # moved on


def test_retry_that_succeeds_stores_page_and_no_failure(tmp_path):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    answers = iter([httpx.Response(502), httpx.Response(200, html="<p>ok</p>")])
    net = Net({u: lambda req: next(answers)})
    clock = Clock()
    sa.crawl(db, net.client, clock)
    assert clock.long == [60]
    assert rows(db, "select status_code from pages") == [(200,)]
    assert rows(db, "select * from failures") == []


def test_five_failures_in_a_row_abort(tmp_path):
    db = tmp_path / "a.db"
    urls = queue(db, *[f"/recipes/{i}-x" for i in range(7)])
    net = Net({u: httpx.Response(500) for u in urls})
    with pytest.raises(sa.Abort, match="5 failures"):
        sa.crawl(db, net.client, Clock())
    assert len(rows(db, "select * from failures")) == 5
    assert len(net.requests) == 10  # two tries each, the sixth URL never tried


def test_success_resets_the_failure_streak(tmp_path):
    db = tmp_path / "a.db"
    paths = [f"/recipes/{i}-x" for i in range(9)]
    urls = queue(db, *paths)
    bad = {u: httpx.Response(500) for i, u in enumerate(urls) if i != 4}
    net = Net({**bad, urls[4]: httpx.Response(200, html="ok")})
    sa.crawl(db, net.client, Clock())  # 4 fail, 1 ok, 4 fail: no abort
    assert len(rows(db, "select * from failures")) == 8


def test_429_waits_60_120_300_600_then_succeeds(tmp_path):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    answers = iter([httpx.Response(429)] * 4 + [httpx.Response(200, html="ok")])
    net = Net({u: lambda req: next(answers)})
    clock = Clock()
    sa.crawl(db, net.client, clock)
    assert clock.long == [60, 120, 300, 600]
    assert rows(db, "select status_code from pages") == [(200,)]


def test_fifth_429_on_one_url_aborts(tmp_path):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    net = Net({u: httpx.Response(429)})
    clock = Clock()
    with pytest.raises(sa.Abort, match="429"):
        sa.crawl(db, net.client, clock)
    assert clock.long == [60, 120, 300, 600] and len(net.requests) == 5
    assert rows(db, "select * from pages") == [] and rows(db, "select * from failures") == []


def test_429_on_a_second_url_in_the_batch_aborts(tmp_path):
    db = tmp_path / "a.db"
    u1, u2, u3 = queue(db, "/recipes/1-x", "/recipes/2-x", "/recipes/3-x")
    first = iter([httpx.Response(429), httpx.Response(200, html="ok")])
    net = Net({u1: lambda req: next(first), u2: httpx.Response(429), u3: httpx.Response(200)})
    clock = Clock()
    with pytest.raises(sa.Abort, match="second"):
        sa.crawl(db, net.client, clock)
    assert clock.long == [60]  # u1 waited; u2 aborted without waiting
    assert rows(db, "select url from pages") == [(u1,)]


@pytest.mark.parametrize("answer", [
    httpx.Response(403),
    httpx.Response(401),
    httpx.Response(302, headers={"location": f"{BASE}/login?next=/recipes/1-x"}),
    httpx.Response(200, html="<html>Please verify you are human: px-captcha</html>"),
], ids=["403", "401", "login-redirect", "marker"])
def test_challenge_aborts_immediately_and_stores_nothing(tmp_path, answer):
    db = tmp_path / "a.db"
    u1, u2 = queue(db, "/recipes/1-x", "/recipes/2-x")
    net = Net({u1: answer, f"{BASE}/login?next=/recipes/1-x": httpx.Response(200, html="login"),
               u2: httpx.Response(200, html="ok")})
    clock = Clock()
    with pytest.raises(sa.Abort):
        sa.crawl(db, net.client, clock)
    assert rows(db, "select * from pages") == [] and rows(db, "select * from failures") == []
    assert clock.long == [] and net.hits(u2) == []


def test_resume_skips_fetched_and_failed_then_requeue_retries_failed(tmp_path):
    db = tmp_path / "a.db"
    u1, u2, u3 = queue(db, "/recipes/1-x", "/recipes/2-x", "/recipes/3-x")
    with sa.session(db) as c:
        c.execute("insert into pages (url,final_url,fetched_at,status_code) values (?,?,?,?)",
                  (u1, u1, "t", 200))
        c.execute("insert into failures values (?,?,?,?)", (u2, "t", "timeout", 2))
    net = Net({u2: httpx.Response(200, html="ok"), u3: httpx.Response(200, html="ok")})
    sa.crawl(db, net.client, Clock())
    assert [str(q.url) for q in net.requests] == [u3]
    sa.requeue_failed(db)
    assert rows(db, "select * from failures") == []
    sa.crawl(db, net.client, Clock())
    assert [str(q.url) for q in net.requests] == [u3, u2]


def test_crawl_limit(tmp_path):
    db = tmp_path / "a.db"
    queue(db, "/recipes/1-x", "/recipes/2-x", "/recipes/3-x")
    net = Net({})
    sa.crawl(db, net.client, Clock(), limit=2)
    assert len(net.requests) == 2


def test_cookie_goes_to_site_host_only_and_never_on_seed(tmp_path, capsys):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    net = Net({
        SITEMAP: xml_urlset(u),
        u: httpx.Response(302, headers={"location": "https://other.example/recipes/1-x"}),
        "https://other.example/recipes/1-x": httpx.Response(200, html="ok"),
    })
    sa.seed(db, net.client, Clock())
    assert "cookie" not in net.requests[0].headers
    with pytest.raises(sa.Abort):  # redirected off the site: a gate, not a recipe
        sa.crawl(db, net.client, Clock(), cookie="SECRET")
    assert net.requests[1].headers["cookie"] == "NYT-S=SECRET"
    assert "user-agent" in net.requests[1].headers
    assert "cookie" not in net.requests[2].headers
    out = capsys.readouterr()
    assert "SECRET" not in out.out + out.err


def page(db, n, ld):
    (u,) = queue(db, f"/recipes/{n}-x")
    with sa.session(db) as c:
        c.execute("insert into pages (url,final_url,fetched_at,status_code,json_ld) values (?,?,?,?,?)",
                  (u, u, "t", 200, None if ld is None else json.dumps(ld)))
    return u


def test_fetch_images_ok_and_cdn_redirect_without_cookie(tmp_path):
    db = tmp_path / "a.db"
    u1 = page(db, 1, {"@type": "Recipe", "image": ["https://img.example/a.jpg"]})
    u2 = page(db, 2, {"@type": "Recipe", "image": {"url": f"{BASE}/img/b.jpg"}})
    page(db, 3, {"@type": "Recipe"})  # no image url
    page(db, 4, None)                 # not a recipe
    net = Net({
        "https://img.example/a.jpg": httpx.Response(200, content=b"AAA", headers={"content-type": "image/jpeg"}),
        f"{BASE}/img/b.jpg": httpx.Response(302, headers={"location": "https://cdn.example/b.jpg"}),
        "https://cdn.example/b.jpg": httpx.Response(200, content=b"BBB", headers={"content-type": "image/webp"}),
    })
    sa.fetch_images(db, net.client, Clock())
    assert rows(db, "select page_url, src_url, status, content_type, bytes from images order by page_url") == [
        (u1, "https://img.example/a.jpg", "ok", "image/jpeg", b"AAA"),
        (u2, f"{BASE}/img/b.jpg", "ok", "image/webp", b"BBB")]
    assert all("cookie" not in q.headers for q in net.requests)
    assert len(net.requests) == 3


def test_fetch_images_404_and_non_image_are_recorded_and_not_retried(tmp_path):
    db = tmp_path / "a.db"
    u1 = page(db, 1, {"@type": "Recipe", "image": "https://img.example/gone.jpg"})
    u2 = page(db, 2, {"@type": "Recipe", "image": "https://img.example/page.html"})
    net = Net({
        "https://img.example/gone.jpg": httpx.Response(404),
        "https://img.example/page.html": httpx.Response(200, html="<p>hi</p>"),
    })
    clock = Clock()
    sa.fetch_images(db, net.client, clock)
    sa.fetch_images(db, net.client, clock)
    assert rows(db, "select page_url, status, bytes from images order by page_url") == [
        (u1, "http 404", None), (u2, "not an image", None)]
    assert len(net.requests) == 2 and clock.long == []


def test_fetch_images_final_failure_leaves_no_row_so_next_run_retries(tmp_path):
    db = tmp_path / "a.db"
    page(db, 1, {"@type": "Recipe", "image": "https://img.example/a.jpg"})
    net = Net({"https://img.example/a.jpg": httpx.Response(503)})
    clock = Clock()
    sa.fetch_images(db, net.client, clock)
    assert rows(db, "select * from images") == [] and clock.long == [60]
    sa.fetch_images(db, net.client, clock)
    assert len(net.requests) == 4


def test_mark_imported_reads_ok_and_dup_lines_only(tmp_path):
    db = tmp_path / "a.db"
    u1, u2, u3 = (page(db, i, {"@type": "Recipe"}) for i in (1, 2, 3))
    f = tmp_path / "imported.txt"
    f.write_text(f"ok\t{u1}\ndup\t{u2}/\nskip\t{u3}\nnoise\n\n")
    sa.mark_imported(db, f)
    assert rows(db, "select url, mealier_imported_at is not null from pages order by url") == [
        (u1, 1), (u2, 1), (u3, 0)]


def test_prune_html_only_imported_with_json_ld_and_dry_run(tmp_path, capsys):
    db = tmp_path / "a.db"
    keep_unimported = page(db, 1, {"@type": "Recipe"})
    keep_no_ld = page(db, 2, None)
    prune = page(db, 3, {"@type": "Recipe"})
    with sa.session(db) as c:
        c.execute("update pages set raw_html = x'01', mealier_imported_at = 't' where url != ?",
                  (keep_unimported,))
        c.execute("update pages set raw_html = x'01' where url = ?", (keep_unimported,))
    sa.prune_html(db, dry_run=True)
    assert "1" in capsys.readouterr().out
    assert rows(db, "select count(*) from pages where raw_html is not null") == [(3,)]
    sa.prune_html(db)
    assert rows(db, "select url from pages where raw_html is null") == [(prune,)]
    assert keep_no_ld and keep_unimported


def test_stats_counts_by_state(tmp_path):
    db = tmp_path / "a.db"
    r = page(db, 1, {"@type": "Recipe"})
    page(db, 2, None)
    queue(db, "/recipes/3-x", "/recipes/4-x")
    with sa.session(db) as c:
        c.execute("insert into pages (url,final_url,fetched_at,status_code) values (?,?,?,?)",
                  (BASE + "/recipes/3-x", "x", "t", 404))
        c.execute("insert into failures values (?,?,?,?)", (BASE + "/recipes/4-x", "t", "timeout", 2))
        c.execute("update pages set mealier_imported_at='t' where url=?", (r,))
        c.execute("insert into images values (?,?,?,?,?,?)", (r, "s", "t", "ok", "image/jpeg", b"x"))
    assert sa.stats(db) == {"queued": 4, "fetched": 3, "recipe": 1, "not a recipe": 1,
                            "gone": 1, "failed": 1, "imported": 1, "images ok": 1}


def test_pacing_between_requests_and_long_pause_every_300_to_500(tmp_path):
    db = tmp_path / "a.db"
    queue(db, *[f"/recipes/{i}-x" for i in range(520)])
    clock = Clock()
    sa.crawl(db, Net({}).client, clock)  # every page 404s, instantly
    pauses = [w for w in clock.waits if w >= 600]
    short = [w for w in clock.waits if w < 60]
    assert len(pauses) == 1 and 600 <= pauses[0] <= 900
    assert len(short) == 519 and all(2.5 <= w <= 4.5 for w in short)


def test_connection_enforces_foreign_keys_and_leaves_checkpointed_wal(tmp_path):
    db = tmp_path / "a.db"
    queue(db, "/recipes/1-x")
    with pytest.raises(sqlite3.IntegrityError):
        with sa.session(db) as c:
            c.execute("insert into pages (url,final_url,fetched_at,status_code) values ('nope','x','t',200)")
    sa.crawl(db, Net({}).client, Clock())
    assert rows(db, "pragma journal_mode") == [("wal",)]
    wal = tmp_path / "a.db-wal"
    assert not wal.exists() or wal.stat().st_size == 0


# The same literal list sits in tests/import_archive.rs; keep them identical.
NORMALIZE_CASES = [
    ("HTTPS://Cooking.NYTimes.com:443/recipes/1-soup/?x=1#y", "https://cooking.nytimes.com/recipes/1-soup"),
    ("http://Example.com:80/a/b/", "http://example.com/a/b"),
    ("https://example.com:8443/a", "https://example.com:8443/a"),
    ("http://example.com:443/a", "http://example.com:443/a"),
    ("https://example.com/", "https://example.com/"),
    ("https://example.com", "https://example.com/"),
    ("https://example.com/a//", "https://example.com/a"),
    ("https://example.com/a?q=1", "https://example.com/a"),
]


@pytest.mark.parametrize("given,expected", NORMALIZE_CASES)
def test_normalize_matches_the_importer(given, expected):
    assert sa.normalize(given) == expected


def test_429_count_survives_the_retry_after_a_network_error(tmp_path):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    answers = iter([httpx.Response(429)] * 3 + [httpx.ConnectError("x")]
                   + [httpx.Response(429)] * 2 + [httpx.Response(200, html="ok")])

    def answer(req):
        a = next(answers)
        if isinstance(a, Exception):
            raise a
        return a

    net = Net({u: answer})
    with pytest.raises(sa.Abort, match="fifth 429"):
        sa.crawl(db, net.client, Clock())
    assert rows(db, "select * from pages") == []


def test_image_fetch_answered_with_a_challenge_page_aborts(tmp_path):
    db = tmp_path / "a.db"
    page(db, 1, {"@type": "Recipe", "image": "https://img.example/a.jpg"})
    net = Net({"https://img.example/a.jpg": httpx.Response(
        200, text="<html>px-captcha</html>", headers={"content-type": "text/html"})})
    with pytest.raises(sa.Abort, match="challenge"):
        sa.fetch_images(db, net.client, Clock())
    assert rows(db, "select * from images") == []


def test_redirect_loop_is_a_failure_row_not_a_crash(tmp_path):
    db = tmp_path / "a.db"
    (u,) = queue(db, "/recipes/1-x")
    net = Net({u: httpx.Response(302, headers={"location": u})})
    sa.crawl(db, net.client, Clock())
    assert rows(db, "select url, reason from failures") == [(u, "too many redirects")]


@pytest.mark.parametrize("attr", ["type='application/ld+json'", "type=application/ld+json",
                                  'id="x" type="application/ld+json" async'])
def test_json_ld_tag_attribute_quoting(attr):
    html = f'<script {attr}>{{"@type": "Recipe", "name": "Q"}}</script>'
    assert json.loads(sa.find_recipe(html))["name"] == "Q"
