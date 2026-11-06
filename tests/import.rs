use std::path::{Path, PathBuf};

use mealier::{Db, Dimension, ImportReport, import_mealie_backup};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mealie_v3.28.0.zip")
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

    let units = db.units().await.unwrap();
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
    let foods = db.foods().await.unwrap();
    let flour = foods.iter().find(|f| f.name == "mealier-fixture-flour").unwrap();
    assert_eq!(flour.aliases, vec!["mealier-fixture-flour-alias"]);
    assert_eq!(foods.iter().find(|f| f.name == "mealier-fixture-salt").unwrap().aliases, Vec::<String>::new());
}

async fn recipe(db: &Db, name: &str) -> mealier::Recipe {
    let id = db.list_recipes(true, None).await.unwrap().into_iter().find(|r| r.name == name).unwrap().id;
    db.get_recipe(id).await.unwrap().unwrap()
}

#[tokio::test]
async fn recipe_a_is_imported_with_one_snapshot_and_its_fields() {
    let (db, report, _dir) = imported().await;
    assert_eq!(
        (report.recipes, report.tags, report.categories, report.cookbooks, report.tools),
        (2, 2, 2, 1, 1)
    );
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    assert_eq!(db.snapshots(a.id).await.unwrap().len(), 1);
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

    let batches = db.list_batches(a.id).await.unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!((batch.status.as_str(), batch.source.as_str(), batch.snapshot_id), ("done", "imported", None));
    assert_eq!((batch.made_on.as_deref(), batch.scale, batch.rating), (Some("2026-11-06"), 1.0, None));
    assert_eq!(batch.notes, "made it, tasted good");
    assert!(db.list_batches(b.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn other_timeline_events_are_reported_and_the_comment_is_kept() {
    let (db, report, _dir) = imported().await;
    assert!(report.skipped.iter().any(|s| s.contains("mealier-fixture info event")), "{:?}", report.skipped);
    assert!(report.skipped.iter().any(|s| s.contains("2 system timeline events")), "{:?}", report.skipped);
    assert!(!report.skipped.iter().any(|s| s.contains("last_made")), "a's last_made has its made event");

    assert_eq!(report.comments, 1);
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let comments = db.comments(a.id).await.unwrap();
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

    let plan = db.meal_plan("2026-12-01", "2026-12-31").await.unwrap();
    let got: Vec<_> = plan.iter().map(|e| (e.date.as_str(), e.slot.as_str(), e.recipe_id, e.title.as_str(), e.text.as_str(), e.batch_id)).collect();
    assert_eq!(
        got,
        vec![
            ("2026-12-01", "dinner", Some(a.id), "mealier-fixture planned recipe", "", None),
            ("2026-12-02", "lunch", None, "mealier-fixture note entry", "eat out, no recipe", None),
        ]
    );
    assert_eq!(db.meal_plan("2026-12-02", "2026-12-02").await.unwrap().len(), 1, "range is inclusive");
    assert!(db.meal_plan("2026-12-03", "2026-12-31").await.unwrap().is_empty());
}

#[tokio::test]
async fn shopping_list_items_keep_checked_state_amounts_and_recipe() {
    let (db, report, _dir) = imported().await;
    assert_eq!((report.shopping_lists, report.shopping_list_items), (1, 4));
    let a = recipe(&db, "mealier-fixture-recipe-a").await;
    let units = db.units().await.unwrap();
    let cup = units.iter().find(|u| u.name == "mealier-fixture-cup").unwrap().id;
    let flour = db.foods().await.unwrap().into_iter().find(|f| f.name == "mealier-fixture-flour").unwrap().id;

    let lists = db.shopping_lists().await.unwrap();
    assert_eq!(lists.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["mealier-fixture-list"]);
    let list = db.shopping_list(lists[0].id).await.unwrap();
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
    let imgs = db.recipe_images(a.id).await.unwrap();
    assert_eq!(imgs.iter().map(|i| (i.hash.as_str(), i.mime.as_str())).collect::<Vec<_>>(), vec![(hash, "image/webp")]);
    assert!(db.recipe_images(b.id).await.unwrap().is_empty());
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
    assert!(db.units().await.unwrap().is_empty());
    assert!(db.foods().await.unwrap().is_empty());
    assert!(db.list_recipes(true, None).await.unwrap().is_empty());
    assert!(db.users().await.unwrap().is_empty());
    assert!(db.shopping_lists().await.unwrap().is_empty());
    assert!(!images.exists() || std::fs::read_dir(images).unwrap().next().is_none());
}

#[tokio::test]
async fn a_non_empty_database_is_refused_and_left_as_it_was() {
    let (db, _report, dir) = imported().await;
    let err = import_mealie_backup(&db, &fixture(), dir.path()).await.unwrap_err();
    assert!(matches!(&err, mealier::Error::Validation(m) if m.contains("not empty")), "{err}");
    assert_eq!(db.units().await.unwrap().len(), 26);
    assert_eq!(db.list_recipes(true, None).await.unwrap().len(), 2);

    let db = Db::open_memory().await.unwrap();
    db.add_food("salt").await.unwrap();
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
