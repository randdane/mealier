//! Migration 0009 (groups and households): a database holding data at 0008 is upgraded without loss,
//! keeps its constraints and triggers, and a fresh one gets no tenants until one is needed.
use std::borrow::Cow;
use std::path::Path;

use mealier::Db;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Connection};

/// A connection to a file DB migrated only up to 0008.
async fn at_0008(path: &Path) -> sqlx::SqliteConnection {
    let mut m = sqlx::migrate::Migrator::new(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))).await.unwrap();
    m.migrations = Cow::Owned(m.migrations.iter().filter(|x| x.version <= 8).cloned().collect());
    let mut c = SqliteConnectOptions::new().filename(path).create_if_missing(true).journal_mode(SqliteJournalMode::Wal).foreign_keys(true).connect().await.unwrap();
    m.run(&mut c).await.unwrap();
    c
}

/// A file DB migrated only up to 0008 and filled with a row in every table 0009 rebuilds.
async fn db_at_0008(path: &Path) {
    let mut c = at_0008(path).await;
    for sql in [
        "INSERT INTO users (id, username, email, role) VALUES (1, 'ann', 'ann@x', 'user')",
        "INSERT INTO users (id, username, email, role, local_admin) VALUES (2, 'admin', 'local-admin@mealier.invalid', 'admin', 1)",
        "INSERT INTO sessions (id_hash, user_id, expires_at) VALUES ('s1', 1, '2999-01-01T00:00:00.000Z')",
        "INSERT INTO api_tokens (hash, user_id, name) VALUES ('t1', 1, 'cli')",
        "INSERT INTO units (id, name, dimension, factor) VALUES (1, 'g', 'mass', 1.0), (2, 'pinch', 'other', NULL)",
        "INSERT INTO foods (id, name) VALUES (1, 'flour'), (2, 'salt')",
        "INSERT INTO tags (id, name) VALUES (1, 'quick')",
        "INSERT INTO categories (id, name) VALUES (1, 'dinner')",
        "INSERT INTO cookbooks (id, name) VALUES (1, 'weeknight')",
        "INSERT INTO tools (id, name) VALUES (1, 'whisk')",
        "INSERT INTO recipes (id, name) VALUES (1, 'Bread'), (2, 'Soup')",
        "INSERT INTO recipe_snapshots (id, recipe_id, content) VALUES (1, 1, '{}'), (2, 2, '{}')",
        "UPDATE recipes SET current_snapshot_id = id",
        "INSERT INTO images (hash, mime) VALUES ('h1', 'image/png')",
        "INSERT INTO recipe_images (recipe_id, hash) VALUES (1, 'h1')",
        "INSERT INTO recipe_comments (recipe_id, user_id, text, created_at) VALUES (1, 1, 'nice', '2026-01-01')",
        "INSERT INTO batches (id, status, recipe_id, snapshot_id, source, made_on) VALUES (1, 'planned', 1, 1, 'native', NULL), (2, 'done', 2, NULL, 'imported', '2026-01-02')",
        "INSERT INTO batch_lines (id, batch_id, qty_min, qty_max, unit_id, food_id) VALUES (1, 1, 250, 250, 1, 1)",
        "INSERT INTO batch_photos (batch_id, hash) VALUES (1, 'h1')",
        "INSERT INTO meal_plan_entries (id, date, slot, recipe_id, batch_id) VALUES (1, '2026-12-01', 'dinner', 1, 1)",
        "INSERT INTO shopping_lists (id, name) VALUES (1, 'Weekly')",
        "INSERT INTO shopping_list_items (id, list_id, qty, unit_id, food_id, recipe_id) VALUES (1, 1, 2, 1, 1, 1)",
        "INSERT INTO webhooks (name, kind, url, events) VALUES ('n', 'json', 'http://x', '[]')",
    ] {
        sqlx::query(sql).execute(&mut c).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    c.close().await.unwrap();
}

async fn n(db: &Db, sql: &str) -> Option<i64> {
    sqlx::query_scalar(sql).fetch_one(db.pool()).await.ok()
}

async fn s(db: &Db, sql: &str) -> Option<String> {
    sqlx::query_scalar(sql).fetch_one(db.pool()).await.ok()
}

async fn fails(db: &Db, sql: &str) -> bool {
    sqlx::query(sql).execute(db.pool()).await.is_err()
}

async fn works(db: &Db, sql: &str) -> bool {
    let r = sqlx::query(sql).execute(db.pool()).await;
    if let Err(e) = &r {
        eprintln!("{sql}: {e}");
    }
    r.is_ok()
}

async fn upgraded() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mealier.db");
    db_at_0008(&path).await;
    (Db::open(&path).await.unwrap(), dir)
}

