#!/usr/bin/env -S uv run --python 3.15
# /// script
# requires-python = ">=3.15"
# dependencies = ["httpx[http2]", "zstandard"]
# ///
"""Slow, resumable crawl of a recipe site into SQLite. See docs/spec.md, "Site archive"."""
import argparse
import json
import os
import random
import gzip
import re
import sqlite3
import sys
import time
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urljoin, urlsplit

import httpx
import zstandard

SITE = {
    "host": "cooking.nytimes.com",
    "recipe_path": "/recipes/",
    "sitemap_url": "https://www.nytimes.com/sitemaps/new/cooking.xml.gz",  # from robots.txt, 2026-11-07
    "cookie_name": "NYT-S",
    # Copy these from the owner's real browser; no TLS impersonation.
    "headers": {
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) "
                      "Chrome/156.0.0.0 Safari/537.36",
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        "Accept-Language": "en-US,en;q=0.9",
        "Sec-Ch-Ua": '"Not:A-Brand";v="8", "Chromium";v="156", "Google Chrome";v="156"',
        "Sec-Ch-Ua-Mobile": "?0",
        "Sec-Ch-Ua-Platform": '"Linux"',
        "Sec-Fetch-Dest": "document",
        "Sec-Fetch-Mode": "navigate",
        "Sec-Fetch-Site": "none",
        "Sec-Fetch-User": "?1",
    },
    "challenge_markers": ["px-captcha", "Access to this page has been denied", "cf-challenge"],
}

SCHEMA = """
CREATE TABLE IF NOT EXISTS queue (url TEXT PRIMARY KEY, site TEXT NOT NULL, queued_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS pages (
  url TEXT PRIMARY KEY REFERENCES queue(url),
  final_url TEXT NOT NULL, fetched_at TEXT NOT NULL, status_code INTEGER NOT NULL,
  raw_html BLOB, json_ld TEXT,
  is_recipe INTEGER GENERATED ALWAYS AS (json_ld IS NOT NULL) STORED,
  mealier_imported_at TEXT);
CREATE TABLE IF NOT EXISTS failures (
  url TEXT PRIMARY KEY REFERENCES queue(url),
  attempted_at TEXT NOT NULL, reason TEXT NOT NULL, attempts INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS images (
  page_url TEXT PRIMARY KEY REFERENCES pages(url),
  src_url TEXT NOT NULL, fetched_at TEXT NOT NULL, status TEXT NOT NULL,
  content_type TEXT, bytes BLOB);
"""


def now():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def normalize(url):
    """Lowercase scheme and host, default port, query, fragment and trailing slash dropped (bare "/" kept)."""
    p = urlsplit(url.strip())
    host = p.hostname or ""
    if ":" in host:
        host = f"[{host}]"
    if p.port and p.port != {"http": 80, "https": 443}.get(p.scheme.lower()):
        host += f":{p.port}"
    return f"{p.scheme.lower()}://{host}{p.path.rstrip('/') or '/'}"


def is_recipe_url(site, url):
    p = urlsplit(url)
    return p.hostname == site["host"] and p.path.startswith(site["recipe_path"])


def check_streak(f):
    if f.streak >= 5:
        raise Abort("5 failures in a row")


@contextmanager
def session(db):
    conn = sqlite3.connect(db)
    conn.execute("PRAGMA foreign_keys = ON")
    conn.execute("PRAGMA journal_mode = WAL")
    conn.executescript(SCHEMA)
    try:
        yield conn
    finally:
        conn.commit()
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        conn.close()


WAITS_429 = (60, 120, 300, 600)


class Abort(Exception):
    """Stop the run; the next one resumes where this stopped."""


def log(msg):
    """Progress on stderr. Never pass the cookie or request headers."""
    print(f"{datetime.now():%H:%M:%S} {msg}", file=sys.stderr, flush=True)


class Failed(Exception):
    """A URL that failed twice; the message is the reason stored in failures."""


