//! One quantity-range rule for recipes, batches and shopping lists. The ordinary cases (a lone minimum
//! fills the maximum, a maximum alone, negatives, reversed ranges) are covered where each is used;
//! these are the values no JSON body can carry, which a client of the Rust API can.
mod common;
use mealier::{BatchLineInput, Db, Error, IngredientInput, ItemInput, RecipeInput};

const BAD: [(Option<f64>, Option<f64>); 8] = [
    (Some(f64::NAN), None),
    (Some(f64::INFINITY), None),
    (Some(1.0), Some(f64::INFINITY)),
    (Some(1.0), Some(f64::NAN)),
    (Some(f64::NAN), Some(f64::NAN)),
    (Some(f64::NEG_INFINITY), Some(1.0)),
    (None, Some(f64::NAN)),
    (None, Some(f64::INFINITY)),
];

fn validation<T: std::fmt::Debug>(r: Result<T, Error>, what: &str) {
    assert!(matches!(r, Err(Error::Validation(_))), "{what}: {r:?}");
}

#[tokio::test]
async fn recipes_refuse_non_finite_quantities() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    for (min, max) in BAD {
        let line = IngredientInput { qty_min: min, qty_max: max, original_text: "x".into(), ..Default::default() };
        validation(db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ingredients: vec![line], ..Default::default() }).await, &format!("{min:?} {max:?}"));
    }
}

#[tokio::test]
async fn batch_lines_refuse_non_finite_quantities() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let b = db.start_batch(&sc, r.id, 1.0).await.unwrap().batch.id;
    for (min, max) in BAD {
        validation(db.add_batch_line(&sc, b, BatchLineInput { qty_min: min, qty_max: max, ..Default::default() }).await, &format!("{min:?} {max:?}"));
    }
}

#[tokio::test]
async fn shopping_items_refuse_non_finite_quantities() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let list = db.create_list(&sc, "Weekly").await.unwrap().id;
    for (qty, qty_max) in BAD {
        validation(db.add_item(&sc, list, ItemInput { qty, qty_max, note: "salt".into(), ..Default::default() }).await, &format!("{qty:?} {qty_max:?}"));
    }
}
