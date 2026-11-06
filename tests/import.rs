mod common;
use std::path::{Path, PathBuf};

use mealier::{Db, Dimension, ImportReport, import_mealie_backup};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mealie_v3.28.0.zip")
}

/// The tenant of the fixture's admin: where the backup put everything.
async fn tenant(db: &Db) -> mealier::Scope {
    mealier::Scope::from(&user(db, "fixture-admin").await)
}

async fn user(db: &Db, name: &str) -> mealier::User {
    db.users().await.unwrap().into_iter().find(|u| u.username == name).unwrap()
}

async fn imported() -> (Db, ImportReport, tempfile::TempDir) {
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, &fixture(), dir.path()).await.unwrap();
    (db, report, dir)
}

#[tokio::test]
async fn units_and_foods_are_imported_with_dimensions_and_aliases() {
    let (db, report, _dir) = imported().await;
    assert_eq!(report.units, 26);
    assert_eq!(report.foods, 2689);

    let units = db.units(&tenant(&db).await).await.unwrap();
    let unit = |name: &str| units.iter().find(|u| u.name == name).unwrap();
    assert_eq!((unit("gram").dimension, unit("gram").factor), (Dimension::Mass, Some(1.0)));
    assert_eq!((unit("pound").dimension, unit("pound").factor), (Dimension::Mass, Some(453.59237)));
    assert_eq!((unit("teaspoon").dimension, unit("teaspoon").factor), (Dimension::Volume, Some(4.92892159375)));
    assert_eq!((unit("cup").dimension, unit("cup").factor), (Dimension::Volume, Some(236.5882365)));
    assert_eq!((unit("pinch").dimension, unit("pinch").factor), (Dimension::Other, None));
    assert_eq!(unit("mealier-fixture-cup").aliases, vec!["mealier-fixture-cups"]);
}

#[tokio::test]
async fn food_aliases_are_kept() {
    let (db, _report, _dir) = imported().await;
    let foods = db.foods(&tenant(&db).await).await.unwrap();
    let flour = foods.iter().find(|f| f.name == "mealier-fixture-flour").unwrap();
    assert_eq!(flour.aliases, vec!["mealier-fixture-flour-alias"]);
    assert_eq!(foods.iter().find(|f| f.name == "mealier-fixture-salt").unwrap().aliases, Vec::<String>::new());
}

async fn recipe(db: &Db, name: &str) -> mealier::Recipe {
    let id = db.list_recipes(&tenant(db).await, true, None).await.unwrap().into_iter().find(|r| r.name == name).unwrap().id;
    db.get_recipe(&tenant(db).await, id).await.unwrap().unwrap()
}

#[tokio::test]
async fn recipe_a_is_imported_with_one_snapshot_and_its_fields() {
    let (db, report, _dir) = imported().await;
    assert_eq!(
        (report.recipes, report.tags, report.categories, report.cookbooks, report.tools),
        (2, 2, 2, 1, 1)
    );
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    assert_eq!(db.snapshots(&tenant(&db).await, a.id).await.unwrap().len(), 1);
    let c = &a.content;
    assert_eq!(c.description, "mealier importer fixture");
    assert_eq!(c.source_url.as_deref(), Some("https://example.invalid/fixture"));
    assert_eq!(c.tags, vec!["mealier-fixture-tag-1", "mealier-fixture-tag-2"]);
    assert_eq!(c.categories, vec!["mealier-fixture-cat-1", "mealier-fixture-cat-2"]);
    assert_eq!(c.tools, vec!["mealier-fixture-tool"]);
    assert_eq!(c.steps, vec!["Mix the dry ingredients.", "Add water and knead.", "Bake until golden."]);
    let n = c.nutrition.as_ref().unwrap();
    assert_eq!((n.calories, n.protein_g, n.fat_g, n.carbs_g), (Some(250.0), Some(5.0), Some(10.0), Some(30.0)));
}

