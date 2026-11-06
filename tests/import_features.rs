//! The Mealie importer's recipe features (step 14, part A): ingredient section titles and reference ids,
//! step titles and ingredient links, substitutions, notes, assets, extras and settings. The fixture
//! has some of them; the tables it leaves empty get synthetic rows injected into a copy of the zip.
//!
//! Column names of the empty tables are not in the fixture. The ones used below come from Mealie's
//! OpenAPI schemas and its models, and are marked "unconfirmed" in the importer.
mod common;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use mealier::{Db, ImportReport, Recipe, Scope, import_mealie_backup};
use serde_json::{Value, json};

const A: &str = "9de853457e8748ff8f4712c3a19bc18c";
const A_DASHED: &str = "9de85345-7e87-48ff-8f47-12c3a19bc18c";
const FLOUR: &str = "b69a4f358c82447d8368c61957e91eb5";
const SALT: &str = "82382640c5454315928891ae59b64661";
const LIST: &str = "10ba6ecf46d74cc9ba5f3341fdbaef9f";
const ITEM: &str = "5f577e7febbf4ba2bbd6fc8e857712d2";
const INS: [&str; 3] = ["98c9dff6653b44ce91644f1247f583b6", "e6f6d56b4e1d405081e8109e44d05f92", "67d8a327af514c6caa7bd0b3a35ce078"];
const REF: [&str; 6] = ["bcdf720fed0246cc82c2195101a55777", "638f432eba5147df928dc8658d97f75a", "f2b51761438142ba910f6b18dd822201", "01fc6e2819034d4b892343550202df18", "41b400f02ee14a2e91c276889b0d82da", "33d74d204a0a4acf9d1e2caa63f17a39"];

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

async fn recipe(db: &Db, name: &str) -> Recipe {
    let sc = tenant(db).await;
    let id = db.list_recipes(&sc, true, None).await.unwrap().into_iter().find(|r| r.name == name).unwrap().id;
    db.get_recipe(&sc, id).await.unwrap().unwrap()
}

fn skipped(report: &ImportReport, needle: &str) -> usize {
    report.skipped.iter().filter(|s| s.contains(needle)).count()
}

// ---------- what the fixture itself has ----------

#[tokio::test]
async fn the_fixtures_reference_ids_become_line_ids_and_its_settings_are_kept() {
    let (db, report, _dir) = import(&fixture()).await;
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    assert_eq!(a.content.ingredients.iter().map(|i| i.uid.as_str()).collect::<Vec<_>>(), REF, "Mealie's reference id is the line's id");
    assert_eq!(a.content.step_details.len(), 3, "one entry per step");
    assert!(a.content.step_details.iter().all(|d| d.title.is_empty() && d.ingredient_uids.is_empty()));
    let s = &a.content.settings;
    assert_eq!((s.disable_amount, s.locked, s.hide_nutrition, s.public, s.disable_comments), (true, false, false, false, false), "show_nutrition is true in the fixture");
    let b = recipe(&db, "mealier-fixture-recipe-b").await;
    assert_eq!((b.content.settings.hide_nutrition, b.content.settings.public), (true, true), "show_nutrition false means hide");
    assert_eq!(skipped(&report, "section titles"), 0, "{:?}", report.skipped);
    assert!(a.content.notes.is_empty() && a.content.extras.is_empty() && a.content.assets.is_empty());
}

// ---------- injected rows ----------