class Fetcher:
    def __init__(self, client, sleep, site=SITE, cookie=None):
        self.client, self.sleep, self.site = client, sleep, site
        self.cookie = cookie
        self.sent = 0
        self.pause_at = random.randint(300, 500)
        self.streak = 0  # URLs in a row that ended in a failure
        self.n429 = {}  # 429s met per URL in this batch, kept across the retry

    def get(self, url, ok=(200, 404, 410), kind="page"):
        """Returns (response, final_url). One retry after 60 s, then raises Failed."""
        try:
            try:
                got = self._try(url, ok, kind)
            except Failed as e:
                log(f"  {e} on {url}; retrying in 60 s")
                self.sleep(60)
                got = self._try(url, ok, kind)
        except Failed:
            self.streak += 1
            raise
        self.streak = 0
        return got

    def _try(self, url, ok, kind):
        try:
            r, final = self._follow(url)
        except httpx.HTTPError as e:
            raise Failed(type(e).__name__) from e
        if r.status_code not in ok:
            raise Failed(f"http {r.status_code}")
        if kind == "page":
            if not is_recipe_url(self.site, final):
                raise Abort(f"{url} redirected to {final}")
        if r.status_code == 200 and (kind != "image" or r.headers.get("content-type", "").startswith("text/html")):
            body = r.text
            for m in self.site["challenge_markers"]:
                if m in body:
                    raise Abort(f"challenge marker {m!r} on {url}")
        return r, final

    def _headers(self, url):
        h = dict(self.site["headers"])
        if self.cookie and urlsplit(url).hostname == self.site["host"]:
            h["Cookie"] = f"{self.site['cookie_name']}={self.cookie}"
        return h

    def _pace(self):
        if self.sent:
            self.sleep(random.uniform(2.5, 4.5))
        if self.sent and self.sent % self.pause_at == 0:
            pause = random.uniform(600, 900)
            log(f"batch done; resting {pause / 60:.0f} min")
            self.sleep(pause)
            self.pause_at = random.randint(300, 500)
            self.sent = 0
        self.sent += 1

    def _send(self, url):
        while True:
            self._pace()
            r = self.client.get(url, headers=self._headers(url))
            if r.status_code in (401, 403):
                raise Abort(f"http {r.status_code} on {url}")
            if r.status_code != 429:
                return r
            if url not in self.n429 and self.n429:
                raise Abort(f"429 on a second URL ({url})")
            self.n429[url] = self.n429.get(url, 0) + 1
            if self.n429[url] > len(WAITS_429):
                raise Abort(f"fifth 429 on {url}")
            log(f"  429 on {url}; waiting {WAITS_429[self.n429[url] - 1]} s")
            self.sleep(WAITS_429[self.n429[url] - 1])

    def _follow(self, url):
        """Follow redirects by hand."""
        for _ in range(10):
            r = self._send(url)
            if r.status_code not in (301, 302, 303, 307, 308):
                return r, url
            url = urljoin(url, r.headers["location"])
        raise Failed("too many redirects")


def seed(db, client, sleep, site=SITE):
    f = Fetcher(client, sleep, site)
    with session(db) as conn:
        todo, seen = [site["sitemap_url"]], 0
        while todo:
            seen += 1
            sm = todo.pop()
            r = f.get(sm, (200,), "sitemap")[0]
            raw = r.content
            body = (gzip.decompress(raw) if raw[:2] == b"\x1f\x8b" else raw).decode("utf-8", "replace")
            locs = re.findall(r"<loc>\s*(.*?)\s*</loc>", body)
            if "<sitemapindex" in body:
                todo += locs
                log(f"sitemap {seen}/{seen + len(todo)}: index, {len(locs)} sitemaps  {sm}")
                continue
            added = 0
            for u in map(normalize, locs):
                if is_recipe_url(site, u):
                    added += conn.execute("INSERT OR IGNORE INTO queue VALUES (?,?,?)",
                                          (u, site["host"], now())).rowcount
            conn.commit()
            log(f"sitemap {seen}/{seen + len(todo)}: +{added} recipe URLs  {sm}")


LD_RE = re.compile(r'<script[^>]*type=["\']?application/ld\+json["\']?(?=[\s>/])[^>]*>(.*?)</script>', re.S)


def find_recipe(html):
    """The Recipe object from JSON-LD (plain, list or @graph; @type may be a list), as JSON text."""
    todo = []
    for m in LD_RE.findall(html):
        try:
            todo.append(json.loads(m))
        except ValueError:
            pass
    while todo:
        o = todo.pop(0)
        if isinstance(o, list):
            todo += o
        elif isinstance(o, dict):
            t = o.get("@type")
            if t == "Recipe" or (isinstance(t, list) and "Recipe" in t):
                return json.dumps(o)
            todo += o.get("@graph", [])
    return None


def crawl(db, client, sleep, limit=None, site=SITE, cookie=None):
    f = Fetcher(client, sleep, site, cookie)
    with session(db) as conn:
        urls = [r[0] for r in conn.execute(
            "SELECT url FROM queue WHERE url NOT IN (SELECT url FROM pages)"
            " AND url NOT IN (SELECT url FROM failures) ORDER BY url LIMIT ?",
            (limit if limit else -1,))]
        for i, url in enumerate(urls, 1):
            try:
                r, final = f.get(url)
            except Failed as e:
                log(f"[{i}/{len(urls)}] failed ({e})  {url}")
                conn.execute("INSERT OR REPLACE INTO failures VALUES (?,?,?,2)",
                             (url, now(), str(e)))
                conn.commit()
                check_streak(f)
                continue
            html = r.text if r.status_code == 200 else None
            conn.execute(
                "INSERT INTO pages (url, final_url, fetched_at, status_code, raw_html, json_ld)"
                " VALUES (?,?,?,?,?,?)",
                (url, normalize(final), now(), r.status_code,
                 zstandard.compress(r.content) if html else None,
                 ld := find_recipe(html) if html else None))
            conn.commit()
            log(f"[{i}/{len(urls)}] {r.status_code} {'recipe' if ld else 'no JSON-LD'}  {url}")