#[tokio::test]
async fn recipe_a_ingredient_lines() {
    let (db, _report, _dir) = imported().await;
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let lines: Vec<_> = a
        .content
        .ingredients
        .iter()
        .map(|i| (i.qty_min, i.qty_max, i.unit_name.as_deref(), i.food_name.as_deref(), i.note.as_str(), i.original_text.as_str()))
        .collect();
    let cup = Some("mealier-fixture-cup");
    let flour = Some("mealier-fixture-flour");
    let salt = Some("mealier-fixture-salt");
    assert_eq!(
        lines,
        vec![
            (Some(2.0), Some(2.0), cup, flour, "", "2 cups flour"),
            (Some(0.5), Some(0.5), cup, flour, "", "1/2 cup flour"),
            (None, None, None, salt, "to taste", "salt, to taste"),
            (None, None, None, None, "a handful of fresh herbs, roughly torn", "a handful of fresh herbs, roughly torn"),
            (Some(1.0), Some(1.0), Some("mealier-fixture-pinch"), salt, "finely ground", "1 pinch salt, finely ground"),
            (Some(1.5), Some(1.5), cup, flour, "range 1-2 cups in source; schema has no range", "1-2 cups flour"),
        ]
    );
}

#[tokio::test]
async fn users_are_imported_without_passwords_or_oidc_links() {
    let (db, report, _dir) = imported().await;
    assert_eq!(report.users, 2);
    let users: Vec<_> = db
        .users()
        .await
        .unwrap()
        .into_iter()
        .map(|u| (u.username, u.email, u.full_name, u.role, u.oidc_issuer, u.oidc_subject, u.disabled))
        .collect();
    assert_eq!(
        users,
        vec![
            ("fixture-admin".into(), "fixture-admin@example.invalid".into(), "Fixture Admin".into(), "admin".into(), None, None, false),
            ("fixture-user".into(), "fixture-user@example.invalid".into(), "Fixture User".into(), "user".into(), None, None, false),
        ]
    );
    let secret_columns: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_table_info('users') WHERE name LIKE '%pass%' OR name LIKE '%token%'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(secret_columns, 0);
}

