//! The Mealie importer's household and user features (step 14, part B): favourites and ratings, recipe
//! actions, foods on hand, tools owned and share links. The fixture has none of them (its copies of
//! these tables are empty, and it has no `recipe_share_tokens` at all), so synthetic rows are injected
//! into a copy of the zip.
//!
//! The column names come from Mealie v3.28.0's models, not from rows: `UserToRecipe` (user_id, recipe_id,
//! rating float, is_favorite), `GroupRecipeAction` (group_id, household_id, action_type, title, url),
//! the `households_to_ingredient_foods` (household_id, food_id) and `households_to_tools` (household_id,
//! tool_id) tables, and `RecipeShareTokenModel` (id, group_id, recipe_id, expires_at). A share token's
//! value is its `id`, shown dashed in the URL.
mod common;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use mealier::{Db, Error, ImportReport, Scope, import_mealie_backup};
use serde_json::{Value, json};

const A: &str = "9de853457e8748ff8f4712c3a19bc18c";
const FLOUR: &str = "b69a4f358c82447d8368c61957e91eb5";
const SALT: &str = "82382640c5454315928891ae59b64661";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mealie_v3.28.0.zip")
}

/// The fixture with database.json edited by `edit`, and `files` added to the zip.
fn mutated(edit: impl Fn(&mut Value), files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
    let mut src = zip::ZipArchive::new(std::fs::File::open(fixture()).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.zip");
    let mut out = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..src.len() {
        let mut f = src.by_index(i).unwrap();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).unwrap();
        let name = f.name().unwrap().to_string();
        if name == "database.json" {
            let mut v: Value = serde_json::from_slice(&bytes).unwrap();
            edit(&mut v);
            bytes = serde_json::to_vec(&v).unwrap();
        }
        out.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        out.write_all(&bytes).unwrap();
    }
    for (name, bytes) in files {
        out.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        out.write_all(bytes).unwrap();
    }
    out.finish().unwrap();
    (dir, path)
}

fn push(v: &mut Value, table: &str, row: Value) {
    v[table].as_array_mut().unwrap().push(row);
}

async fn import(path: &Path) -> (Db, ImportReport, tempfile::TempDir) {
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, path, dir.path()).await.unwrap();
    (db, report, dir)
}

async fn tenant(db: &Db) -> Scope {
    let u = db.users().await.unwrap().into_iter().find(|u| u.username == "fixture-admin").unwrap();
    Scope::from(&u)
}

fn skipped(report: &ImportReport, needle: &str) -> usize {
    report.skipped.iter().filter(|s| s.contains(needle)).count()
}

const B: &str = "3f920b15c2064902b28f95340c75d222";
const ADMIN: &str = "4dc54de44a2e486faf58e99f8249ab83";
const USER: &str = "30eb0800db984398b168a418da4f3f96";
const HOUSEHOLD: &str = "bdd39ab8b477457a89bcdf4d68e12d0d";
const GROUP: &str = "6a1a76f2eef844d7b079f8421d1d7b6d";
const TOOL: &str = "9a154a53ffd74a9b907c4199e14616c1";
const TOKEN: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
const TOKEN_URL: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
const OLD_TOKEN: &str = "aaaaaaaabbbbccccddddeeeeeeeeeeee";

async fn user_scope(db: &Db, name: &str) -> Scope {
    let u = db.users().await.unwrap().into_iter().find(|u| u.username == name).unwrap();
    Scope::from(&u)
}

async fn recipe_id(db: &Db, name: &str) -> i64 {
    db.list_recipes(&tenant(db).await, true, None).await.unwrap().into_iter().find(|r| r.name == name).unwrap().id
}

// ---------- the fixture has none ----------

#[tokio::test]
async fn the_fixture_alone_brings_none_of_them() {
    let (db, report, _dir) = import(&fixture()).await;
    assert_eq!((report.favourites, report.ratings, report.recipe_actions, report.foods_on_hand, report.tools_owned, report.share_links), (0, 0, 0, 0, 0, 0));
    assert!(db.actions(&tenant(&db).await).await.unwrap().is_empty());
    let n: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM user_recipes) + (SELECT count(*) FROM household_foods) + (SELECT count(*) FROM household_tools) + (SELECT count(*) FROM share_links)").fetch_one(db.pool()).await.unwrap();
    assert_eq!(n, 0);
}