def requeue_failed(db):
    with session(db) as conn:
        n = conn.execute("DELETE FROM failures").rowcount
    print(f"requeued {n}")


def image_url(ld):
    img = ld.get("image")
    while isinstance(img, list) and img:
        img = img[0]
    if isinstance(img, dict):
        img = img.get("url")
    return img if isinstance(img, str) and img else None


def fetch_images(db, client, sleep, limit=None, site=SITE):
    f = Fetcher(client, sleep, site)  # no cookie: images may live on any host
    with session(db) as conn:
        todo = conn.execute(
            "SELECT url, json_ld FROM pages WHERE json_ld IS NOT NULL"
            " AND url NOT IN (SELECT page_url FROM images) ORDER BY url").fetchall()
        done = 0
        for url, ld in todo:
            src = image_url(json.loads(ld))
            if not src:
                continue
            if limit and done >= limit:
                break
            done += 1
            try:
                r, _ = f.get(src, kind="image")
            except Failed:
                check_streak(f)
                continue
            ctype = r.headers.get("content-type", "")
            if r.status_code != 200:
                status, data = f"http {r.status_code}", None
            elif ctype.startswith("image/"):
                status, data = "ok", r.content
            else:
                status, data = "not an image", None
            conn.execute("INSERT INTO images VALUES (?,?,?,?,?,?)",
                         (url, src, now(), status, ctype or None, data))
            conn.commit()


def mark_imported(db, path):
    with session(db) as conn:
        n = 0
        with open(path) as lines:
            for line in lines:
                kind, _, url = line.rstrip("\n").partition("\t")
                if kind in ("ok", "dup") and url:
                    n += conn.execute(
                        "UPDATE pages SET mealier_imported_at = ? WHERE url = ?",
                        (now(), normalize(url))).rowcount
    print(f"marked {n}")


def prune_html(db, dry_run=False):
    where = "raw_html IS NOT NULL AND mealier_imported_at IS NOT NULL AND json_ld IS NOT NULL"
    with session(db) as conn:
        n = conn.execute(f"SELECT count(*) FROM pages WHERE {where}").fetchone()[0]
        print(f"{'would prune' if dry_run else 'pruning'} {n} pages")
        if not dry_run:
            conn.execute(f"UPDATE pages SET raw_html = NULL WHERE {where}")
    return n


def stats(db):
    q = {
        "queued": "SELECT count(*) FROM queue",
        "fetched": "SELECT count(*) FROM pages",
        "recipe": "SELECT count(*) FROM pages WHERE is_recipe",
        "not a recipe": "SELECT count(*) FROM pages WHERE status_code = 200 AND NOT is_recipe",
        "gone": "SELECT count(*) FROM pages WHERE status_code IN (404, 410)",
        "failed": "SELECT count(*) FROM failures",
        "imported": "SELECT count(*) FROM pages WHERE mealier_imported_at IS NOT NULL",
    }
    with session(db) as conn:
        out = {k: conn.execute(v).fetchone()[0] for k, v in q.items()}
        for status, n in conn.execute("SELECT status, count(*) FROM images GROUP BY status"):
            out[f"images {status}"] = n
    for k, v in out.items():
        print(f"{k:14} {v}")
    return out


def data_dir(name):
    return Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share") / "_r_" / name


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=data_dir("site-archive") / "archive.db")
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("seed", "crawl", "fetch-images", "requeue-failed", "mark-imported",
                 "prune-html", "stats"):
        p = sub.add_parser(name)
        if name in ("crawl", "fetch-images"):
            p.add_argument("--limit", type=int)
        if name == "mark-imported":
            p.add_argument("file")
        if name == "prune-html":
            p.add_argument("--dry-run", action="store_true")
    a = ap.parse_args(argv)
    db = Path(a.db)
    db.parent.mkdir(parents=True, exist_ok=True)
    try:
        if a.cmd == "requeue-failed":
            requeue_failed(db)
        elif a.cmd == "mark-imported":
            mark_imported(db, a.file)
        elif a.cmd == "prune-html":
            prune_html(db, a.dry_run)
        elif a.cmd == "stats":
            stats(db)
        else:
            with httpx.Client(http2=True, timeout=30) as client:
                if a.cmd == "seed":
                    seed(db, client, time.sleep)
                elif a.cmd == "crawl":
                    cookie = (data_dir("nyt") / "cookie").read_text().strip()
                    crawl(db, client, time.sleep, a.limit, cookie=cookie)
                else:
                    fetch_images(db, client, time.sleep, a.limit)
    except (Abort, Failed) as e:
        sys.exit(f"ABORT: {e}")


if __name__ == "__main__":
    main()
