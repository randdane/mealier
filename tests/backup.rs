//! Export and restore (spec, "Backup and export"): a zip of a `VACUUM INTO` copy and every image,
//! swapped in only after it has been checked, with the old data kept.
mod common;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use mealier::backup::{export, lock_data_dir, restore, restore_offline};
use mealier::{Db, Error, RecipeInput};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;

/// A data directory the way the server lays it out: `mealier.db` and `images/`.
struct Site {
    root: tempfile::TempDir,
    data: PathBuf,
}

impl Site {
    fn new() -> Site {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir(&data).unwrap();
        Site { root, data }
    }
    fn path(&self) -> &Path {
        &self.data
    }
    /// Beside the data directory, not in it.
    fn outside(&self) -> &Path {
        self.root.path()
    }
    fn images(&self) -> PathBuf {
        self.path().join("images")
    }
    async fn db(&self) -> Db {
        Db::open(&self.path().join("mealier.db")).await.unwrap()
    }
}

/// A library worth saving: two recipes (one with a replaced image, so an image only an old snapshot lists),
/// a batch with two photos, and a shopping list.
async fn filled(site: &Site) -> Db {
    let db = site.db().await;
    let sc = common::scope(&db).await;
    let soup = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), steps: vec!["Stir.".into()], ..Default::default() }).await.unwrap();
    db.create_recipe(&sc, RecipeInput { name: "Stew".into(), ..Default::default() }).await.unwrap();
    db.set_recipe_image(&sc, soup.id, &site.images(), &common::png(30, 20, 1)).await.unwrap();
    db.set_recipe_image(&sc, soup.id, &site.images(), &common::png(30, 20, 2)).await.unwrap();
    let batch = db.start_batch(&sc, soup.id, 2.0).await.unwrap().batch.id;
    db.add_batch_photo(&sc, batch, &site.images(), &common::jpeg(40, 40, 100)).await.unwrap();
    db.add_batch_photo(&sc, batch, &site.images(), &common::webp(40, 40, 200)).await.unwrap();
    db.create_list(&sc, "Weekly").await.unwrap();
    db
}

async fn count(db: &Db, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(db.pool()).await.unwrap()
}

async fn snapshots(db: &Db) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM recipe_snapshots ORDER BY id").fetch_all(db.pool()).await.unwrap()
}

/// File name -> bytes of everything in a directory.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.unwrap()).filter(|e| e.path().is_file()).map(|e| (e.file_name().into_string().unwrap(), std::fs::read(e.path()).unwrap())).collect(),
        Err(_) => BTreeMap::new(),
    }
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Everything under a data directory that restore must leave alone on failure.
fn fingerprint(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            // SQLite removes these itself as the last connection closes, a moment after `close()` returns.
            if rel.ends_with("-wal") || rel.ends_with("-shm") {
                continue;
            }
            if p.is_dir() {
                out.insert(rel, "dir".into());
                walk(root, &p, out);
            } else {
                out.insert(rel, sha(&std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))));
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

async fn exported(site: &Site, db: &Db) -> PathBuf {
    let out = site.outside().join("export.zip");
    export(db.pool(), site.path(), &out).await.unwrap();
    out
}

/// The entries of a zip as (name, bytes).
fn entries(zip: &Path) -> Vec<(String, Vec<u8>)> {
    let mut a = zip::ZipArchive::new(std::fs::File::open(zip).unwrap()).unwrap();
    (0..a.len())
        .map(|i| {
            let mut f = a.by_index(i).unwrap();
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            (f.name().unwrap().to_string(), b)
        })
        .collect()
}

fn write_zip(path: &Path, entries: &[(String, Vec<u8>)]) {
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        w.start_file(name.as_str(), SimpleFileOptions::default()).unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap();
}

