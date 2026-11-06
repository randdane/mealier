use mealier::{BatchLineInput, Db, Dimension, Error, IngredientInput, ItemInput, RecipeInput};

struct Ids {
    g: i64,
    kg: i64,
    ml: i64,
    cup: i64,
    tsp: i64,
    flour: i64,
    milk: i64,
    egg: i64,
    salt: i64,
}

async fn setup(db: &Db) -> Ids {
    Ids {
        g: db.add_unit("g", Dimension::Mass, Some(1.0)).await.unwrap(),
        kg: db.add_unit("kg", Dimension::Mass, Some(1000.0)).await.unwrap(),
        ml: db.add_unit("ml", Dimension::Volume, Some(1.0)).await.unwrap(),
        cup: db.add_unit("cup", Dimension::Volume, Some(236.5882365)).await.unwrap(),
        tsp: db.add_unit("tsp", Dimension::Volume, Some(4.92892159375)).await.unwrap(),
        flour: db.add_food("flour").await.unwrap(),
        milk: db.add_food("milk").await.unwrap(),
        egg: db.add_food("egg").await.unwrap(),
        salt: db.add_food("salt").await.unwrap(),
    }
}

fn ing(qty: Option<(f64, f64)>, unit: Option<i64>, food: i64) -> IngredientInput {
    IngredientInput { qty_min: qty.map(|q| q.0), qty_max: qty.map(|q| q.1), unit_id: unit, food_id: Some(food), ..Default::default() }
}

async fn recipe(db: &Db, name: &str, ingredients: Vec<IngredientInput>) -> i64 {
    db.create_recipe(RecipeInput { name: name.into(), ingredients, ..Default::default() }).await.unwrap().id
}

fn item(qty: Option<f64>, unit: Option<i64>, food: i64) -> ItemInput {
    ItemInput { qty, unit_id: unit, food_id: Some(food), ..Default::default() }
}

fn r3(n: f64) -> f64 {
    (n * 1000.0).round() / 1000.0
}

