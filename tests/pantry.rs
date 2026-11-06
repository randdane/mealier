//! Step 14, part B: foods on hand and tools owned, per household; adding a recipe to a list can skip
//! what the household has.
mod common;
use mealier::{Db, Dimension, Error, IngredientInput, RecipeInput};

fn line(food: i64, unit: i64) -> IngredientInput {
    IngredientInput { qty_min: Some(1.0), qty_max: Some(1.0), unit_id: Some(unit), food_id: Some(food), ..Default::default() }
}

#[tokio::test]
async fn a_household_marks_foods_and_tools_and_another_does_not_see_it() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let flour = db.add_food(&t.a, "flour").await.unwrap();
    db.create_recipe(&t.a, RecipeInput { name: "Bread".into(), tools: vec!["whisk".into()], ..Default::default() }).await.unwrap();
    let before = db.pantry(&t.a).await.unwrap();
    assert!(before.foods.iter().all(|f| !f.have) && before.tools.iter().all(|f| !f.have));
    let whisk = before.tools[0].id;

    db.set_food_on_hand(&t.a, flour, true).await.unwrap();
    db.set_tool_owned(&t.a, whisk, true).await.unwrap();
    db.set_food_on_hand(&t.a, flour, true).await.unwrap(); // again: no error, still one row
    let mine = db.pantry(&t.a).await.unwrap();
    assert_eq!((mine.foods[0].name.as_str(), mine.foods[0].have, mine.tools[0].have), ("flour", true, true));
    let theirs = db.pantry(&t.b).await.unwrap();
    assert_eq!((theirs.foods[0].name.as_str(), theirs.foods[0].have, theirs.tools[0].have), ("flour", false, false), "same group, other household: the food is shared, having it is not");
    assert!(db.pantry(&t.c).await.unwrap().foods.is_empty(), "another group sees none of the group's foods");

    db.set_food_on_hand(&t.a, flour, false).await.unwrap();
    db.set_tool_owned(&t.a, whisk, false).await.unwrap();
    let after = db.pantry(&t.a).await.unwrap();
    assert!(!after.foods[0].have && !after.tools[0].have);
}

#[tokio::test]
async fn a_food_or_tool_of_another_group_is_not_found_and_nothing_is_written() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let flour = db.add_food(&t.a, "flour").await.unwrap();
    db.create_recipe(&t.a, RecipeInput { name: "Bread".into(), tools: vec!["whisk".into()], ..Default::default() }).await.unwrap();
    let whisk = db.pantry(&t.a).await.unwrap().tools[0].id;
    assert!(matches!(db.set_food_on_hand(&t.c, flour, true).await, Err(Error::NotFound(_))));
    assert!(matches!(db.set_tool_owned(&t.c, whisk, true).await, Err(Error::NotFound(_))));
    let n: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM household_foods) + (SELECT count(*) FROM household_tools)").fetch_one(db.pool()).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn adding_a_recipe_can_skip_foods_on_hand_and_defaults_to_adding_everything() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let g = db.add_unit(&t.a, "g", Dimension::Mass, Some(1.0)).await.unwrap();
    let (flour, salt) = (db.add_food(&t.a, "flour").await.unwrap(), db.add_food(&t.a, "salt").await.unwrap());
    let r = db.create_recipe(&t.a, RecipeInput { name: "Bread".into(), ingredients: vec![line(flour, g), line(salt, g)], ..Default::default() }).await.unwrap().id;
    db.set_food_on_hand(&t.a, salt, true).await.unwrap();

    let all = db.create_list(&t.a, "all").await.unwrap().id;
    assert_eq!(db.add_recipe_to_list(&t.a, all, r, 1.0).await.unwrap().items.len(), 2);
    let skip = db.create_list(&t.a, "skip").await.unwrap().id;
    let list = db.add_recipe_to_list_with(&t.a, skip, r, 1.0, true).await.unwrap();
    assert_eq!(list.items.iter().map(|i| i.food_id).collect::<Vec<_>>(), [Some(flour)]);
    let keep = db.create_list(&t.a, "keep").await.unwrap().id;
    assert_eq!(db.add_recipe_to_list_with(&t.a, keep, r, 1.0, false).await.unwrap().items.len(), 2);

    // another household has nothing on hand: nothing is skipped for it
    let other = db.create_list(&t.b, "b").await.unwrap().id;
    assert_eq!(db.add_recipe_to_list_with(&t.b, other, r, 1.0, true).await.unwrap().items.len(), 2);
}

#[tokio::test]
async fn skipping_keeps_lines_without_a_food_and_still_validates() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "X".into(), ingredients: vec![IngredientInput { original_text: "a pinch of love".into(), ..Default::default() }], ..Default::default() }).await.unwrap().id;
    let l = db.create_list(&sc, "L").await.unwrap().id;
    assert_eq!(db.add_recipe_to_list_with(&sc, l, r, 1.0, true).await.unwrap().items.len(), 1);
    assert!(matches!(db.add_recipe_to_list_with(&sc, l, r, 0.0, true).await, Err(Error::Validation(_))));
    assert!(matches!(db.add_recipe_to_list_with(&sc, l, 99, 1.0, true).await, Err(Error::NotFound(_))));
    assert!(matches!(db.add_recipe_to_list_with(&sc, 99, r, 1.0, true).await, Err(Error::NotFound(_))));
}