#[tokio::test]
async fn a_made_it_event_becomes_an_imported_done_batch_without_snapshot() {
    let (db, report, _dir) = imported().await;
    assert_eq!(report.batches, 1);
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let b = recipe(&db, "mealier-fixture-recipe-b").await;

    let batches = db.list_batches(&tenant(&db).await, a.id).await.unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!((batch.status.as_str(), batch.source.as_str(), batch.snapshot_id), ("done", "imported", None));
    assert_eq!((batch.made_on.as_deref(), batch.scale, batch.rating), (Some("2026-11-06"), 1.0, None));
    assert_eq!(batch.notes, "made it, tasted good");
    assert!(db.list_batches(&tenant(&db).await, b.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn other_timeline_events_are_reported_and_the_comment_is_kept() {
    let (db, report, _dir) = imported().await;
    assert!(report.skipped.iter().any(|s| s.contains("mealier-fixture info event")), "{:?}", report.skipped);
    assert!(report.skipped.iter().any(|s| s.contains("2 system timeline events")), "{:?}", report.skipped);
    assert!(!report.skipped.iter().any(|s| s.contains("last_made")), "a's last_made has its made event");

    assert_eq!(report.comments, 1);
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let comments = db.comments(&tenant(&db).await, a.id).await.unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].text, "mealier-fixture comment");
    let admin = db.users().await.unwrap().into_iter().find(|u| u.username == "fixture-admin").unwrap();
    assert_eq!(comments[0].user_id, Some(admin.id));
}

#[tokio::test]
async fn meal_plan_entries_include_the_note_only_one() {
    let (db, report, _dir) = imported().await;
    assert_eq!(report.meal_plan_entries, 2);
    let a = recipe(&db, "mealier-fixture-recipe-a").await;

    let plan = db.meal_plan(&tenant(&db).await, "2026-12-01", "2026-12-31").await.unwrap();
    let got: Vec<_> = plan.iter().map(|e| (e.date.as_str(), e.slot.as_str(), e.recipe_id, e.title.as_str(), e.text.as_str(), e.batch_id)).collect();
    assert_eq!(
        got,
        vec![
            ("2026-12-01", "dinner", Some(a.id), "mealier-fixture planned recipe", "", None),
            ("2026-12-02", "lunch", None, "mealier-fixture note entry", "eat out, no recipe", None),
        ]
    );
    assert_eq!(db.meal_plan(&tenant(&db).await, "2026-12-02", "2026-12-02").await.unwrap().len(), 1, "range is inclusive");
    assert!(db.meal_plan(&tenant(&db).await, "2026-12-03", "2026-12-31").await.unwrap().is_empty());
}

#[tokio::test]
async fn shopping_list_items_keep_checked_state_amounts_and_recipe() {
    let (db, report, _dir) = imported().await;
    assert_eq!((report.shopping_lists, report.shopping_list_items), (1, 4));
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let units = db.units(&tenant(&db).await).await.unwrap();
    let cup = units.iter().find(|u| u.name == "mealier-fixture-cup").unwrap().id;
    let flour = db.foods(&tenant(&db).await).await.unwrap().into_iter().find(|f| f.name == "mealier-fixture-flour").unwrap().id;

    let lists = db.shopping_lists(&tenant(&db).await).await.unwrap();
    assert_eq!(lists.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["mealier-fixture-list"]);
    let list = db.shopping_list(&tenant(&db).await, lists[0].id).await.unwrap();
    let got: Vec<_> = list.items.iter().map(|i| (i.qty, i.unit_id, i.food_id, i.note.as_str(), i.checked, i.recipe_id)).collect();
    assert_eq!(
        got,
        vec![
            (None, None, None, "1 Cup Flour", false, Some(a.id)),
            (Some(3.0), Some(cup), Some(flour), "hand added", false, None),
            (Some(1.0), None, None, "mealier-fixture checked item", true, None),
            (None, None, None, "mealier-fixture note-only item", false, None),
        ]
    );
}

#[tokio::test]
async fn recipe_images_are_stored_under_their_sha256() {
    use sha2::{Digest, Sha256};
    let (db, report, dir) = imported().await;
    assert_eq!(report.images, 1);
    let hash = "c1372d09b33530862330ee4c1426ba3ec3f7646de551d0e074c0ac5067020fb5";

    let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(files, vec![format!("{hash}.webp")]);
    let bytes = std::fs::read(dir.path().join(format!("{hash}.webp"))).unwrap();
    let digest: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!((bytes.len(), digest.as_str()), (66, hash));

    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let b = recipe(&db, "mealier-fixture-recipe-b").await;
    let imgs = db.recipe_images(&tenant(&db).await, a.id).await.unwrap();
    assert_eq!(imgs.iter().map(|i| (i.hash.as_str(), i.mime.as_str())).collect::<Vec<_>>(), vec![(hash, "image/webp")]);
    assert!(db.recipe_images(&tenant(&db).await, b.id).await.unwrap().is_empty());
}

/// The fixture with database.json edited by `edit`, as a new zip.
fn mutated(edit: impl Fn(&mut serde_json::Value)) -> (tempfile::TempDir, PathBuf) {
    use std::io::{Read, Write};
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
            let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            edit(&mut v);
            bytes = serde_json::to_vec(&v).unwrap();
        }
        out.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        out.write_all(&bytes).unwrap();
    }
    out.finish().unwrap();
    (dir, path)
}

async fn assert_empty(db: &Db, images: &Path) {
    // Raw counts: a tenant-scoped read of an empty DB is empty whatever leaked.
    for t in ["units", "foods", "recipes", "users", "shopping_lists", "shopping_list_items", "meal_plan_entries", "batches", "tags", "categories", "tools", "cookbooks", "groups", "households", "import_ids"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {t}")).fetch_one(db.pool()).await.unwrap();
        assert_eq!(n, 0, "{t} must be empty");
    }
    assert!(!images.exists() || std::fs::read_dir(images).unwrap().next().is_none());
}

