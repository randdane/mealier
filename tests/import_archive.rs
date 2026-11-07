//! `import-archive` (spec, "Site archive and `import-archive`"): an archive.db built here with the crawler's schema.
mod common;
use std::path::PathBuf;

use mealier::archive::import_archive;
use mealier::{Db, Scope};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, SqliteConnection};

const SCHEMA: &str = "
CREATE TABLE queue (url TEXT PRIMARY KEY, site TEXT NOT NULL, queued_at TEXT NOT NULL);
CREATE TABLE pages (url TEXT PRIMARY KEY REFERENCES queue(url), final_url TEXT NOT NULL, fetched_at TEXT NOT NULL,
  status_code INTEGER NOT NULL, raw_html BLOB, json_ld TEXT,
  is_recipe INTEGER GENERATED ALWAYS AS (json_ld IS NOT NULL) STORED, mealier_imported_at TEXT);
CREATE TABLE failures (url TEXT PRIMARY KEY REFERENCES queue(url), attempted_at TEXT NOT NULL, reason TEXT NOT NULL, attempts INTEGER NOT NULL);
CREATE TABLE images (page_url TEXT PRIMARY KEY REFERENCES pages(url), src_url TEXT NOT NULL, fetched_at TEXT NOT NULL,
  status TEXT NOT NULL, content_type TEXT, bytes BLOB);";

struct Archive {
    _dir: tempfile::TempDir,
    path: PathBuf,
    conn: SqliteConnection,
}

impl Archive {
    async fn new() -> Archive {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.db");
        let mut conn = SqliteConnectOptions::new().filename(&path).create_if_missing(true).connect().await.unwrap();
        sqlx::raw_sql(SCHEMA).execute(&mut conn).await.unwrap();
        Archive { _dir: dir, path, conn }
    }

    /// A page fetched at `url` (final_url the same). `ld` and `html` may be absent.
    async fn page(&mut self, url: &str, status: i64, ld: Option<&str>, html: Option<&str>) {
        let raw = html.map(|h| zstd::encode_all(h.as_bytes(), 3).unwrap());
        sqlx::query("INSERT INTO queue VALUES (?, 'nyt', 'now')").bind(url).execute(&mut self.conn).await.unwrap();
        sqlx::query("INSERT INTO pages (url, final_url, fetched_at, status_code, raw_html, json_ld) VALUES (?, ?, 'now', ?, ?, ?)")
            .bind(url)
            .bind(url)
            .bind(status)
            .bind(raw)
            .bind(ld)
            .execute(&mut self.conn)
            .await
            .unwrap();
    }

    async fn image(&mut self, page: &str, status: &str, bytes: Option<Vec<u8>>) {
        sqlx::query("INSERT INTO images VALUES (?, 'http://img/x', 'now', ?, 'image/png', ?)").bind(page).bind(status).bind(bytes).execute(&mut self.conn).await.unwrap();
    }
}

fn ld(name: &str, keywords: &str) -> String {
    serde_json::json!({"@type": "Recipe", "name": name, "recipeIngredient": ["1 cup flour"], "recipeInstructions": ["Mix."], "keywords": keywords}).to_string()
}

/// A db with household `g/h` holding an enabled admin; returns its scope.
async fn db_with_household() -> (Db, Scope) {
    let db = Db::open_memory().await.unwrap();
    let g = db.create_group("g").await.unwrap();
    let h = db.create_household(g.id, "h").await.unwrap();
    sqlx::query("INSERT INTO users (username, email, role, household_id) VALUES ('boss', 'boss@x', 'admin', ?)").bind(h.id).execute(db.pool()).await.unwrap();
    let sc = Scope { user_id: 0, group_id: g.id, household_id: h.id, admin: false };
    (db, sc)
}

async fn run(db: &Db, a: &Archive, min_tag: u32) -> (String, mealier::archive::Report) {
    let (mut out, imgs) = (Vec::new(), tempfile::tempdir().unwrap());
    let r = import_archive(db, &a.path, "g/h", min_tag, imgs.path(), &mut out).await.unwrap();
    (String::from_utf8(out).unwrap(), r)
}