fn invalid<T: std::fmt::Debug>(r: Result<T, Error>) -> String {
    match r {
        Err(Error::Validation(m)) => m,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

// ---------- the round trip ----------

#[tokio::test]
async fn export_then_restore_into_a_fresh_directory_gives_the_same_library() {
    let a = Site::new();
    let db = filled(&a).await;
    let zip = exported(&a, &db).await;
    assert!(!a.path().join("export-staging").exists() || std::fs::read_dir(a.path().join("export-staging")).unwrap().next().is_none(), "the staging copy is cleaned up");

    let b = Site::new();
    let restored = restore(&zip, b.path()).await.unwrap();
    assert!(restored.previous.is_none(), "nothing to keep from an empty directory");
    let copy = b.db().await;

    for table in ["recipes", "recipe_snapshots", "recipe_images", "batches", "batch_lines", "batch_photos", "images", "shopping_lists"] {
        assert_eq!(count(&copy, table).await, count(&db, table).await, "{table}");
    }
    assert_eq!(count(&copy, "images").await, 4, "two recipe images (one only an old snapshot lists) and two photos");
    assert_eq!(snapshots(&copy).await, snapshots(&db).await);
    assert_eq!(files(&b.images()), files(&a.images()), "every image, byte for byte");
    assert_eq!(files(&b.images()).len(), 4);
    assert!(!b.path().join("restore-staging").exists());
}

#[tokio::test]
async fn restoring_over_existing_data_keeps_the_old_data_in_a_pre_restore_directory() {
    let a = Site::new();
    let db = filled(&a).await;
    let zip = exported(&a, &db).await;

    let b = Site::new();
    let old = b.db().await;
    old.create_recipe(&common::scope(&old).await, RecipeInput { name: "Old Recipe".into(), ..Default::default() }).await.unwrap();
    old.add_batch_photo(&common::scope(&old).await, {
        let r = old.create_recipe(&common::scope(&old).await, RecipeInput { name: "Other".into(), ..Default::default() }).await.unwrap();
        old.start_batch(&common::scope(&old).await, r.id, 1.0).await.unwrap().batch.id
    }, &b.images(), &common::png(9, 9, 77)).await.unwrap();
    let old_images = files(&b.images());
    old.pool().close().await;
    let unrelated = b.path().join("notes.txt");
    std::fs::write(&unrelated, "keep me").unwrap();

    let restored = restore(&zip, b.path()).await.unwrap();

    let previous = restored.previous.expect("the old data was kept");
    assert!(previous.starts_with(b.path()) && previous.file_name().unwrap().to_str().unwrap().starts_with("pre-restore-"));
    assert_eq!(files(&previous.join("images")), old_images);
    let kept = Db::open(&previous.join("mealier.db")).await.unwrap();
    let names: Vec<String> = kept.list_recipes(&common::scope(&kept).await, true, None).await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["Old Recipe", "Other"]);

    let now = b.db().await;
    let names: Vec<String> = now.list_recipes(&common::scope(&now).await, true, None).await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["Soup", "Stew"]);
    assert_eq!(files(&b.images()), files(&a.images()), "the images are the backup's, not a mixture");
    assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "keep me", "files restore does not own are left alone");
}

// ---------- refusals leave the current data alone ----------

/// Restores `zip` over a site that has data and expects a refusal that changes nothing. Returns the message.
async fn refused(zip: &Path) -> String {
    let b = Site::new();
    let db = b.db().await;
    db.create_recipe(&common::scope(&db).await, RecipeInput { name: "Mine".into(), ..Default::default() }).await.unwrap();
    // Fold the write-ahead log in, so closing the pool leaves the database file as it is.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(db.pool()).await.unwrap();
    db.pool().close().await;
    std::fs::create_dir_all(b.images()).unwrap();
    std::fs::write(b.images().join("keep.txt"), "x").unwrap();
    let before = fingerprint(b.path());

    let msg = invalid(restore(zip, b.path()).await);

    assert_eq!(fingerprint(b.path()), before, "current data untouched, no staging left behind");
    assert!(!b.outside().join("evil").exists() && !Path::new("/tmp/evil-absolute").exists(), "nothing written outside the data dir");
    msg
}

#[tokio::test]
async fn a_zip_with_an_unsafe_path_is_refused_and_nothing_changes() {
    let a = Site::new();
    let db = filled(&a).await;
    let good = entries(&exported(&a, &db).await);
    let t = tempfile::tempdir().unwrap();
    for bad in ["../evil", "/tmp/evil-absolute", "images/../../evil", "images/..%2Fevil", "evil", "images/notahash.png", "images/sub/dir.png"] {
        let mut es = good.clone();
        es.push((bad.to_string(), b"pwned".to_vec()));
        let zip = t.path().join("bad.zip");
        write_zip(&zip, &es);
        let msg = refused(&zip).await;
        assert!(msg.contains(bad), "{bad}: {msg}");
    }
}

#[tokio::test]
async fn a_zip_missing_an_image_is_refused() {
    let a = Site::new();
    let db = filled(&a).await;
    let mut es = entries(&exported(&a, &db).await);
    let gone = es.iter().position(|(n, _)| n.starts_with("images/")).unwrap();
    let (name, _) = es.remove(gone);
    let t = tempfile::tempdir().unwrap();
    let zip = t.path().join("short.zip");
    write_zip(&zip, &es);
    let msg = refused(&zip).await;
    assert!(msg.contains(name.strip_prefix("images/").unwrap().split('.').next().unwrap()), "{msg}");
}