// ---------- favourites and ratings ----------

#[tokio::test]
async fn favourites_and_ratings_become_each_users_own() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "users_to_recipes", json!({"id": "u1", "user_id": ADMIN, "recipe_id": A, "rating": 4.5, "is_favorite": true}));
            push(v, "users_to_recipes", json!({"id": "u2", "user_id": ADMIN, "recipe_id": B, "rating": 3.7, "is_favorite": false}));
            push(v, "users_to_recipes", json!({"id": "u3", "user_id": USER, "recipe_id": B, "rating": null, "is_favorite": true}));
            // nothing to keep: no rating (Mealie writes 0 for "none") and not a favourite
            push(v, "users_to_recipes", json!({"id": "u4", "user_id": USER, "recipe_id": A, "rating": 0.0, "is_favorite": false}));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    let (admin, user) = (user_scope(&db, "fixture-admin").await, user_scope(&db, "fixture-user").await);
    let (a, b) = (recipe_id(&db, "mealier-fixture-recipe-a").await, recipe_id(&db, "mealier-fixture-recipe-b").await);
    let o = db.opinion(&admin, a).await.unwrap();
    assert_eq!((o.favourite, o.rating), (true, Some(4.5)));
    let o = db.opinion(&admin, b).await.unwrap();
    assert_eq!((o.favourite, o.rating), (false, Some(3.5)), "3.7 is kept to the nearest half star");
    let o = db.opinion(&user, b).await.unwrap();
    assert_eq!((o.favourite, o.rating), (true, None));
    let o = db.opinion(&user, a).await.unwrap();
    assert_eq!((o.favourite, o.rating), (false, None));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM user_recipes").fetch_one(db.pool()).await.unwrap();
    assert_eq!(n, 3, "the empty row was not kept");
    assert_eq!((report.favourites, report.ratings), (2, 2));
    assert_eq!(skipped(&report, "3.7"), 1, "{:?}", report.skipped);
}

#[tokio::test]
async fn a_rating_out_of_range_is_clamped_and_reported_and_a_stranger_refuses_the_import() {
    let (_dir, zip) = mutated(|v| push(v, "users_to_recipes", json!({"id": "u1", "user_id": ADMIN, "recipe_id": A, "rating": 9.0, "is_favorite": false})), &[]);
    let (db, report, _d) = import(&zip).await;
    let a = recipe_id(&db, "mealier-fixture-recipe-a").await;
    assert_eq!(db.opinion(&user_scope(&db, "fixture-admin").await, a).await.unwrap().rating, Some(5.0));
    assert_eq!(skipped(&report, "rating 9 "), 1, "{:?}", report.skipped);

    let (_dir, zip) = mutated(|v| push(v, "users_to_recipes", json!({"id": "u1", "user_id": "ffffffffffffffffffffffffffffffff", "recipe_id": A, "rating": 3.0, "is_favorite": false})), &[]);
    let r = import_mealie_backup(&Db::open_memory().await.unwrap(), &zip, tempfile::tempdir().unwrap().path()).await;
    assert!(matches!(r, Err(Error::Validation(m)) if m.contains("user")), "an unknown user is refused whole");
}

// ---------- recipe actions ----------

#[tokio::test]
async fn recipe_actions_go_to_their_household_and_keep_their_kind_and_url() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "recipe_actions", json!({"id": "r1", "group_id": GROUP, "household_id": HOUSEHOLD, "action_type": "link", "title": "Search", "url": "https://www.google.com/search?q=${slug}"}));
            push(v, "recipe_actions", json!({"id": "r2", "group_id": GROUP, "household_id": HOUSEHOLD, "action_type": "post", "title": "Hook", "url": "http://hass.lan/api/webhook/x"}));
            // no household: the group's first
            push(v, "recipe_actions", json!({"id": "r3", "group_id": GROUP, "household_id": null, "action_type": "link", "title": "Bring", "url": "https://api.getbring.com/rest/bringrecipes/deeplink?url=${url}&source=web"}));
            push(v, "recipe_actions", json!({"id": "r4", "group_id": GROUP, "household_id": HOUSEHOLD, "action_type": "carrier-pigeon", "title": "Odd", "url": "https://x.test/"}));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    let got = db.actions(&tenant(&db).await).await.unwrap();
    assert_eq!(got.iter().map(|a| (a.title.as_str(), a.kind.as_str(), a.url.as_str())).collect::<Vec<_>>(), [
        ("Bring", "link", "https://api.getbring.com/rest/bringrecipes/deeplink?url=${url}&source=web"),
        ("Hook", "post", "http://hass.lan/api/webhook/x"),
        ("Search", "link", "https://www.google.com/search?q=${slug}"),
    ]);
    assert_eq!(report.recipe_actions, 3);
    assert_eq!(skipped(&report, "carrier-pigeon"), 1, "{:?}", report.skipped);
}