fn features(v: &mut Value) {
    // section titles on two lines
    v["recipes_ingredients"][0]["title"] = json!("Dry");
    v["recipes_ingredients"][2]["title"] = json!("Wet");
    // step titles, a summary mealier has no place for, and links
    v["recipe_instructions"][0]["title"] = json!("Mix");
    v["recipe_instructions"][1]["summary"] = json!("a summary");
    push(v, "recipe_ingredient_ref_link", json!({"id": 1, "instruction_id": INS[0], "reference_id": REF[0]}));
    push(v, "recipe_ingredient_ref_link", json!({"id": 2, "instruction_id": INS[0], "reference_id": REF[1]}));
    push(v, "recipe_ingredient_ref_link", json!({"id": 3, "instruction_id": INS[2], "reference_id": REF[0]}));
    push(v, "recipe_ingredient_ref_link", json!({"id": 4, "instruction_id": INS[2], "reference_id": "ffffffffffffffffffffffffffffffff"}));
    // substitutions, by the ingredient's integer id and by its reference id
    push(v, "recipes_ingredients_substitutions", json!({"id": 1, "ingredient_id": 18, "substitute_food_id": SALT, "note": "if no flour"}));
    push(v, "recipes_ingredients_substitutions", json!({"id": 2, "reference_id": REF[2], "substitute_food_id": FLOUR, "note": null}));
    push(v, "notes", json!({"id": "n1", "recipe_id": A, "title": "Tip", "text": "Salt late"}));
    push(v, "notes", json!({"id": "n2", "recipe_id": A, "title": "", "text": "Second"}));
    push(v, "recipe_note_ref_link", json!({"id": 1, "instruction_id": INS[0], "reference_id": "n1"}));
    // recipe extras, and the settings Mealie enforces
    v["api_extras"] = json!([{"id": 1, "recipee_id": A, "key_name": "origin", "value": "gran"}]);
    for r in v["recipe_settings"].as_array_mut().unwrap() {
        if r["recipe_id"] == A {
            r["locked"] = json!(true);
            r["disable_comments"] = json!(true);
            r["landscape_view"] = json!(true);
            r["show_assets"] = json!(true);
        }
    }
    // assets: one with its file in the zip, one without
    push(v, "recipe_assets", json!({"id": "as1", "recipe_id": A, "name": "Menu", "icon": "mdi-file-pdf-box", "file_name": "menu.pdf"}));
    push(v, "recipe_assets", json!({"id": "as2", "recipe_id": A, "name": "Gone", "icon": "mdi-file", "file_name": "gone.txt"}));
    // foods
    push(v, "ingredient_food_extras", json!({"id": 1, "ingredient_food_id": FLOUR, "key_name": "aisle", "value": "3"}));
    push(v, "ingredient_foods_substitutions", json!({"id": 1, "food_id": FLOUR, "substitute_food_id": SALT, "note": "odd but ok"}));
    // shopping
    push(v, "shopping_list_extras", json!({"id": 1, "shopping_list_id": LIST, "key_name": "store", "value": "co-op"}));
    push(v, "shopping_list_item_extras", json!({"id": 1, "shopping_list_item_id": ITEM, "key_name": "brand", "value": "x"}));
}

const PDF: &[u8] = b"%PDF-1.4 a menu, not an image";

fn asset_entry() -> String {
    format!("data/recipes/{A_DASHED}/assets/menu.pdf")
}

#[tokio::test]
async fn titles_links_substitutions_notes_extras_assets_and_settings_come_across() {
    let entry = asset_entry();
    let (_z, path) = mutated(features, &[(entry.as_str(), PDF)]);
    let (db, report, dir) = import(&path).await;
    let sc = tenant(&db).await;
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let c = &a.content;

    assert_eq!(c.ingredients.iter().map(|i| i.section.as_str()).collect::<Vec<_>>(), ["Dry", "", "Wet", "", "", ""]);
    assert_eq!(c.step_details[0].title, "Mix");
    assert_eq!(c.step_details[0].ingredient_uids, [REF[0], REF[1]]);
    assert_eq!(c.step_details[2].ingredient_uids, [REF[0]]);
    assert_eq!(skipped(&report, "ffffffffffffffffffffffffffffffff"), 1, "a link to a line the recipe lacks is reported: {:?}", report.skipped);
    assert_eq!(skipped(&report, "summar"), 1, "{:?}", report.skipped);
    assert_eq!(skipped(&report, "section titles"), 0);

    let sub = |n: usize| c.ingredients[n].substitutions.iter().map(|s| (s.food.clone().unwrap(), s.note.clone())).collect::<Vec<_>>();
    assert_eq!(sub(0), [("mealier-fixture-salt".to_string(), "if no flour".to_string())]);
    assert_eq!(sub(2), [("mealier-fixture-flour".to_string(), String::new())]);
    assert!(c.ingredients[1].substitutions.is_empty());

    assert_eq!(c.notes.iter().map(|n| (n.title.as_str(), n.text.as_str())).collect::<Vec<_>>(), [("Tip", "Salt late"), ("", "Second")]);
    assert_eq!(skipped(&report, "note references"), 1, "{:?}", report.skipped);
    assert_eq!(c.extras.get("origin").map(String::as_str), Some("gran"));
    let s = &c.settings;
    assert_eq!((s.locked, s.disable_comments, s.landscape_view, s.show_assets, s.disable_amount, s.public), (true, true, true, true, true, false));

    // the asset: in the snapshot, in the table, on disk, byte for byte; the one without a file is reported
    assert_eq!(c.assets.len(), 1);
    let assets = db.recipe_assets(&sc, a.id).await.unwrap();
    assert_eq!((assets.len(), assets[0].name.as_str(), assets[0].icon.as_str(), assets[0].file_name.as_str()), (1, "Menu", "mdi-file-pdf-box", "menu.pdf"));
    assert_eq!(assets[0].hash, c.assets[0]);
    assert_eq!(std::fs::read(dir.path().join("assets").join(&assets[0].hash)).unwrap(), PDF);
    assert_eq!(skipped(&report, "gone.txt"), 1, "{:?}", report.skipped);
    assert_eq!(report.assets, 1);

    // only recipe a was touched
    let b = recipe(&db, "mealier-fixture-recipe-b").await;
    assert!(b.content.notes.is_empty() && b.content.extras.is_empty() && b.content.assets.is_empty());
    assert!(b.content.ingredients.iter().all(|i| i.section.is_empty() && i.substitutions.is_empty()));
    assert_eq!(db.snapshots(&sc, a.id).await.unwrap().len(), 1, "one snapshot, written with everything in it");
}