#[tokio::test]
async fn a_fresh_database_has_the_tables_but_no_tenant() {
    let db = Db::open_memory().await.unwrap();
    assert_eq!(n(&db, "SELECT count(*) FROM groups").await, Some(0));
    assert_eq!(n(&db, "SELECT count(*) FROM households").await, Some(0));
    assert_eq!(n(&db, "SELECT count(*) FROM instance_settings").await, Some(0));
    assert_eq!(n(&db, "SELECT count(*) FROM group_preferences").await, Some(0));
    assert_eq!(n(&db, "SELECT count(*) FROM household_preferences").await, Some(0));
}

#[tokio::test]
async fn scoping_columns_are_not_null() {
    let db = Db::open_memory().await.unwrap();
    for (table, col) in [
        ("recipes", "group_id"),
        ("units", "group_id"),
        ("foods", "group_id"),
        ("tags", "group_id"),
        ("categories", "group_id"),
        ("tools", "group_id"),
        ("cookbooks", "group_id"),
        ("batches", "household_id"),
        ("meal_plan_entries", "household_id"),
        ("shopping_lists", "household_id"),
        ("users", "household_id"),
    ] {
        let sql = format!("SELECT \"notnull\" FROM pragma_table_info('{table}') WHERE name = '{col}'");
        assert_eq!(n(&db, &sql).await, Some(1), "{table}.{col} must be NOT NULL");
    }
}

#[tokio::test]
async fn existing_rows_move_into_one_home_group_and_household() {
    let (db, _dir) = upgraded().await;
    assert_eq!(s(&db, "SELECT group_concat(name) FROM groups").await.as_deref(), Some("Home"));
    assert_eq!(s(&db, "SELECT group_concat(name) FROM households").await.as_deref(), Some("Home"));
    assert_eq!(n(&db, "SELECT count(*) FROM households h JOIN groups g ON g.id = h.group_id WHERE g.slug = 'home' AND h.slug = 'home'").await, Some(1));
    assert_eq!(
        s(&db, "SELECT value FROM instance_settings WHERE key = 'default_household_id'").await,
        s(&db, "SELECT CAST(id AS TEXT) FROM households").await,
        "the default household is the one made for the old data"
    );
    assert_eq!(n(&db, "SELECT count(*) FROM group_preferences").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM household_preferences").await, Some(1));

    for (table, rows) in [("recipes", 2), ("units", 2), ("foods", 2), ("tags", 1), ("categories", 1), ("tools", 1), ("cookbooks", 1)] {
        assert_eq!(n(&db, &format!("SELECT count(*) FROM {table}")).await, Some(rows), "{table} lost rows");
        assert_eq!(n(&db, &format!("SELECT count(*) FROM {table} WHERE group_id = (SELECT id FROM groups)")).await, Some(rows), "{table} not backfilled");
    }
    for (table, rows) in [("batches", 2), ("meal_plan_entries", 1), ("shopping_lists", 1), ("users", 2)] {
        assert_eq!(n(&db, &format!("SELECT count(*) FROM {table}")).await, Some(rows), "{table} lost rows");
        assert_eq!(n(&db, &format!("SELECT count(*) FROM {table} WHERE household_id = (SELECT id FROM households)")).await, Some(rows), "{table} not backfilled");
    }
}

#[tokio::test]
async fn ids_and_links_survive_the_rebuilds() {
    let (db, _dir) = upgraded().await;
    assert_eq!(s(&db, "SELECT group_concat(id || name) FROM recipes ORDER BY id").await.as_deref(), Some("1Bread,2Soup"));
    assert_eq!(n(&db, "SELECT current_snapshot_id FROM recipes WHERE id = 2").await, Some(2));
    assert_eq!(n(&db, "SELECT count(*) FROM batch_lines WHERE batch_id = 1 AND unit_id = 1 AND food_id = 1").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM batches WHERE id = 2 AND snapshot_id IS NULL AND source = 'imported'").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM meal_plan_entries WHERE id = 1 AND recipe_id = 1 AND batch_id = 1").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM shopping_list_items WHERE list_id = 1 AND unit_id = 1 AND food_id = 1 AND recipe_id = 1").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM recipe_comments WHERE user_id = 1").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM batch_photos").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM recipe_images").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM webhooks").await, Some(1));
    let violations = sqlx::query("PRAGMA foreign_key_check").fetch_all(db.pool()).await.unwrap();
    assert_eq!(violations.len(), 0, "dangling references after the rebuild");
}