#[tokio::test]
async fn create_list_and_reject_an_empty_name() {
    let db = Db::open_memory().await.unwrap();
    let l = db.create_list("Weekly").await.unwrap();
    assert_eq!(l.name, "Weekly");
    assert_eq!(db.shopping_list(l.id).await.unwrap().items.len(), 0);
    assert!(matches!(db.create_list("  ").await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn add_hand_item_with_amount_unit_food_and_note() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    let it = db.add_item(l, ItemInput { note: "sifted".into(), ..item(Some(2.0), Some(i.kg), i.flour) }).await.unwrap();
    assert_eq!((it.qty, it.qty_max, it.unit_id, it.food_id, it.note.as_str(), it.checked, it.recipe_id), (Some(2.0), Some(2.0), Some(i.kg), Some(i.flour), "sifted", false, None));
}

#[tokio::test]
async fn add_hand_item_with_a_note_only() {
    let db = Db::open_memory().await.unwrap();
    let l = db.create_list("L").await.unwrap().id;
    let it = db.add_item(l, ItemInput { note: "paper towels".into(), ..Default::default() }).await.unwrap();
    assert_eq!((it.qty, it.food_id, it.note.as_str()), (None, None, "paper towels"));
}

#[tokio::test]
async fn add_item_rejects_nothing_to_buy_a_reversed_range_and_a_missing_list_or_food() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    assert!(matches!(db.add_item(l, ItemInput::default()).await, Err(Error::Validation(_))));
    assert!(matches!(db.add_item(l, ItemInput { qty_max: Some(1.0), ..item(Some(2.0), None, i.egg) }).await, Err(Error::Validation(_))));
    assert!(matches!(db.add_item(99, item(Some(1.0), None, i.egg)).await, Err(Error::NotFound(_))));
    assert!(matches!(db.add_item(l, item(Some(1.0), None, 999)).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn check_uncheck_update_and_delete_an_item() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    let it = db.add_item(l, item(Some(1.0), None, i.egg)).await.unwrap();
    db.set_item_checked(it.id, true).await.unwrap();
    assert!(db.shopping_list(l).await.unwrap().items[0].checked);
    db.set_item_checked(it.id, false).await.unwrap();
    assert!(!db.shopping_list(l).await.unwrap().items[0].checked);
    let u = db.update_item(it.id, ItemInput { checked: true, ..item(Some(6.0), None, i.egg) }).await.unwrap();
    assert_eq!((u.qty, u.checked), (Some(6.0), true));
    db.delete_item(it.id).await.unwrap();
    assert!(db.shopping_list(l).await.unwrap().items.is_empty());
    assert!(matches!(db.delete_item(it.id).await, Err(Error::NotFound(_))));
    assert!(matches!(db.set_item_checked(it.id, true).await, Err(Error::NotFound(_))));
    assert!(matches!(db.update_item(it.id, item(Some(1.0), None, i.egg)).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn recipe_lines_are_scaled_and_record_the_recipe() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let r = recipe(&db, "Bread", vec![ing(Some((100.0, 100.0)), Some(i.g), i.flour), ing(None, None, i.salt)]).await;
    let l = db.create_list("L").await.unwrap().id;
    let list = db.add_recipe_to_list(l, r, 3.0).await.unwrap();
    let got: Vec<_> = list.items.iter().map(|x| (x.qty, x.qty_max, x.unit_id, x.food_id, x.recipe_id)).collect();
    assert_eq!(got, vec![(Some(300.0), Some(300.0), Some(i.g), Some(i.flour), Some(r)), (None, None, None, Some(i.salt), Some(r))]);
}

#[tokio::test]
async fn a_line_with_no_food_keeps_its_original_text_as_the_note() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(RecipeInput { name: "X".into(), ingredients: vec![IngredientInput { original_text: "a pinch of love".into(), ..Default::default() }], ..Default::default() }).await.unwrap().id;
    let l = db.create_list("L").await.unwrap().id;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!((list.items[0].food_id, list.items[0].note.as_str()), (None, "a pinch of love"));
}

#[tokio::test]
async fn adding_a_recipe_validates_scale_recipe_and_list() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "X", vec![]).await;
    let l = db.create_list("L").await.unwrap().id;
    assert!(matches!(db.add_recipe_to_list(l, r, 0.0).await, Err(Error::Validation(_))));
    assert!(matches!(db.add_recipe_to_list(l, 99, 1.0).await, Err(Error::NotFound(_))));
    assert!(matches!(db.add_recipe_to_list(99, r, 1.0).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn merge_200_g_flour_plus_0_5_kg_flour_is_700_g() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    db.add_item(l, item(Some(200.0), Some(i.g), i.flour)).await.unwrap();
    let r = recipe(&db, "Bread", vec![ing(Some((0.5, 0.5)), Some(i.kg), i.flour)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!((r3(list.items[0].qty.unwrap()), list.items[0].unit_id), (700.0, Some(i.g)));
}

#[tokio::test]
async fn merge_1_cup_milk_plus_250_ml_milk_is_2_057_cups() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    db.add_item(l, item(Some(1.0), Some(i.cup), i.milk)).await.unwrap();
    let r = recipe(&db, "Latte", vec![ing(Some((250.0, 250.0)), Some(i.ml), i.milk)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!((r3(list.items[0].qty.unwrap()), list.items[0].unit_id), (2.057, Some(i.cup)));
}

#[tokio::test]
async fn merge_2_eggs_plus_3_eggs_is_5() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    db.add_item(l, item(Some(2.0), None, i.egg)).await.unwrap();
    let r = recipe(&db, "Omelette", vec![ing(Some((3.0, 3.0)), None, i.egg)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!((list.items.len(), list.items[0].qty), (1, Some(5.0)));
}

#[tokio::test]
async fn ranges_add_min_to_min_and_max_to_max() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    db.add_item(l, ItemInput { qty_max: Some(2.0), ..item(Some(1.0), None, i.egg) }).await.unwrap();
    let r = recipe(&db, "Omelette", vec![ing(Some((2.0, 3.0)), None, i.egg)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!((list.items.len(), list.items[0].qty, list.items[0].qty_max), (1, Some(3.0), Some(5.0)));
}

#[tokio::test]
async fn a_line_with_no_amount_never_merges() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    db.add_item(l, item(Some(1.0), Some(i.tsp), i.salt)).await.unwrap();
    let r = recipe(&db, "Stew", vec![ing(None, None, i.salt)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!(list.items.len(), 2);
    // and a second to-taste line does not fold into the first
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!(list.items.len(), 3);
}

#[tokio::test]
async fn mass_and_volume_do_not_merge_nor_do_different_foods_or_units_of_count() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    db.add_item(l, item(Some(100.0), Some(i.g), i.flour)).await.unwrap();
    let r = recipe(&db, "X", vec![ing(Some((1.0, 1.0)), Some(i.cup), i.flour), ing(Some((50.0, 50.0)), Some(i.g), i.milk), ing(Some((2.0, 2.0)), None, i.flour)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!(list.items.len(), 4);
}

#[tokio::test]
async fn a_checked_item_is_not_merged_into() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    let it = db.add_item(l, item(Some(200.0), Some(i.g), i.flour)).await.unwrap();
    db.set_item_checked(it.id, true).await.unwrap();
    let r = recipe(&db, "Bread", vec![ing(Some((100.0, 100.0)), Some(i.g), i.flour)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!(list.items.iter().map(|x| x.qty).collect::<Vec<_>>(), vec![Some(200.0), Some(100.0)]);
}

#[tokio::test]
async fn lines_of_one_recipe_merge_with_each_other() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let l = db.create_list("L").await.unwrap().id;
    let r = recipe(&db, "X", vec![ing(Some((100.0, 100.0)), Some(i.g), i.flour), ing(Some((1.0, 1.0)), Some(i.kg), i.flour)]).await;
    let list = db.add_recipe_to_list(l, r, 1.0).await.unwrap();
    assert_eq!((list.items.len(), list.items[0].qty), (1, Some(1100.0)));
}

#[tokio::test]
async fn batch_uses_actual_amounts_substitutes_and_added_lines() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let oat = db.add_food("oat milk").await.unwrap();
    let r = recipe(&db, "Pancakes", vec![ing(Some((250.0, 250.0)), Some(i.g), i.flour), ing(Some((300.0, 300.0)), Some(i.ml), i.milk)]).await;
    let b = db.start_batch(r, 2.0).await.unwrap();
    let (flour_line, milk_line) = (b.lines[0].id, b.lines[1].id);
    let line = |id, qty, unit, sub| BatchLineInput { id: Some(id), qty_min: Some(qty), qty_max: Some(qty), unit_id: Some(unit), substitute_food_id: sub, ..Default::default() };
    db.update_batch(
        b.batch.id,
        mealier::BatchInput {
            scale: 2.0,
            lines: vec![
                line(flour_line, 480.0, i.g, None),
                line(milk_line, 500.0, i.ml, Some(oat)),
                BatchLineInput { qty_min: Some(2.0), qty_max: Some(2.0), food_id: Some(i.egg), ..Default::default() },
            ],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let l = db.create_list("L").await.unwrap().id;
    let list = db.add_batch_to_list(l, b.batch.id).await.unwrap();
    let got: Vec<_> = list.items.iter().map(|x| (x.qty, x.unit_id, x.food_id, x.recipe_id)).collect();
    assert_eq!(got, vec![(Some(480.0), Some(i.g), Some(i.flour), Some(r)), (Some(500.0), Some(i.ml), Some(oat), Some(r)), (Some(2.0), None, Some(i.egg), Some(r))]);
}

#[tokio::test]
async fn batch_lines_merge_into_existing_items_and_missing_batch_or_list_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    let i = setup(&db).await;
    let r = recipe(&db, "Bread", vec![ing(Some((250.0, 250.0)), Some(i.g), i.flour)]).await;
    let b = db.start_batch(r, 2.0).await.unwrap().batch.id;
    let l = db.create_list("L").await.unwrap().id;
    db.add_item(l, item(Some(1.0), Some(i.kg), i.flour)).await.unwrap();
    let list = db.add_batch_to_list(l, b).await.unwrap();
    assert_eq!((list.items.len(), list.items[0].qty, list.items[0].unit_id), (1, Some(1.5), Some(i.kg)));
    assert!(matches!(db.add_batch_to_list(l, 99).await, Err(Error::NotFound(_))));
    assert!(matches!(db.add_batch_to_list(99, b).await, Err(Error::NotFound(_))));
}