// ---------- foods on hand and tools owned ----------

#[tokio::test]
async fn foods_on_hand_and_tools_owned_go_to_the_household() {
    let (_dir, zip) = mutated(
        |v| {
            push(v, "households_to_ingredient_foods", json!({"household_id": HOUSEHOLD, "food_id": FLOUR}));
            push(v, "households_to_ingredient_foods", json!({"household_id": HOUSEHOLD, "food_id": SALT}));
            push(v, "households_to_tools", json!({"household_id": HOUSEHOLD, "tool_id": TOOL}));
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    let p = db.pantry(&tenant(&db).await).await.unwrap();
    let have = |items: &[mealier::PantryItem]| items.iter().filter(|i| i.have).map(|i| i.name.clone()).collect::<Vec<_>>();
    assert_eq!(have(&p.foods), ["mealier-fixture-flour", "mealier-fixture-salt"]);
    assert_eq!(have(&p.tools), ["mealier-fixture-tool"]);
    assert_eq!((report.foods_on_hand, report.tools_owned), (2, 1));

    let (_dir, zip) = mutated(|v| push(v, "households_to_tools", json!({"household_id": "ffffffffffffffffffffffffffffffff", "tool_id": TOOL})), &[]);
    let r = import_mealie_backup(&Db::open_memory().await.unwrap(), &zip, tempfile::tempdir().unwrap().path()).await;
    assert!(matches!(r, Err(Error::Validation(m)) if m.contains("household")), "an unknown household is refused whole");
}

// ---------- share links ----------

#[tokio::test]
async fn mealies_share_tokens_keep_working_and_only_their_hash_is_stored() {
    let (_dir, zip) = mutated(
        |v| {
            v["recipe_share_tokens"] = json!([
                {"id": TOKEN, "group_id": GROUP, "recipe_id": A, "expires_at": "2099-01-01T00:00:00", "created_at": "2026-10-01T12:00:00.123456", "update_at": "2026-10-01T12:00:00.123456"},
                {"id": OLD_TOKEN, "group_id": GROUP, "recipe_id": A, "expires_at": "2020-01-01 00:00:00", "created_at": "2019-12-01 12:00:00", "update_at": "2019-12-01 12:00:00"},
                {"id": "11111111111111111111111111111111", "group_id": GROUP, "recipe_id": B, "expires_at": "soon", "created_at": "2026-10-01T12:00:00"},
            ]);
        },
        &[],
    );
    let (db, report, _d) = import(&zip).await;
    let recipe = db.shared_recipe(TOKEN_URL).await.unwrap();
    assert_eq!(recipe.content.name, "mealier-fixture-recipe-a");
    assert!(matches!(db.shared_recipe(TOKEN).await, Err(Error::NotFound(_))), "the URL form is the token: no dashes, no match");
    assert!(matches!(db.shared_recipe("0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f1").await, Err(Error::NotFound(_))));
    assert!(matches!(db.shared_recipe("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").await, Err(Error::NotFound(_))), "expired in Mealie, expired here");
    let links = db.share_links(&tenant(&db).await, recipe.id).await.unwrap();
    assert_eq!(links.len(), 2);
    assert!(links.iter().any(|l| l.expires_at == "2099-01-01T00:00:00Z"), "{links:?}");
    let all: String = sqlx::query_scalar("SELECT json_group_array(json_array(token_hash, created_at, expires_at)) FROM share_links").fetch_one(db.pool()).await.unwrap();
    assert!(!all.contains(TOKEN) && !all.contains(TOKEN_URL), "only the hash is stored");
    assert_eq!(report.share_links, 2);
    assert_eq!(skipped(&report, "soon"), 1, "a token with an expiry mealier cannot read is reported: {:?}", report.skipped);
}
