mod common;
use mealier::{Db, Dimension, IngredientInput, RecipeInput};

fn input(name: &str) -> RecipeInput {
    RecipeInput { name: name.into(), ..Default::default() }
}

#[tokio::test]
async fn create_writes_one_snapshot() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(&common::scope(&db).await, input("Pancakes")).await.unwrap();

    let snaps = db.snapshots(&common::scope(&db).await, r.id).await.unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(r.current_snapshot_id, snaps[0].id);
    assert_eq!(r.content.name, "Pancakes");
}

#[tokio::test]
async fn update_writes_another_snapshot_and_get_returns_new_content() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(&common::scope(&db).await, input("Pancakes")).await.unwrap();

    let mut v2 = input("Better Pancakes");
    v2.tags = vec!["breakfast".into()];
    v2.steps = vec!["Mix".into(), "Fry".into()];
    let updated = db.update_recipe(&common::scope(&db).await, r.id, v2).await.unwrap();

    let snaps = db.snapshots(&common::scope(&db).await, r.id).await.unwrap();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].id, updated.current_snapshot_id, "newest first");
    let got = db.get_recipe(&common::scope(&db).await, r.id).await.unwrap().unwrap();
    assert_eq!(got.content.name, "Better Pancakes");
    assert_eq!(got.content.tags, vec!["breakfast"]);
    assert_eq!(got.content.steps, vec!["Mix", "Fry"]);
    assert_eq!(got.current_snapshot_id, snaps[0].id);
}

#[tokio::test]
async fn old_snapshot_content_is_unchanged_after_update() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(&common::scope(&db).await, input("Pancakes")).await.unwrap();
    let first = r.current_snapshot_id;

    db.update_recipe(&common::scope(&db).await, r.id, input("Waffles")).await.unwrap();

    let old = db.snapshot(&common::scope(&db).await, first).await.unwrap();
    assert_eq!(old.recipe_id, r.id);
    assert_eq!(old.content.name, "Pancakes");
}

#[tokio::test]
async fn renaming_unit_and_food_does_not_change_snapshot_names() {
    let db = Db::open_memory().await.unwrap();
    let cup = db.add_unit(&common::scope(&db).await, "cup", Dimension::Volume, Some(240.0)).await.unwrap();
    let flour = db.add_food(&common::scope(&db).await, "flour").await.unwrap();
    let mut i = input("Bread");
    i.ingredients = vec![IngredientInput {
        qty_min: Some(2.0),
        qty_max: Some(3.0),
        unit_id: Some(cup),
        food_id: Some(flour),
        note: "sifted".into(),
        original_text: "2-3 cups flour, sifted".into(),
    }];
    let r = db.create_recipe(&common::scope(&db).await, i).await.unwrap();

    db.rename_unit(&common::scope(&db).await, cup, "c").await.unwrap();
    db.rename_food(&common::scope(&db).await, flour, "plain flour").await.unwrap();

    let line = &db.snapshot(&common::scope(&db).await, r.current_snapshot_id).await.unwrap().content.ingredients[0];
    assert_eq!(line.unit_name.as_deref(), Some("cup"));
    assert_eq!(line.unit_factor, Some(240.0));
    assert_eq!(line.food_name.as_deref(), Some("flour"));
    assert_eq!((line.qty_min, line.qty_max), (Some(2.0), Some(3.0)));
    assert_eq!(line.original_text, "2-3 cups flour, sifted");
}

#[tokio::test]
async fn archived_recipes_are_hidden_from_default_list_but_snapshots_remain() {
    let db = Db::open_memory().await.unwrap();
    let keep = db.create_recipe(&common::scope(&db).await, input("Soup")).await.unwrap();
    let gone = db.create_recipe(&common::scope(&db).await, input("Stew")).await.unwrap();

    db.archive_recipe(&common::scope(&db).await, gone.id).await.unwrap();

    let names: Vec<_> = db.list_recipes(&common::scope(&db).await, false, None).await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(names, vec!["Soup"]);
    let all = db.list_recipes(&common::scope(&db).await, true, None).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|r| r.id == gone.id && r.archived));
    assert!(all.iter().any(|r| r.id == keep.id && !r.archived));
    assert_eq!(db.snapshots(&common::scope(&db).await, gone.id).await.unwrap().len(), 1);
    assert_eq!(db.snapshot(&common::scope(&db).await, gone.current_snapshot_id).await.unwrap().content.name, "Stew");
    assert!(db.get_recipe(&common::scope(&db).await, gone.id).await.unwrap().unwrap().archived);
}

#[tokio::test]
async fn empty_name_is_a_validation_error_and_writes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let err = db.create_recipe(&common::scope(&db).await, input("   ")).await.unwrap_err();
    assert!(matches!(err, mealier::Error::Validation(_)), "{err:?}");
    assert!(db.list_recipes(&common::scope(&db).await, true, None).await.unwrap().is_empty());

    let r = db.create_recipe(&common::scope(&db).await, input("Soup")).await.unwrap();
    let err = db.update_recipe(&common::scope(&db).await, r.id, input("")).await.unwrap_err();
    assert!(matches!(err, mealier::Error::Validation(_)), "{err:?}");
    assert_eq!(db.snapshots(&common::scope(&db).await, r.id).await.unwrap().len(), 1);
}