#[tokio::test]
async fn a_non_empty_database_is_refused_and_left_as_it_was() {
    let (db, _report, dir) = imported().await;
    let err = import_mealie_backup(&db, &fixture(), dir.path()).await.unwrap_err();
    assert!(matches!(&err, mealier::Error::Validation(m) if m.contains("not empty")), "{err}");
    assert_eq!(db.units(&tenant(&db).await).await.unwrap().len(), 26);
    assert_eq!(db.list_recipes(&tenant(&db).await, true, None).await.unwrap().len(), 2);

    let db = Db::open_memory().await.unwrap();
    db.add_food(&common::scope(&db).await, "salt").await.unwrap();
    assert!(import_mealie_backup(&db, &fixture(), dir.path()).await.is_err(), "any existing data refuses");
}

#[tokio::test]
async fn another_mealie_schema_version_is_refused() {
    let (_keep, zip) = mutated(|v| v["alembic_version"][0]["version_num"] = "deadbeef0000".into());
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = import_mealie_backup(&db, &zip, dir.path()).await.unwrap_err();
    assert!(matches!(&err, mealier::Error::Validation(m) if m.contains("3527efeeec34") && m.contains("deadbeef0000")), "{err}");
    assert_empty(&db, dir.path()).await;
}

#[tokio::test]
async fn a_truncated_zip_leaves_the_database_empty() {
    let bytes = std::fs::read(fixture()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cut = dir.path().join("cut.zip");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    let db = Db::open_memory().await.unwrap();
    let images = dir.path().join("images");
    assert!(import_mealie_backup(&db, &cut, &images).await.is_err());
    assert_empty(&db, &images).await;
}

#[tokio::test]
async fn a_failure_after_most_rows_are_in_rolls_everything_back() {
    // The last thing imported is a shopping item; point it at a unit that is not in the backup.
    let (_keep, zip) = mutated(|v| v["shopping_list_items"][0]["unit_id"] = "00000000000000000000000000000000".into());
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let images = dir.path().join("images");
    let err = import_mealie_backup(&db, &zip, &images).await.unwrap_err();
    assert!(matches!(err, mealier::Error::Validation(_)), "{err}");
    assert_empty(&db, &images).await;
    // and the same DB takes a good import afterwards
    import_mealie_backup(&db, &fixture(), &images).await.unwrap();
}

#[tokio::test]
async fn last_made_without_a_made_event_is_reported_not_turned_into_a_batch() {
    let (_keep, zip) = mutated(|v| {
        for h in v["households_to_recipes"].as_array_mut().unwrap() {
            h["last_made"] = "2026-01-02T03:04:05".into();
        }
    });
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, &zip, dir.path()).await.unwrap();
    assert_eq!(report.batches, 1);
    assert_eq!(report.skipped.iter().filter(|s| s.contains("last_made 2026-01-02T03:04:05")).count(), 1, "{:?}", report.skipped);
}

// ---------- S38: groups and households ----------

async fn count_in(db: &Db, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap()
}