#[tokio::test]
async fn dropping_the_old_users_table_did_not_cascade_into_sessions_and_tokens() {
    let (db, _dir) = upgraded().await;
    assert_eq!(n(&db, "SELECT count(*) FROM sessions").await, Some(1));
    assert_eq!(n(&db, "SELECT count(*) FROM api_tokens").await, Some(1));
    let pragma: Option<i64> = n(&db, "PRAGMA foreign_keys").await;
    assert_eq!(pragma, Some(1), "foreign keys are back on once the migration is done");
}

#[tokio::test]
async fn snapshots_are_still_immutable() {
    let (db, _dir) = upgraded().await;
    assert!(fails(&db, "UPDATE recipe_snapshots SET content = '{\"x\":1}' WHERE id = 1").await);
    assert!(fails(&db, "DELETE FROM recipe_snapshots WHERE id = 1").await);
    assert_eq!(s(&db, "SELECT content FROM recipe_snapshots WHERE id = 1").await.as_deref(), Some("{}"));
}

#[tokio::test]
async fn the_local_admin_rules_still_hold_on_the_rebuilt_users_table() {
    let (db, _dir) = upgraded().await;
    let hh = "(SELECT id FROM households)";
    assert!(works(&db, &format!("INSERT INTO users (username, email, role, cook_theme, household_id) VALUES ('control', 'control@x', 'user', 'max-white', {hh})")).await, "the statements below differ from this one only in the rule they break");
    assert!(fails(&db, &format!("INSERT INTO users (username, email, role, local_admin, oidc_issuer, oidc_subject, household_id) VALUES ('x', 'x@x', 'admin', 1, 'i', 's', {hh})")).await, "insert trigger");
    assert!(fails(&db, "UPDATE users SET oidc_issuer = 'i', oidc_subject = 's' WHERE local_admin = 1").await, "update trigger");
    assert!(fails(&db, &format!("INSERT INTO users (username, email, role, local_admin, household_id) VALUES ('y', 'y@x', 'admin', 1, {hh})")).await, "one local admin only");
    assert!(fails(&db, &format!("INSERT INTO users (username, email, role, cook_theme, household_id) VALUES ('z', 'z@x', 'user', 'neon', {hh})")).await, "cook_theme CHECK");
    assert!(fails(&db, &format!("INSERT INTO users (username, email, role, household_id) VALUES ('ann', 'other@x', 'user', {hh})")).await, "username stays unique");
    assert!(fails(&db, "INSERT INTO users (username, email, role) VALUES ('nohome', 'nohome@x', 'user')").await, "household_id is required");
}

#[tokio::test]
async fn checks_survive_the_rebuilds() {
    let (db, _dir) = upgraded().await;
    let g = "(SELECT id FROM groups)";
    let h = "(SELECT id FROM households)";
    assert!(works(&db, &format!("INSERT INTO units (group_id, name, dimension, factor) VALUES ({g}, 'ok', 'volume', 2)")).await, "controls: each rule below is the only thing wrong with its statement");
    assert!(works(&db, &format!("INSERT INTO recipes (group_id, name) VALUES ({g}, 'ok')")).await);
    assert!(works(&db, &format!("INSERT INTO batches (household_id, status, recipe_id, snapshot_id, source, scale, rating) VALUES ({h}, 'planned', 1, 1, 'native', 1, 5)")).await);
    assert!(fails(&db, &format!("INSERT INTO units (group_id, name, dimension, factor) VALUES ({g}, 'bad', 'volume', NULL)")).await);
    assert!(fails(&db, &format!("INSERT INTO units (group_id, name, dimension, factor) VALUES ({g}, 'bad2', 'count', 1)")).await);
    assert!(fails(&db, &format!("INSERT INTO recipes (group_id, name) VALUES ({g}, '  ')")).await);
    assert!(fails(&db, &format!("INSERT INTO batches (household_id, status, recipe_id, source) VALUES ({h}, 'planned', 1, 'native')")).await, "native batch needs a snapshot");
    assert!(fails(&db, &format!("INSERT INTO batches (household_id, status, recipe_id, snapshot_id, source, scale) VALUES ({h}, 'planned', 1, 1, 'native', 0)")).await, "scale > 0");
    assert!(fails(&db, &format!("INSERT INTO batches (household_id, status, recipe_id, snapshot_id, source, rating) VALUES ({h}, 'done', 1, 1, 'native', 6)")).await, "rating 1-5");
    assert!(fails(&db, "INSERT INTO recipes (name) VALUES ('orphan')").await, "group_id is required");
    assert!(fails(&db, "INSERT INTO batches (status, recipe_id, snapshot_id, source) VALUES ('planned', 1, 1, 'native')").await, "household_id is required");
    assert!(fails(&db, "INSERT INTO meal_plan_entries (date, slot, title) VALUES ('2026-12-01', 'dinner', 'x')").await);
    assert!(fails(&db, "INSERT INTO shopping_lists (name) VALUES ('x')").await);
}