// The trigger has no API surface by design, so this is the one test that
// issues SQL itself.
#[tokio::test]
async fn snapshot_rows_cannot_be_updated_or_deleted() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(&common::scope(&db).await, input("Soup")).await.unwrap();
    let id = r.current_snapshot_id;

    let upd = sqlx::query("UPDATE recipe_snapshots SET content = '{}' WHERE id = ?").bind(id).execute(db.pool()).await;
    assert!(upd.is_err());
    let del = sqlx::query("DELETE FROM recipe_snapshots WHERE id = ?").bind(id).execute(db.pool()).await;
    assert!(del.is_err());
    assert_eq!(db.snapshot(&common::scope(&db).await, id).await.unwrap().content.name, "Soup");
}

fn with_qty(min: Option<f64>, max: Option<f64>) -> RecipeInput {
    let mut i = input("Bread");
    i.ingredients = vec![IngredientInput { qty_min: min, qty_max: max, ..Default::default() }];
    i
}

#[tokio::test]
async fn single_quantity_fills_max() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(&common::scope(&db).await, with_qty(Some(2.0), None)).await.unwrap();
    let line = &db.snapshot(&common::scope(&db).await, r.current_snapshot_id).await.unwrap().content.ingredients[0];
    assert_eq!((line.qty_min, line.qty_max), (Some(2.0), Some(2.0)));
    let u = db.update_recipe(&common::scope(&db).await, r.id, with_qty(Some(1.5), None)).await.unwrap();
    assert_eq!((u.content.ingredients[0].qty_min, u.content.ingredients[0].qty_max), (Some(1.5), Some(1.5)));
}

#[tokio::test]
async fn bad_quantities_are_rejected() {
    let db = Db::open_memory().await.unwrap();
    for (min, max) in [(Some(3.0), Some(2.0)), (None, Some(2.0)), (Some(-1.0), None), (Some(1.0), Some(-2.0))] {
        let err = db.create_recipe(&common::scope(&db).await, with_qty(min, max)).await.unwrap_err();
        assert!(matches!(err, mealier::Error::Validation(_)), "{min:?} {max:?}: {err:?}");
    }
    assert!(db.list_recipes(&common::scope(&db).await, true, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn unit_factor_must_match_dimension() {
    let db = Db::open_memory().await.unwrap();
    for (dim, factor) in [
        (Dimension::Volume, None),
        (Dimension::Mass, Some(0.0)),
        (Dimension::Mass, Some(-1.0)),
        (Dimension::Count, Some(1.0)),
        (Dimension::Other, Some(2.0)),
    ] {
        let err = db.add_unit(&common::scope(&db).await, "x", dim, factor).await.unwrap_err();
        assert!(matches!(err, mealier::Error::Validation(_)), "{dim:?} {factor:?}: {err:?}");
    }
    db.add_unit(&common::scope(&db).await, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    db.add_unit(&common::scope(&db).await, "pinch", Dimension::Other, None).await.unwrap();
}

#[tokio::test]
async fn units_table_rejects_bad_factor_in_sql() {
    let db = Db::open_memory().await.unwrap();
    let group = common::scope(&db).await.group_id;
    let bad = sqlx::query("INSERT INTO units (group_id, name, dimension, factor) VALUES (?, 'x', 'volume', NULL)")
        .bind(group)
        .execute(db.pool())
        .await;
    assert!(bad.is_err());
    let bad = sqlx::query("INSERT INTO units (group_id, name, dimension, factor) VALUES (?, 'y', 'count', 1)")
        .bind(group)
        .execute(db.pool())
        .await;
    assert!(bad.is_err());
    // control: the same statement with a factor that fits the dimension goes in, so the CHECK was what refused the two above
    sqlx::query("INSERT INTO units (group_id, name, dimension, factor) VALUES (?, 'z', 'volume', 1)").bind(group).execute(db.pool()).await.unwrap();
}

#[tokio::test]
async fn search_matches_name_or_description_case_insensitively_and_escapes_wildcards() {
    let db = Db::open_memory().await.unwrap();
    db.create_recipe(&common::scope(&db).await, input("Tomato Soup")).await.unwrap();
    let mut d = input("Stew");
    d.description = "Rich 100% beef".into();
    db.create_recipe(&common::scope(&db).await, d).await.unwrap();
    db.create_recipe(&common::scope(&db).await, input("Snake_case")).await.unwrap();

    let names = |q: &'static str| {
        let db = db.clone();
        async move { db.list_recipes(&common::scope(&db).await, false, Some(q)).await.unwrap().into_iter().map(|r| r.name).collect::<Vec<_>>() }
    };
    assert_eq!(names("tomato").await, vec!["Tomato Soup"]);
    assert_eq!(names("BEEF").await, vec!["Stew"]);
    assert_eq!(names("100%").await, vec!["Stew"]);
    assert_eq!(names("%").await, vec!["Stew"]);
    assert_eq!(names("e_c").await, vec!["Snake_case"]);
    assert!(names("zzz").await.is_empty());
    assert_eq!(db.list_recipes(&common::scope(&db).await, false, None).await.unwrap().len(), 3);
}