#[tokio::test]
async fn the_fixture_lands_in_its_own_group_and_household() {
    let (db, report, _dir) = imported().await;
    assert_eq!((report.groups, report.households), (1, 1));
    let groups = db.groups().await.unwrap();
    assert_eq!(groups.iter().map(|g| (g.name.as_str(), g.slug.as_str())).collect::<Vec<_>>(), vec![("Fixture", "fixture")]);
    let households = db.households().await.unwrap();
    assert_eq!(households.iter().map(|h| (h.name.as_str(), h.slug.as_str(), h.group_id)).collect::<Vec<_>>(), vec![("Fixture", "fixture", groups[0].id)]);
    assert_eq!(db.default_household().await.unwrap(), households[0], "the first imported household becomes the default");

    let admin = user(&db, "fixture-admin").await;
    let member = user(&db, "fixture-user").await;
    assert_eq!([admin.household_id, member.household_id], [households[0].id; 2]);
    assert_eq!(admin.group_id, groups[0].id);

    let g = groups[0].id;
    let h = households[0].id;
    for t in ["recipes", "units", "foods", "tags", "categories", "tools", "cookbooks"] {
        assert_eq!(count_in(&db, &format!("SELECT count(*) FROM {t} WHERE group_id != {g}")).await, 0, "{t} outside the group");
        assert!(count_in(&db, &format!("SELECT count(*) FROM {t}")).await > 0, "{t} imported");
    }
    for t in ["batches", "meal_plan_entries", "shopping_lists"] {
        assert_eq!(count_in(&db, &format!("SELECT count(*) FROM {t} WHERE household_id != {h}")).await, 0, "{t} outside the household");
        assert!(count_in(&db, &format!("SELECT count(*) FROM {t}")).await > 0, "{t} imported");
    }
    let pref: (i64, bool) = sqlx::query_as("SELECT first_day_of_week, private_household FROM household_preferences WHERE household_id = ?").bind(h).fetch_one(db.pool()).await.unwrap();
    assert_eq!(pref, (0, false));
    let (private, extra): (bool, String) = sqlx::query_as("SELECT private_group, extra FROM group_preferences WHERE group_id = ?").bind(g).fetch_one(db.pool()).await.unwrap();
    assert!(!private);
    assert!(extra.contains("recipe_public"), "the other preference fields are kept: {extra}");
}

#[tokio::test]
async fn nobody_else_sees_what_was_imported() {
    let (db, _report, _dir) = imported().await;
    let g = db.create_group("Neighbours").await.unwrap();
    let h = db.create_household(g.id, "Next door").await.unwrap();
    let them = mealier::Scope { user_id: 0, group_id: g.id, household_id: h.id, admin: false };
    assert!(db.units(&them).await.unwrap().is_empty());
    assert!(db.foods(&them).await.unwrap().is_empty());
    assert!(db.list_recipes(&them, true, None).await.unwrap().is_empty());
    assert!(db.meal_plan(&them, "2000-01-01", "2999-12-31").await.unwrap().is_empty());
    assert!(db.shopping_lists(&them).await.unwrap().is_empty());
}

fn push(v: &mut serde_json::Value, table: &str, row: serde_json::Value) {
    v[table].as_array_mut().unwrap().push(row);
}