#[tokio::test]
async fn a_json_ld_page_is_imported_with_an_ok_line() {
    let (db, sc) = db_with_household().await;
    let mut a = Archive::new().await;
    a.page("https://cooking.nytimes.com/recipes/1-soup", 200, Some(&ld("Soup", "")), None).await;
    let (out, _) = run(&db, &a, 20).await;
    assert_eq!(out, "ok\thttps://cooking.nytimes.com/recipes/1-soup\n");
    let list = db.list_recipes(&sc, false, None).await.unwrap();
    assert_eq!(list.len(), 1);
    let r = db.get_recipe(&sc, list[0].id).await.unwrap().unwrap();
    assert_eq!(r.content.name, "Soup");
    assert_eq!(r.content.source_url.as_deref(), Some("https://cooking.nytimes.com/recipes/1-soup"));
}

#[tokio::test]
async fn a_rerun_reports_dup_and_adds_nothing_even_when_the_url_is_spelled_differently() {
    let (db, sc) = db_with_household().await;
    let mut a = Archive::new().await;
    a.page("https://cooking.nytimes.com/recipes/1-soup", 200, Some(&ld("Soup", "")), None).await;
    // a recipe already in the group (e.g. from Mealie) whose source differs only by case, query and slash
    db.create_recipe(&sc, mealier::RecipeInput { name: "Old".into(), source_url: Some("HTTPS://Cooking.NYTimes.com/recipes/2-pie/?utm=x#top".into()), ..Default::default() }).await.unwrap();
    a.page("https://cooking.nytimes.com/recipes/2-pie", 200, Some(&ld("Pie", "")), None).await;
    let (out, r) = run(&db, &a, 20).await;
    assert_eq!(out, "ok\thttps://cooking.nytimes.com/recipes/1-soup\ndup\thttps://cooking.nytimes.com/recipes/2-pie\n");
    assert_eq!((r.imported, r.dup), (1, 1));
    let (out, r) = run(&db, &a, 20).await;
    assert_eq!(out, "dup\thttps://cooking.nytimes.com/recipes/1-soup\ndup\thttps://cooking.nytimes.com/recipes/2-pie\n");
    assert_eq!((r.imported, r.dup), (0, 2));
    assert_eq!(db.list_recipes(&sc, false, None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn microdata_is_read_from_the_stored_html_when_there_is_no_json_ld() {
    let (db, sc) = db_with_household().await;
    let mut a = Archive::new().await;
    let html = r#"<html><body><div itemscope itemtype="https://schema.org/Recipe"><h1 itemprop="name">Micro Stew</h1>
        <span itemprop="recipeIngredient">2 cups water</span><span itemprop="recipeInstructions">Boil.</span></div></body></html>"#;
    a.page("https://cooking.nytimes.com/recipes/3-stew", 200, None, Some(html)).await;
    a.page("https://cooking.nytimes.com/recipes/4-blank", 200, None, Some("<html><body>nothing</body></html>")).await;
    let (out, r) = run(&db, &a, 20).await;
    assert_eq!(out, "ok\thttps://cooking.nytimes.com/recipes/3-stew\n");
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].0, "https://cooking.nytimes.com/recipes/4-blank");
    let list = db.list_recipes(&sc, false, None).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(db.get_recipe(&sc, list[0].id).await.unwrap().unwrap().content.name, "Micro Stew");
}

#[tokio::test]
async fn a_404_row_is_ignored_even_if_it_carries_json_ld() {
    let (db, sc) = db_with_household().await;
    let mut a = Archive::new().await;
    a.page("https://cooking.nytimes.com/recipes/5-gone", 404, Some(&ld("Gone", "")), None).await;
    let (out, r) = run(&db, &a, 20).await;
    assert_eq!(out, "");
    assert_eq!((r.imported, r.dup, r.skipped.len()), (0, 0, 0));
    assert!(db.list_recipes(&sc, false, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_keyword_becomes_a_tag_only_on_enough_distinct_pages() {
    let (db, sc) = db_with_household().await;
    let mut a = Archive::new().await;
    // "quick" is on pages A and B (twice on A, in two cases); "soup" only on A; "dinner" twice on C alone.
    a.page("https://x.test/recipes/a", 200, Some(&ld("A", "Soup, Quick, quick")), None).await;
    a.page("https://x.test/recipes/b", 200, Some(&ld("B", " QUICK ,Vegan")), None).await;
    a.page("https://x.test/recipes/c", 200, Some(&ld("C", "dinner, Dinner")), None).await;
    run(&db, &a, 2).await;
    let tags = |name: &'static str| {
        let (db, sc) = (db.clone(), sc);
        async move {
            let l = db.list_recipes(&sc, false, None).await.unwrap();
            let id = l.iter().find(|r| r.name == name).unwrap().id;
            db.get_recipe(&sc, id).await.unwrap().unwrap().content.tags
        }
    };
    assert_eq!(tags("A").await, ["quick"]);
    assert_eq!(tags("B").await, ["quick"]);
    assert!(tags("C").await.is_empty());
}

#[tokio::test]
async fn an_ok_image_is_attached_and_a_rejected_one_leaves_the_recipe_without() {
    let (db, sc) = db_with_household().await;
    let mut a = Archive::new().await;
    for (n, name) in [("a", "Good"), ("b", "Bad"), ("c", "Missing")] {
        a.page(&format!("https://x.test/recipes/{n}"), 200, Some(&ld(name, "")), None).await;
    }
    a.image("https://x.test/recipes/a", "ok", Some(common::png(8, 8, 1))).await;
    a.image("https://x.test/recipes/b", "ok", Some(b"not an image at all".to_vec())).await;
    a.image("https://x.test/recipes/c", "http 404", None).await;
    let (out, r) = run(&db, &a, 20).await;
    assert_eq!(out.lines().count(), 3, "every page is still imported: {out}");
    assert_eq!(r.image_rejected.len(), 1);
    assert_eq!(r.image_rejected[0].0, "https://x.test/recipes/b");
    let images = |name: &'static str| {
        let (db, sc) = (db.clone(), sc);
        async move {
            let l = db.list_recipes(&sc, false, None).await.unwrap();
            let id = l.iter().find(|r| r.name == name).unwrap().id;
            db.get_recipe(&sc, id).await.unwrap().unwrap().content.images
        }
    };
    assert_eq!(images("Good").await.len(), 1);
    assert!(images("Bad").await.is_empty());
    assert!(images("Missing").await.is_empty());
}

#[tokio::test]
async fn it_fails_without_an_enabled_admin_in_the_household() {
    let db = Db::open_memory().await.unwrap();
    let g = db.create_group("g").await.unwrap();
    let h = db.create_household(g.id, "h").await.unwrap();
    for (name, role, disabled) in [("off", "admin", 1), ("plain", "user", 0)] {
        sqlx::query("INSERT INTO users (username, email, role, disabled, household_id) VALUES (?, ?, ?, ?, ?)")
            .bind(name).bind(format!("{name}@x")).bind(role).bind(disabled).bind(h.id).execute(db.pool()).await.unwrap();
    }
    let mut a = Archive::new().await;
    a.page("https://x.test/recipes/a", 200, Some(&ld("A", "")), None).await;
    let (mut out, imgs) = (Vec::new(), tempfile::tempdir().unwrap());
    let err = import_archive(&db, &a.path, "g/h", 20, imgs.path(), &mut out).await.unwrap_err();
    assert!(err.to_string().contains("no enabled admin"), "{err}");
    assert!(out.is_empty());
    let err = import_archive(&db, &a.path, "nope/h", 20, imgs.path(), &mut out).await.unwrap_err();
    assert!(matches!(err, mealier::Error::NotFound(_)), "{err}");
}

#[tokio::test]
async fn the_archive_is_opened_read_only_and_left_untouched() {
    use std::os::unix::fs::PermissionsExt;
    let (db, _) = db_with_household().await;
    let mut a = Archive::new().await;
    sqlx::query("PRAGMA journal_mode = WAL").execute(&mut a.conn).await.unwrap();
    a.page("https://x.test/recipes/a", 200, Some(&ld("A", "")), None).await;
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&mut a.conn).await.unwrap();
    a.conn.close().await.unwrap();
    let dir = a.path.parent().unwrap().to_path_buf();
    let names = |d: &std::path::Path| {
        let mut v: Vec<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name()).collect();
        v.sort();
        v
    };
    let (before, listing) = (std::fs::read(&a.path).unwrap(), names(&dir));
    let set = |d: u32, f: u32| {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(d)).unwrap();
        std::fs::set_permissions(&a.path, std::fs::Permissions::from_mode(f)).unwrap();
    };
    set(0o555, 0o444);
    if std::fs::File::create(dir.join("probe")).is_ok() {
        set(0o755, 0o644);
        eprintln!("running as root: permissions do not bind, skipping");
        return;
    }
    let (mut out, imgs) = (Vec::new(), tempfile::tempdir().unwrap());
    let res = import_archive(&db, &a.path, "g/h", 20, imgs.path(), &mut out).await;
    let (after, listing_after) = (std::fs::read(&a.path).unwrap(), names(&dir));
    set(0o755, 0o644); // restore before asserting so the tempdir can be removed
    res.unwrap();
    assert_eq!(after, before);
    assert_eq!(listing_after, listing, "no -wal or -shm file appears next to it");
}

