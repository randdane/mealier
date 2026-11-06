//! Cookbooks (step 13): migration 0011, the store, scoping, and filter-backed membership.
mod common;
use std::borrow::Cow;
use std::path::Path;

use mealier::{CookbookInput, Db, Error, RecipeInput, Scope};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Connection};

fn input(name: &str, filter: &str) -> CookbookInput {
    CookbookInput { name: name.into(), query_filter: filter.into(), ..Default::default() }
}

async fn recipe(db: &Db, sc: &Scope, name: &str, tags: &[&str]) -> i64 {
    db.create_recipe(sc, RecipeInput { name: name.into(), tags: tags.iter().map(|t| t.to_string()).collect(), ..Default::default() }).await.unwrap().id
}

fn names(v: Vec<mealier::RecipeSummary>) -> Vec<String> {
    v.into_iter().map(|r| r.name).collect()
}

#[tokio::test]
async fn migration_0011_upgrades_existing_cookbooks_and_adds_the_rules_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    let mut m = sqlx::migrate::Migrator::new(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))).await.unwrap();
    m.migrations = Cow::Owned(m.migrations.iter().filter(|x| x.version <= 10).cloned().collect());
    let mut c = SqliteConnectOptions::new().filename(&path).create_if_missing(true).journal_mode(SqliteJournalMode::Wal).foreign_keys(false).connect().await.unwrap();
    m.run(&mut c).await.unwrap();
    for sql in [
        "INSERT INTO groups (id, name, slug) VALUES (1, 'Home', 'home')",
        "INSERT INTO cookbooks (id, group_id, name) VALUES (1, 1, 'Weeknight'), (2, 1, 'Weekend')",
    ] {
        sqlx::query(sql).execute(&mut c).await.unwrap();
    }
    c.close().await.unwrap();

    let db = Db::open(&path).await.unwrap();
    type Row = (String, String, String, i64, bool, String, Option<String>);
    let rows: Vec<Row> =
        sqlx::query_as("SELECT name, description, slug, position, public, query_filter, filter_json FROM cookbooks ORDER BY id").fetch_all(db.pool()).await.unwrap();
    assert_eq!(rows[0], ("Weeknight".into(), "".into(), "cookbook-1".into(), 1, false, "".into(), None));
    assert_eq!(rows[1].2, "cookbook-2", "old rows get distinct slugs");
    let twin: Result<_, _> = sqlx::query("INSERT INTO cookbooks (group_id, name, slug) VALUES (1, 'Other', 'cookbook-1')").execute(db.pool()).await;
    assert!(twin.is_err(), "slugs are unique per group");

    for (table, cols) in [
        ("meal_plan_rules", "id, group_id, day, slot, query_filter, filter_json"),
        ("meal_plan_rule_households", "rule_id, household_id"),
    ] {
        let q = format!("SELECT {cols} FROM {table}");
        sqlx::query(&q).fetch_all(db.pool()).await.unwrap_or_else(|e| panic!("{table}: {e}"));
    }
    let bad_day = sqlx::query("INSERT INTO meal_plan_rules (group_id, day) VALUES (1, 'someday')").execute(db.pool()).await;
    assert!(bad_day.is_err(), "day is checked");
    let bad_slot = sqlx::query("INSERT INTO meal_plan_rules (group_id, slot) VALUES (1, 'brunch')").execute(db.pool()).await;
    assert!(bad_slot.is_err(), "slot is checked");
    sqlx::query("INSERT INTO meal_plan_rules (id, group_id) VALUES (1, 1)").execute(db.pool()).await.unwrap();
    let (day, slot, q): (String, String, String) = sqlx::query_as("SELECT day, slot, query_filter FROM meal_plan_rules WHERE id = 1").fetch_one(db.pool()).await.unwrap();
    assert_eq!((day.as_str(), slot.as_str(), q.as_str()), ("unset", "unset", ""));
}