/// The fixture plus: a second household in its group (one user, a list, a plan entry, a made-it event),
/// and a second group with its own household, user, units, foods, tag, recipe, plan, list and made-it event.
fn two_groups(v: &mut serde_json::Value) {
    use serde_json::json;
    let f = "6a1a76f2eef844d7b079f8421d1d7b6d";
    let a_recipe = "9de853457e8748ff8f4712c3a19bc18c";
    let (h2, g2, gh) = ("00000000000000000000000000000h02", "00000000000000000000000000000g02", "000000000000000000000000000000h3");
    let (u3, u4) = ("0000000000000000000000000000u003", "0000000000000000000000000000u004");
    let (cup2, flour2, tag2, recipe2) = ("00000000000000000000000000000c02", "00000000000000000000000000000f02", "00000000000000000000000000000t02", "00000000000000000000000000000r02");
    push(v, "households", json!({"id": h2, "group_id": f, "name": "Fixture Two", "slug": "fixture-two"}));
    push(v, "household_preferences", json!({"id": "p2", "household_id": h2, "first_day_of_week": 3, "private_household": true}));
    push(v, "groups", json!({"id": g2, "name": "Second", "slug": "second"}));
    push(v, "group_preferences", json!({"id": "p3", "group_id": g2, "private_group": true, "first_day_of_week": 1}));
    push(v, "households", json!({"id": gh, "group_id": g2, "name": "Second Home", "slug": "second-home"}));
    push(v, "household_preferences", json!({"id": "p4", "household_id": gh, "first_day_of_week": 1, "private_household": true}));
    push(v, "users", json!({"id": u3, "username": "second-user", "email": "second@example.invalid", "full_name": "S", "admin": false, "group_id": f, "household_id": h2}));
    push(v, "users", json!({"id": u4, "username": "g2-user", "email": "g2@example.invalid", "full_name": "G", "admin": false, "group_id": g2, "household_id": gh}));
    push(v, "ingredient_units", json!({"id": cup2, "name": "mealier-fixture-cup", "abbreviation": "", "group_id": g2}));
    push(v, "ingredient_foods", json!({"id": flour2, "name": "mealier-fixture-flour", "group_id": g2}));
    push(v, "tags", json!({"id": tag2, "name": "mealier-fixture-tag-1", "group_id": g2}));
    push(v, "recipes", json!({"id": recipe2, "name": "g2-recipe", "group_id": g2, "description": "", "user_id": u4}));
    push(v, "recipes_ingredients", json!({"id": "i1", "recipe_id": recipe2, "position": 0, "quantity": 1.0, "unit_id": cup2, "food_id": flour2, "note": "", "original_text": "1 cup flour"}));
    push(v, "recipe_instructions", json!({"id": "s1", "recipe_id": recipe2, "position": 0, "text": "Mix."}));
    push(v, "recipes_to_tags", json!({"recipe_id": recipe2, "tag_id": tag2}));
    // second household of the first group
    push(v, "shopping_lists", json!({"id": "l2", "name": "second list", "group_id": f, "user_id": u3}));
    push(v, "group_meal_plans", json!({"id": 10, "date": "2026-12-05", "entry_type": "dinner", "group_id": f, "recipe_id": a_recipe, "title": "", "text": "", "user_id": u3}));
    push(v, "recipe_timeline_events", json!({"id": "e2", "event_type": "comment", "recipe_id": a_recipe, "subject": "second made it", "timestamp": "2026-11-07T10:00:00", "message": "by the second household", "user_id": u3}));
    // the second group
    push(v, "shopping_lists", json!({"id": "l3", "name": "g2 list", "group_id": g2, "user_id": u4}));
    push(v, "shopping_list_items", json!({"id": "it3", "shopping_list_id": "l3", "quantity": 2.0, "unit_id": cup2, "food_id": flour2, "note": "", "checked": false}));
    push(v, "group_meal_plans", json!({"id": 11, "date": "2026-12-06", "entry_type": "lunch", "group_id": g2, "recipe_id": recipe2, "title": "", "text": "", "user_id": u4}));
    push(v, "recipe_timeline_events", json!({"id": "e3", "event_type": "comment", "recipe_id": recipe2, "subject": "g2 made it", "timestamp": "2026-11-08T10:00:00", "message": "g2 batch", "user_id": u4}));
}

