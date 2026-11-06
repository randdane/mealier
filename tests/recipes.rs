use mealier::{Db, Dimension, IngredientInput, RecipeInput};

fn input(name: &str) -> RecipeInput {
    RecipeInput { name: name.into(), ..Default::default() }
}

#[tokio::test]
async fn create_writes_one_snapshot() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(input("Pancakes")).await.unwrap();

    let snaps = db.snapshots(r.id).await.unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(r.current_snapshot_id, snaps[0].id);
    assert_eq!(r.name, "Pancakes");
}

#[tokio::test]
async fn update_writes_another_snapshot_and_get_returns_new_content() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(input("Pancakes")).await.unwrap();

    let mut v2 = input("Better Pancakes");
    v2.tags = vec!["breakfast".into()];
    v2.steps = vec!["Mix".into(), "Fry".into()];
    let updated = db.update_recipe(r.id, v2).await.unwrap();

    let snaps = db.snapshots(r.id).await.unwrap();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].id, updated.current_snapshot_id, "newest first");
    let got = db.get_recipe(r.id).await.unwrap().unwrap();
    assert_eq!(got.name, "Better Pancakes");
    assert_eq!(got.tags, vec!["breakfast"]);
    assert_eq!(got.steps, vec!["Mix", "Fry"]);
    assert_eq!(got.current_snapshot_id, snaps[0].id);
}

#[tokio::test]
async fn old_snapshot_content_is_unchanged_after_update() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(input("Pancakes")).await.unwrap();
    let first = r.current_snapshot_id;

    db.update_recipe(r.id, input("Waffles")).await.unwrap();

    let old = db.snapshot(first).await.unwrap();
    assert_eq!(old.recipe_id, r.id);
    assert_eq!(old.content.name, "Pancakes");
}

#[tokio::test]
async fn renaming_unit_and_food_does_not_change_snapshot_names() {
    let db = Db::open_memory().await.unwrap();
    let cup = db.add_unit("cup", Dimension::Volume, Some(240.0)).await.unwrap();
    let flour = db.add_food("flour").await.unwrap();
    let mut i = input("Bread");
    i.ingredients = vec![IngredientInput {
        qty_min: Some(2.0),
        qty_max: Some(3.0),
        unit_id: Some(cup),
        food_id: Some(flour),
        note: "sifted".into(),
        original_text: "2-3 cups flour, sifted".into(),
    }];
    let r = db.create_recipe(i).await.unwrap();

    db.rename_unit(cup, "c").await.unwrap();
    db.rename_food(flour, "plain flour").await.unwrap();

    let line = &db.snapshot(r.current_snapshot_id).await.unwrap().content.ingredients[0];
    assert_eq!(line.unit_name.as_deref(), Some("cup"));
    assert_eq!(line.unit_factor, Some(240.0));
    assert_eq!(line.food_name.as_deref(), Some("flour"));
    assert_eq!((line.qty_min, line.qty_max), (Some(2.0), Some(3.0)));
    assert_eq!(line.original_text, "2-3 cups flour, sifted");
}

#[tokio::test]
async fn archived_recipes_are_hidden_from_default_list_but_snapshots_remain() {
    let db = Db::open_memory().await.unwrap();
    let keep = db.create_recipe(input("Soup")).await.unwrap();
    let gone = db.create_recipe(input("Stew")).await.unwrap();

    db.archive_recipe(gone.id).await.unwrap();

    let names: Vec<_> = db.list_recipes(false).await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(names, vec!["Soup"]);
    let all = db.list_recipes(true).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|r| r.id == gone.id && r.archived));
    assert!(all.iter().any(|r| r.id == keep.id && !r.archived));
    assert_eq!(db.snapshots(gone.id).await.unwrap().len(), 1);
    assert_eq!(db.snapshot(gone.current_snapshot_id).await.unwrap().content.name, "Stew");
    assert!(db.get_recipe(gone.id).await.unwrap().unwrap().archived);
}

#[tokio::test]
async fn empty_name_is_a_validation_error_and_writes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let err = db.create_recipe(input("   ")).await.unwrap_err();
    assert!(matches!(err, mealier::Error::Validation(_)), "{err:?}");
    assert!(db.list_recipes(true).await.unwrap().is_empty());

    let r = db.create_recipe(input("Soup")).await.unwrap();
    let err = db.update_recipe(r.id, input("")).await.unwrap_err();
    assert!(matches!(err, mealier::Error::Validation(_)), "{err:?}");
    assert_eq!(db.snapshots(r.id).await.unwrap().len(), 1);
}

// The trigger has no API surface by design, so this is the one test that
// issues SQL itself.
#[tokio::test]
async fn snapshot_rows_cannot_be_updated_or_deleted() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(input("Soup")).await.unwrap();
    let id = r.current_snapshot_id;

    let upd = sqlx::query("UPDATE recipe_snapshots SET content = '{}' WHERE id = ?").bind(id).execute(db.pool()).await;
    assert!(upd.unwrap_err().to_string().contains("immutable"));
    let del = sqlx::query("DELETE FROM recipe_snapshots WHERE id = ?").bind(id).execute(db.pool()).await;
    assert!(del.unwrap_err().to_string().contains("immutable"));
    assert_eq!(db.snapshot(id).await.unwrap().content.name, "Soup");
}