#[tokio::test]
async fn the_subcommand_prints_ok_lines_on_stdout_and_the_report_on_stderr() {
    use std::process::{Command, Stdio};
    let data = tempfile::tempdir().unwrap();
    {
        let db = Db::open(&data.path().join("mealier.db")).await.unwrap();
        let g = db.create_group("g").await.unwrap();
        let h = db.create_household(g.id, "h").await.unwrap();
        sqlx::query("INSERT INTO users (username, email, role, household_id) VALUES ('boss', 'boss@x', 'admin', ?)").bind(h.id).execute(db.pool()).await.unwrap();
    }
    let mut a = Archive::new().await;
    a.page("https://x.test/recipes/a", 200, Some(&ld("A", "quick")), None).await;
    a.conn.close().await.unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mealier"))
        .args(["import-archive"]).arg(&a.path).args(["--household", "g/h", "--min-tag-count", "1"])
        .env("MEALIER_DATA_DIR", data.path())
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("import-archive did not exit (fell through to the server?)");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout), "ok\thttps://x.test/recipes/a\n");
    assert!(String::from_utf8_lossy(&o.stderr).contains("imported"), "{}", String::from_utf8_lossy(&o.stderr));
}

#[tokio::test]
async fn ok_and_dup_lines_carry_the_archive_key_not_the_final_url() {
    let (db, _) = db_with_household().await;
    let mut a = Archive::new().await;
    a.page("https://cooking.nytimes.com/recipes/1-soup", 200, Some(&ld("Soup", "")), None).await;
    sqlx::query("UPDATE pages SET final_url = 'https://cooking.nytimes.com/recipes/99-renamed'").execute(&mut a.conn).await.unwrap();
    let (out, _) = run(&db, &a, 20).await;
    assert_eq!(out, "ok\thttps://cooking.nytimes.com/recipes/1-soup\n");
    let (out, _) = run(&db, &a, 20).await;
    assert_eq!(out, "dup\thttps://cooking.nytimes.com/recipes/1-soup\n");
}