#[tokio::test]
async fn a_backup_with_two_groups_and_three_households_lands_each_in_its_own_tenant() {
    let (_keep, zip) = mutated(two_groups);
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report = import_mealie_backup(&db, &zip, dir.path()).await.unwrap();
    assert_eq!((report.groups, report.households, report.users), (2, 3, 4));
    assert_eq!(db.groups().await.unwrap().iter().map(|g| g.slug.as_str()).collect::<Vec<_>>(), vec!["fixture", "second"]);
    assert_eq!(db.households().await.unwrap().iter().map(|h| h.slug.as_str()).collect::<Vec<_>>(), vec!["fixture", "fixture-two", "second-home"]);
    assert_eq!(db.default_household().await.unwrap().slug, "fixture", "the first household in the backup");

    let (one, two, three) = (tenant(&db).await, mealier::Scope::from(&user(&db, "second-user").await), mealier::Scope::from(&user(&db, "g2-user").await));
    assert_eq!((one.group_id, two.group_id), (one.group_id, one.group_id));
    assert_ne!(three.group_id, one.group_id);
    assert_ne!(two.household_id, one.household_id);

    // group-scoped: the first group's households share recipes, units, foods; the second group has its own
    let names = |v: Vec<mealier::RecipeSummary>| v.into_iter().map(|r| r.name).collect::<Vec<_>>();
    assert_eq!(names(db.list_recipes(&one, true, None).await.unwrap()), names(db.list_recipes(&two, true, None).await.unwrap()));
    assert_eq!(db.list_recipes(&one, true, None).await.unwrap().len(), 2);
    assert_eq!(names(db.list_recipes(&three, true, None).await.unwrap()), vec!["g2-recipe"]);
    assert_eq!(db.units(&one).await.unwrap().len(), 26);
    assert_eq!(db.units(&three).await.unwrap().iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), vec!["mealier-fixture-cup"], "same name, its own unit");
    assert_eq!(db.foods(&one).await.unwrap().len(), 2689);
    assert_eq!(db.foods(&three).await.unwrap().len(), 1);
    assert_eq!(count_in(&db, "SELECT count(*) FROM tags WHERE name = 'mealier-fixture-tag-1'").await, 2, "one per group");
    assert_eq!(count_in(&db, "SELECT count(DISTINCT group_id) FROM tags").await, 2);

    // household-scoped: each household has only its own plan, lists and batches
    let plan = |sc: mealier::Scope| {
        let db = db.clone();
        async move { db.meal_plan(&sc, "2026-12-01", "2026-12-31").await.unwrap().iter().map(|e| e.date.clone()).collect::<Vec<_>>() }
    };
    assert_eq!(plan(one).await, vec!["2026-12-01", "2026-12-02"]);
    assert_eq!(plan(two).await, vec!["2026-12-05"]);
    assert_eq!(plan(three).await, vec!["2026-12-06"]);
    let lists = |sc: mealier::Scope| {
        let db = db.clone();
        async move { db.shopping_lists(&sc).await.unwrap().into_iter().map(|l| l.name).collect::<Vec<_>>() }
    };
    assert_eq!(lists(one).await, vec!["mealier-fixture-list"]);
    assert_eq!(lists(two).await, vec!["second list"]);
    assert_eq!(lists(three).await, vec!["g2 list"]);

    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let notes = |sc: mealier::Scope, id: i64| {
        let db = db.clone();
        async move { db.list_batches(&sc, id).await.unwrap().into_iter().map(|b| b.notes).collect::<Vec<_>>() }
    };
    assert_eq!(notes(one, a.id).await, vec!["made it, tasted good"]);
    assert_eq!(notes(two, a.id).await, vec!["by the second household"]);
    let g2_recipe = db.list_recipes(&three, true, None).await.unwrap()[0].id;
    assert_eq!(notes(three, g2_recipe).await, vec!["g2 batch"]);
    assert!(matches!(db.list_batches(&three, a.id).await, Err(mealier::Error::NotFound(_))), "the second group cannot see the first group's recipe");

    // the second group's ingredient resolved to the second group's unit and food
    let r = db.get_recipe(&three, g2_recipe).await.unwrap().unwrap();
    assert_eq!((r.content.ingredients[0].unit_name.as_deref(), r.content.ingredients[0].food_name.as_deref()), (Some("mealier-fixture-cup"), Some("mealier-fixture-flour")));
    let list = db.shopping_list(&three, db.shopping_lists(&three).await.unwrap()[0].id).await.unwrap();
    assert_eq!(list.items.len(), 1);
    let (first_pref,): (i64,) = sqlx::query_as("SELECT first_day_of_week FROM household_preferences WHERE household_id = ?").bind(three.household_id).fetch_one(db.pool()).await.unwrap();
    assert_eq!(first_pref, 1);
}

#[tokio::test]
async fn the_local_admin_who_was_already_there_moves_into_the_imported_default_household() {
    let db = Db::open_memory().await.unwrap();
    let admin = db.local_admin().await.unwrap();
    let cookie = db.create_session(admin.id).await.unwrap();
    assert_eq!(db.groups().await.unwrap().len(), 1, "Home was made for the local admin");

    let dir = tempfile::tempdir().unwrap();
    import_mealie_backup(&db, &fixture(), dir.path()).await.unwrap();

    let fixture_home = db.default_household().await.unwrap();
    assert_eq!(fixture_home.slug, "fixture");
    assert_eq!(db.groups().await.unwrap().iter().map(|g| g.slug.as_str()).collect::<Vec<_>>(), vec!["fixture"], "the empty Home group is gone");
    assert_eq!(db.households().await.unwrap().len(), 1);
    let after = db.user(admin.id).await.unwrap();
    assert_eq!((after.household_id, after.group_id, after.role.as_str()), (fixture_home.id, fixture_home.group_id, "admin"));
    let sc = mealier::Scope::from(&db.session_user(&cookie).await.unwrap().expect("their session survives the import"));
    assert_eq!(db.list_recipes(&sc, true, None).await.unwrap().len(), 2, "and sees what was imported");
    assert_eq!(db.users().await.unwrap().len(), 3, "two imported users and the local admin");
}