#[tokio::test]
async fn a_cookbook_round_trips_with_a_slug_and_position() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let a = db.add_cookbook(&sc, CookbookInput { description: "quick things".into(), public: true, ..input("Weeknight Dinners", r#"tags.name IN ["quick"]"#) }).await.unwrap();
    assert_eq!((a.name.as_str(), a.slug.as_str(), a.position, a.public, a.filter_supported), ("Weeknight Dinners", "weeknight-dinners", 1, true, true));
    assert_eq!((a.description.as_str(), a.query_filter.as_str()), ("quick things", r#"tags.name IN ["quick"]"#));
    let b = db.add_cookbook(&sc, input("Weeknight-Dinners", "")).await.unwrap();
    assert_eq!((b.slug.as_str(), b.position), ("weeknight-dinners-2", 2));
    assert_eq!(db.cookbook(&sc, a.id).await.unwrap(), a);
    assert_eq!(db.cookbooks(&sc).await.unwrap(), vec![a.clone(), b.clone()]);

    let a2 = db.update_cookbook(&sc, a.id, CookbookInput { position: Some(3), ..input("Renamed", "") }).await.unwrap();
    assert_eq!((a2.name.as_str(), a2.slug.as_str(), a2.position, a2.public, a2.query_filter.as_str(), a2.description.as_str()), ("Renamed", "renamed", 3, false, "", ""));
    let b2 = db.update_cookbook(&sc, b.id, input("Weeknight-Dinners", "")).await.unwrap();
    assert_eq!((b2.slug.as_str(), b2.position), ("weeknight-dinners-2", 2), "a save without a position keeps it, and without a rename keeps the slug");
    assert_eq!(names_of(&db.cookbooks(&sc).await.unwrap()), ["Weeknight-Dinners", "Renamed"], "by position");
    db.delete_cookbook(&sc, a.id).await.unwrap();
    assert!(matches!(db.cookbook(&sc, a.id).await, Err(Error::NotFound(_))));
    assert!(matches!(db.delete_cookbook(&sc, a.id).await, Err(Error::NotFound(_))));
}

fn names_of(v: &[mealier::Cookbook]) -> Vec<String> {
    v.iter().map(|c| c.name.clone()).collect()
}

#[tokio::test]
async fn bad_cookbooks_are_refused() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    db.add_cookbook(&sc, input("Taken", "")).await.unwrap();
    for (name, filter, want) in [
        ("  ", "", "name"),
        ("Taken", "", "Taken"),
        ("Bad", r#"tags.name IN ["a"] AND"#, "filter not supported"),
        ("Bad", r#"household_id = "x""#, "household_id"),
        ("Bad", r#"tags.id IN ["da133078-247a-42db-a599-57fe46062075"]"#, "not a mealier id"),
    ] {
        match db.add_cookbook(&sc, input(name, filter)).await {
            Err(Error::Validation(m)) => assert!(m.contains(want), "{name} {filter}: {m}"),
            other => panic!("{name} {filter}: {other:?}"),
        }
    }
    assert_eq!(db.cookbooks(&sc).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_cookbook_lists_the_recipes_its_filter_selects() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    recipe(&db, &sc, "Pancakes", &["quick", "breakfast"]).await;
    recipe(&db, &sc, "Stew", &["slow"]).await;
    recipe(&db, &sc, "Toast", &["quick"]).await;
    let quick = db.add_cookbook(&sc, input("Quick", r#"tags.name IN ["quick"]"#)).await.unwrap();
    let all = db.add_cookbook(&sc, input("Everything", "")).await.unwrap();
    let neither = db.add_cookbook(&sc, input("Nothing", r#"tags.name IN ["quick"] AND tags.name IN ["slow"]"#)).await.unwrap();
    assert_eq!(names(db.cookbook_recipes(&sc, quick.id).await.unwrap()), ["Pancakes", "Toast"]);
    assert_eq!(names(db.cookbook_recipes(&sc, all.id).await.unwrap()), ["Pancakes", "Stew", "Toast"]);
    assert_eq!(names(db.cookbook_recipes(&sc, neither.id).await.unwrap()), Vec::<String>::new());
    // by id, the way the builder writes it
    let tag: i64 = sqlx::query_scalar("SELECT id FROM tags WHERE name = 'slow'").fetch_one(db.pool()).await.unwrap();
    let by_id = db.add_cookbook(&sc, input("Slow", &format!(r#"tags.id IN ["{tag}"]"#))).await.unwrap();
    assert_eq!(names(db.cookbook_recipes(&sc, by_id.id).await.unwrap()), ["Stew"]);
    // a recipe edited out of the filter leaves the cookbook at once: membership is computed
    let toast = db.list_recipes(&sc, false, Some("Toast")).await.unwrap()[0].id;
    db.update_recipe(&sc, toast, RecipeInput { name: "Toast".into(), tags: vec!["plain".into()], ..Default::default() }).await.unwrap();
    assert_eq!(names(db.cookbook_recipes(&sc, quick.id).await.unwrap()), ["Pancakes"]);
}

#[tokio::test]
async fn an_unreadable_stored_filter_shows_as_unsupported_and_lists_nothing() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    recipe(&db, &sc, "Pancakes", &["quick"]).await;
    sqlx::query("INSERT INTO cookbooks (id, group_id, name, slug, query_filter, filter_json) VALUES (50, ?, 'Imported', 'imported', 'household_id = \"x\"', NULL)").bind(sc.group_id).execute(db.pool()).await.unwrap();
    let c = db.cookbook(&sc, 50).await.unwrap();
    assert_eq!((c.query_filter.as_str(), c.filter_supported), ("household_id = \"x\"", false));
    assert!(db.cookbook_recipes(&sc, 50).await.unwrap().is_empty());
    // saving it again with a readable filter fixes it
    let fixed = db.update_cookbook(&sc, 50, input("Imported", r#"tags.name IN ["quick"]"#)).await.unwrap();
    assert!(fixed.filter_supported);
    assert_eq!(names(db.cookbook_recipes(&sc, 50).await.unwrap()), ["Pancakes"]);
}

#[tokio::test]
async fn cookbooks_belong_to_the_group_and_every_household_in_it_shares_them() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    recipe(&db, &t.a, "Pancakes", &["quick"]).await;
    recipe(&db, &t.c, "Theirs", &["quick"]).await;
    let book = db.add_cookbook(&t.a, input("Quick", r#"tags.name IN ["quick"]"#)).await.unwrap();
    assert_eq!(db.cookbook(&t.b, book.id).await.unwrap(), book, "another household of the group sees it");
    assert_eq!(names(db.cookbook_recipes(&t.b, book.id).await.unwrap()), ["Pancakes"]);
    db.update_cookbook(&t.b, book.id, input("Quicker", "")).await.unwrap();
    // another group sees nothing, can change nothing, and can reuse the name
    assert!(db.cookbooks(&t.c).await.unwrap().is_empty());
    assert!(matches!(db.cookbook(&t.c, book.id).await, Err(Error::NotFound(_))));
    assert!(matches!(db.cookbook_recipes(&t.c, book.id).await, Err(Error::NotFound(_))));
    assert!(matches!(db.update_cookbook(&t.c, book.id, input("Mine", "")).await, Err(Error::NotFound(_))));
    assert!(matches!(db.delete_cookbook(&t.c, book.id).await, Err(Error::NotFound(_))));
    db.add_cookbook(&t.c, input("Quicker", "")).await.unwrap();
    assert_eq!(db.cookbook(&t.a, book.id).await.unwrap().name, "Quicker");
}

#[tokio::test]
async fn recipes_that_name_a_cookbook_still_add_it_to_the_vocabulary_with_a_slug() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    for (n, c) in [("A", "Weeknight"), ("B", "Weekend"), ("C", "Weeknight")] {
        db.create_recipe(&sc, RecipeInput { name: n.into(), cookbooks: vec![c.into()], ..Default::default() }).await.unwrap();
    }
    let books = db.cookbooks(&sc).await.unwrap();
    assert_eq!(books.iter().map(|b| (b.name.as_str(), b.slug.as_str())).collect::<Vec<_>>(), [("Weeknight", "weeknight"), ("Weekend", "weekend")]);
    assert!(books.iter().all(|b| b.filter_supported && b.query_filter.is_empty()));
}

// ---------- the builder ----------

use mealier::filter::SetField;
use mealier::{Pick, builder_string, picks_of};

fn pick(field: SetField, all: bool, ids: &[i64]) -> Pick {
    Pick { field, all, ids: ids.into() }
}

#[test]
fn the_builder_writes_a_mealie_filter_string_that_reads_back() {
    let picks = vec![pick(SetField::Tags, false, &[1, 2]), pick(SetField::Categories, true, &[3]), pick(SetField::Tools, false, &[])];
    let s = builder_string(&picks);
    assert_eq!(s, r#"tags.id IN ["1", "2"] AND recipe_category.id CONTAINS ALL ["3"]"#);
    assert!(mealier::filter::parse(&s).unwrap().is_some(), "it is a filter Mealie's grammar and ours both read");
    assert_eq!(picks_of(&s), vec![pick(SetField::Tags, false, &[1, 2]), pick(SetField::Categories, true, &[3])]);
    assert_eq!(builder_string(&[]), "");
    assert_eq!(builder_string(&[pick(SetField::Tools, true, &[])]), "");
    assert_eq!(picks_of(""), vec![]);
}

#[test]
fn picks_come_back_only_from_what_the_builder_could_have_written() {
    assert_eq!(picks_of(r#"tools.id CONTAINS ALL ["7"]"#), vec![pick(SetField::Tools, true, &[7])]);
    for other in [
        r#"tags.name IN ["quick"]"#,
        r#"tags.id IN ["1"] OR tags.id IN ["2"]"#,
        r#"tags.id NOT IN ["1"]"#,
        r#"tags.id IN ["1"] AND name LIKE "x""#,
        r#"tags.id IN ["da133078-247a-42db-a599-57fe46062075"]"#,
        "name LIKE",
    ] {
        assert_eq!(picks_of(other), vec![], "{other}");
    }
}

#[tokio::test]
async fn the_vocabulary_lists_the_groups_tags_categories_and_tools() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    db.create_recipe(&t.a, RecipeInput { name: "A".into(), tags: vec!["b-tag".into(), "a-tag".into()], categories: vec!["Mains".into()], tools: vec!["pot".into()], ..Default::default() }).await.unwrap();
    db.create_recipe(&t.c, RecipeInput { name: "C".into(), tags: vec!["c-tag".into()], ..Default::default() }).await.unwrap();
    let n = |v: Vec<mealier::Named>| v.into_iter().map(|x| x.name).collect::<Vec<_>>();
    assert_eq!(n(db.vocabulary(&t.a, SetField::Tags).await.unwrap()), ["a-tag", "b-tag"]);
    assert_eq!(n(db.vocabulary(&t.b, SetField::Categories).await.unwrap()), ["Mains"]);
    assert_eq!(n(db.vocabulary(&t.a, SetField::Tools).await.unwrap()), ["pot"]);
    assert_eq!(n(db.vocabulary(&t.c, SetField::Tags).await.unwrap()), ["c-tag"]);
    assert!(db.vocabulary(&t.c, SetField::Tools).await.unwrap().is_empty());
}