/// The same literal list sits in tools/test_site_archive.py; keep them identical.
const NORMALIZE_CASES: [(&str, &str); 8] = [
    ("HTTPS://Cooking.NYTimes.com:443/recipes/1-soup/?x=1#y", "https://cooking.nytimes.com/recipes/1-soup"),
    ("http://Example.com:80/a/b/", "http://example.com/a/b"),
    ("https://example.com:8443/a", "https://example.com:8443/a"),
    ("http://example.com:443/a", "http://example.com:443/a"),
    ("https://example.com/", "https://example.com/"),
    ("https://example.com", "https://example.com/"),
    ("https://example.com/a//", "https://example.com/a"),
    ("https://example.com/a?q=1", "https://example.com/a"),
];

#[test]
fn normalize_matches_the_crawler_on_the_shared_cases() {
    for (input, expected) in NORMALIZE_CASES {
        assert_eq!(mealier::archive::normalize(input), expected, "{input}");
    }
}

#[tokio::test]
async fn an_undecodable_page_is_reported_failed_and_the_run_goes_on() {
    let (db, sc) = db_with_household().await;
    let mut a = Archive::new().await;
    a.page("https://x.test/recipes/a", 200, None, Some("<html></html>")).await;
    sqlx::query("UPDATE pages SET raw_html = X'0102030405' WHERE url = 'https://x.test/recipes/a'").execute(&mut a.conn).await.unwrap();
    a.page("https://x.test/recipes/b", 200, Some(&ld("B", "")), None).await;
    let (out, r) = run(&db, &a, 20).await;
    assert_eq!(out, "ok\thttps://x.test/recipes/b\n");
    assert_eq!((r.imported, r.failed.len()), (1, 1));
    assert_eq!(r.failed[0].0, "https://x.test/recipes/a");
    assert_eq!(db.list_recipes(&sc, false, None).await.unwrap().len(), 1);
}