#[tokio::test]
async fn names_were_unique_across_the_instance_and_are_now_unique_per_group() {
    let (db, _dir) = upgraded().await;
    assert!(fails(&db, "INSERT INTO units (group_id, name, dimension) VALUES ((SELECT id FROM groups), 'g', 'other')").await, "same group, same name");
    assert!(fails(&db, "INSERT INTO foods (group_id, name) VALUES ((SELECT id FROM groups), 'flour')").await);
    for t in ["tags", "categories", "tools", "cookbooks"] {
        let name = s(&db, &format!("SELECT name FROM {t}")).await.unwrap();
        assert!(fails(&db, &format!("INSERT INTO {t} (group_id, name) VALUES ((SELECT id FROM groups), '{name}')")).await, "{t}");
    }
    sqlx::query("INSERT INTO groups (name, slug) VALUES ('Other', 'other')").execute(db.pool()).await.unwrap();
    for (t, extra, vals) in [("units", ", dimension", ", 'other'"), ("foods", "", ""), ("tags", "", ""), ("categories", "", ""), ("tools", "", ""), ("cookbooks", "", "")] {
        let name = s(&db, &format!("SELECT name FROM {t} LIMIT 1")).await.unwrap();
        let sql = format!("INSERT INTO {t} (group_id, name{extra}) VALUES ((SELECT id FROM groups WHERE slug = 'other'), '{name}'{vals})");
        assert!(!fails(&db, &sql).await, "{t}: the same name in another group is fine");
    }
    assert!(fails(&db, "INSERT INTO groups (name, slug) VALUES ('Again', 'other')").await, "group slugs are unique");
    sqlx::query("INSERT INTO households (group_id, name, slug) VALUES ((SELECT id FROM groups WHERE slug = 'other'), 'Home', 'home')").execute(db.pool()).await.unwrap();
    assert!(fails(&db, "INSERT INTO households (group_id, name, slug) VALUES ((SELECT id FROM groups WHERE slug = 'other'), 'Home 2', 'home')").await, "household slugs are unique within a group");
}

#[tokio::test]
async fn run_with_foreign_keys_on_the_migration_refuses_instead_of_cascading_away_sessions() {
    // Only accounts: nothing else would make a rebuild fail early, so without the guard the DROP of
    // `users` would silently delete the sessions and tokens that reference it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mealier.db");
    let mut seed = at_0008(&path).await;
    for sql in [
        "INSERT INTO users (id, username, email, role) VALUES (1, 'ann', 'ann@x', 'user')",
        "INSERT INTO sessions (id_hash, user_id, expires_at) VALUES ('s1', 1, '2999-01-01T00:00:00.000Z')",
        "INSERT INTO api_tokens (hash, user_id, name) VALUES ('t1', 1, 'cli')",
    ] {
        sqlx::query(sql).execute(&mut seed).await.unwrap();
    }
    seed.close().await.unwrap();

    let mut c = SqliteConnectOptions::new().filename(&path).foreign_keys(true).connect().await.unwrap();
    let outcome = sqlx::migrate!().run(&mut c).await;
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions").fetch_one(&mut c).await.unwrap();
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens").fetch_one(&mut c).await.unwrap();
    assert_eq!((sessions, tokens), (1, 1), "nothing was cascaded away");
    assert!(outcome.is_err(), "the guard at the top of 0009 must stop it");
    let groups: Option<i64> = sqlx::query_scalar("SELECT count(*) FROM groups").fetch_one(&mut c).await.ok();
    assert_eq!(groups, None, "and the half-applied migration was rolled back");
}