#[tokio::test]
async fn an_image_whose_bytes_do_not_match_its_name_is_refused() {
    let a = Site::new();
    let db = filled(&a).await;
    let mut es = entries(&exported(&a, &db).await);
    let i = es.iter().position(|(n, _)| n.starts_with("images/")).unwrap();
    es[i].1.push(0);
    let t = tempfile::tempdir().unwrap();
    let zip = t.path().join("tampered.zip");
    write_zip(&zip, &es);
    assert!(refused(&zip).await.contains("sha256"));
}

#[tokio::test]
async fn a_zip_that_is_not_a_mealier_export_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let zip = t.path().join("x.zip");
    write_zip(&zip, &[]);
    assert!(refused(&zip).await.contains("no mealier.db"));
    write_zip(&zip, &[("mealier.db".into(), b"this is not sqlite".to_vec())]);
    refused(&zip).await;
    std::fs::write(&zip, b"PK nope").unwrap();
    assert!(refused(&zip).await.contains("zip"));
}

#[tokio::test]
async fn a_database_from_a_newer_build_is_refused() {
    let a = Site::new();
    let db = filled(&a).await;
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (999999, 'from the future', 1, x'00', 0)").execute(db.pool()).await.unwrap();
    let zip = exported(&a, &db).await;
    let msg = refused(&zip).await;
    assert!(msg.contains("newer"), "{msg}");
}

#[tokio::test]
async fn a_database_with_no_migrations_table_is_refused() {
    let a = Site::new();
    let db = filled(&a).await;
    let mut es = entries(&exported(&a, &db).await);
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("plain.db");
    let c = sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f).create_if_missing(true)).await.unwrap();
    sqlx::query("CREATE TABLE recipes (id INTEGER PRIMARY KEY)").execute(&c).await.unwrap();
    c.close().await;
    es.iter_mut().find(|(n, _)| n == "mealier.db").unwrap().1 = std::fs::read(&f).unwrap();
    let zip = t.path().join("plain.zip");
    write_zip(&zip, &es);
    assert!(refused(&zip).await.contains("migrations"));
}

// ---------- export ----------

#[tokio::test]
async fn export_names_only_what_restore_reads_and_images_are_stored_whole() {
    let a = Site::new();
    let db = filled(&a).await;
    let es = entries(&exported(&a, &db).await);
    let names: Vec<&str> = es.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.iter().filter(|n| **n == "mealier.db").count(), 1);
    assert_eq!(names.iter().filter(|n| n.starts_with("images/")).count(), 4);
    assert_eq!(names.len(), 5, "{names:?}");
    for (n, b) in es.iter().filter(|(n, _)| n.starts_with("images/")) {
        assert_eq!(n.strip_prefix("images/").unwrap().split('.').next().unwrap(), sha(b));
    }
}

#[tokio::test]
async fn export_fails_loudly_when_an_image_file_has_gone_missing() {
    let a = Site::new();
    let db = filled(&a).await;
    let victim = std::fs::read_dir(a.images()).unwrap().next().unwrap().unwrap().path();
    std::fs::remove_file(&victim).unwrap();
    let out = a.outside().join("missing.zip");
    let msg = invalid(export(db.pool(), a.path(), &out).await);
    assert!(msg.contains(victim.file_stem().unwrap().to_str().unwrap()), "{msg}");
    assert!(!out.exists(), "no half-made zip left behind");
}

#[tokio::test]
async fn export_does_not_block_writers() {
    let a = Site::new();
    let db = filled(&a).await;
    let out = a.outside().join("concurrent.zip");
    let (exported, ()) = tokio::join!(export(db.pool(), a.path(), &out), async {
        for n in 0..20 {
            db.create_list(&common::scope(&db).await, &format!("during {n}")).await.unwrap();
        }
    });
    exported.unwrap();
    assert_eq!(count(&db, "shopping_lists").await, 21);
}

// ---------- the CLI's restore wants the server stopped ----------

#[tokio::test]
async fn the_offline_restore_refuses_while_a_server_holds_the_data_directory() {
    let a = Site::new();
    let db = filled(&a).await;
    let zip = exported(&a, &db).await;

    let b = Site::new();
    let theirs = filled(&b).await;
    theirs.pool().close().await;
    let before = fingerprint(b.path());
    let held = lock_data_dir(b.path()).unwrap();

    let msg = invalid(restore_offline(&zip, b.path()).await);
    assert!(msg.contains("stop"), "{msg}");
    assert!(lock_data_dir(b.path()).is_err(), "and a second server cannot start either");
    drop(held);
    let after_lock = fingerprint(b.path());
    assert_eq!(after_lock.keys().filter(|k| !k.ends_with(".lock")).count(), before.keys().count());

    restore_offline(&zip, b.path()).await.unwrap();
    let now = b.db().await;
    assert_eq!(count(&now, "recipes").await, 2);
    // and the lock is free again afterwards
    lock_data_dir(b.path()).unwrap();
}