#[tokio::test]
async fn an_imported_user_called_admin_pushes_the_local_admin_aside() {
    let (_keep, zip) = mutated(|v| v["users"][0]["username"] = "admin".into());
    let db = Db::open_memory().await.unwrap();
    let local = db.local_admin().await.unwrap();
    assert_eq!(local.username, "admin");
    let dir = tempfile::tempdir().unwrap();
    import_mealie_backup(&db, &zip, dir.path()).await.unwrap();
    assert_eq!(db.user(local.id).await.unwrap().username, "local-admin");
    assert_eq!(user(&db, "admin").await.email, "fixture-admin@example.invalid");
}

#[tokio::test]
async fn tenants_users_or_data_other_than_the_local_admins_home_still_refuse_an_import() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_memory().await.unwrap();
    db.create_group("Other").await.unwrap();
    let err = import_mealie_backup(&db, &fixture(), dir.path()).await.unwrap_err();
    assert!(matches!(&err, mealier::Error::Validation(m) if m.contains("not empty")), "{err}");

    let db = Db::open_memory().await.unwrap();
    let ident = mealier::OidcIdentity { issuer: "i".into(), subject: "s".into(), email: "a@x".into(), name: "a".into(), username: "a".into(), admin: false };
    db.oidc_login(&ident).await.unwrap();
    assert!(import_mealie_backup(&db, &fixture(), dir.path()).await.is_err(), "a real user is not the local admin");

    let db = Db::open_memory().await.unwrap();
    db.local_admin().await.unwrap();
    let sc = common::scope(&db).await;
    db.add_unit(&sc, "pinch", Dimension::Other, None).await.unwrap();
    assert!(import_mealie_backup(&db, &fixture(), dir.path()).await.is_err(), "data in Home counts");
    let gs: i64 = count_in(&db, "SELECT count(*) FROM groups").await;
    assert_eq!(gs, 1, "and nothing was removed");
}

#[tokio::test]
async fn a_backup_that_crosses_groups_or_lacks_households_is_refused_whole() {
    let dir = tempfile::tempdir().unwrap();
    let images = dir.path().join("images");
    // a unit of the fixture group moved to another group while recipes still use it
    let (_k1, zip) = mutated(|v| {
        two_groups(v);
        for u in v["ingredient_units"].as_array_mut().unwrap() {
            if u["name"] == "mealier-fixture-cup" && u["group_id"] == "6a1a76f2eef844d7b079f8421d1d7b6d" {
                u["group_id"] = "00000000000000000000000000000g02".into();
            }
        }
    });
    let db = Db::open_memory().await.unwrap();
    let err = import_mealie_backup(&db, &zip, &images).await.unwrap_err();
    assert!(matches!(err, mealier::Error::NotFound(_) | mealier::Error::Validation(_)), "{err}");
    assert_empty(&db, &images).await;

    // a shopping item whose unit belongs to the other group
    let (_k2, zip) = mutated(|v| {
        two_groups(v);
        v["shopping_list_items"][0]["unit_id"] = "00000000000000000000000000000c02".into();
    });
    let err = import_mealie_backup(&db, &zip, &images).await.unwrap_err();
    assert!(matches!(err, mealier::Error::Validation(_)), "{err}");
    assert_empty(&db, &images).await;

    // users that point at a household the backup does not contain
    let (_k3, zip) = mutated(|v| v["households"] = serde_json::json!([]));
    let err = import_mealie_backup(&db, &zip, &images).await.unwrap_err();
    assert!(matches!(err, mealier::Error::Validation(_)), "{err}");
    assert_empty(&db, &images).await;
}