#[tokio::test]
async fn food_extras_and_substitutions_and_shopping_extras_come_across() {
    let (_z, path) = mutated(features, &[]);
    let (db, _report, _dir) = import(&path).await;
    let sc = tenant(&db).await;
    let foods = db.foods(&sc).await.unwrap();
    let id = |n: &str| foods.iter().find(|f| f.name == n).unwrap().id;
    let flour = db.food(&sc, id("mealier-fixture-flour")).await.unwrap();
    assert_eq!(flour.extras.get("aisle").map(String::as_str), Some("3"));
    assert_eq!(flour.substitutions.iter().map(|s| (s.name.as_str(), s.note.as_str())).collect::<Vec<_>>(), [("mealier-fixture-salt", "odd but ok")]);
    assert!(db.food(&sc, id("mealier-fixture-salt")).await.unwrap().substitutions.is_empty());

    let list = &db.shopping_lists(&sc).await.unwrap()[0];
    let list = db.shopping_list(&sc, list.id).await.unwrap();
    assert_eq!(list.extras.get("store").map(String::as_str), Some("co-op"));
    let item = list.items.iter().find(|i| i.note == "1 Cup Flour").unwrap();
    assert_eq!(item.extras.get("brand").map(String::as_str), Some("x"));
    assert!(list.items.iter().filter(|i| i.id != item.id).all(|i| i.extras.is_empty()));
}

#[tokio::test]
async fn an_asset_that_is_too_big_or_empty_is_reported_and_not_attached() {
    let big = vec![1u8; 20 * 1024 * 1024 + 1];
    let entry = asset_entry();
    let (_z, path) = mutated(
        |v| {
            push(v, "recipe_assets", json!({"id": "as1", "recipe_id": A, "name": "Big", "icon": "", "file_name": "menu.pdf"}));
            push(v, "recipe_assets", json!({"id": "as3", "recipe_id": A, "name": "Empty", "icon": "", "file_name": "empty.txt"}));
        },
        &[(entry.as_str(), big.as_slice()), (format!("data/recipes/{A_DASHED}/assets/empty.txt").as_str(), b"")],
    );
    let (db, report, _dir) = import(&path).await;
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    assert!(a.content.assets.is_empty());
    assert_eq!(skipped(&report, "20 MB"), 1, "{:?}", report.skipped);
    assert_eq!(skipped(&report, "empty.txt"), 1, "{:?}", report.skipped);
    assert_eq!(report.assets, 0);
}

#[tokio::test]
async fn an_imported_backup_exports_and_restores_with_its_assets() {
    let entry = asset_entry();
    let (_z, path) = mutated(features, &[(entry.as_str(), PDF)]);
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let db = Db::open(&data.join("mealier.db")).await.unwrap();
    import_mealie_backup(&db, &path, &data.join("images")).await.unwrap();
    let zip = root.path().join("out.zip");
    mealier::backup::export(db.pool(), &data, &zip).await.unwrap();
    let other = root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    mealier::backup::restore(&zip, &other).await.unwrap();
    let hash: String = sqlx::query_scalar("SELECT hash FROM recipe_assets").fetch_one(db.pool()).await.unwrap();
    assert_eq!(std::fs::read(other.join("images/assets").join(hash)).unwrap(), PDF);
}
