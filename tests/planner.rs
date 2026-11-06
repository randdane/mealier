use mealier::{Db, EntryInput, Error, RecipeInput};

async fn recipe(db: &Db, name: &str) -> i64 {
    db.create_recipe(RecipeInput { name: name.into(), ..Default::default() }).await.unwrap().id
}

fn entry(date: &str, slot: &str, recipe_id: Option<i64>) -> EntryInput {
    EntryInput { date: date.into(), slot: slot.into(), recipe_id, ..Default::default() }
}

#[tokio::test]
async fn add_entry_with_a_recipe() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    let e = db.add_entry(entry("2026-12-01", "dinner", Some(r))).await.unwrap();
    assert_eq!((e.date.as_str(), e.slot.as_str(), e.recipe_id, e.batch_id), ("2026-12-01", "dinner", Some(r), None));
}

#[tokio::test]
async fn add_entry_with_title_and_text_only() {
    let db = Db::open_memory().await.unwrap();
    let e = db.add_entry(EntryInput { title: "Takeout".into(), text: "pizza".into(), ..entry("2026-12-01", "lunch", None) }).await.unwrap();
    assert_eq!((e.recipe_id, e.title.as_str(), e.text.as_str()), (None, "Takeout", "pizza"));
}

#[tokio::test]
async fn add_entry_rejects_unknown_slot() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    assert!(matches!(db.add_entry(entry("2026-12-01", "brunch", Some(r))).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn add_entry_rejects_no_recipe_and_no_title() {
    let db = Db::open_memory().await.unwrap();
    assert!(matches!(db.add_entry(entry("2026-12-01", "dinner", None)).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn add_entry_rejects_a_bad_date() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    assert!(matches!(db.add_entry(entry("2026-13-45", "dinner", Some(r))).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn add_entry_for_a_missing_recipe_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    assert!(matches!(db.add_entry(entry("2026-12-01", "dinner", Some(99))).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn add_entry_takes_a_planned_batch_of_the_same_recipe() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    let b = db.start_batch(r, 2.0).await.unwrap().batch.id;
    let e = db.add_entry(EntryInput { batch_id: Some(b), ..entry("2026-12-01", "dinner", Some(r)) }).await.unwrap();
    assert_eq!(e.batch_id, Some(b));
}

#[tokio::test]
async fn add_entry_rejects_a_batch_of_another_recipe() {
    let db = Db::open_memory().await.unwrap();
    let (a, other) = (recipe(&db, "Soup").await, recipe(&db, "Stew").await);
    let b = db.start_batch(other, 1.0).await.unwrap().batch.id;
    assert!(matches!(db.add_entry(EntryInput { batch_id: Some(b), ..entry("2026-12-01", "dinner", Some(a)) }).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn add_entry_rejects_a_batch_that_is_done() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    let b = db.start_batch(r, 1.0).await.unwrap().batch.id;
    db.finish_batch(b).await.unwrap();
    assert!(matches!(db.add_entry(EntryInput { batch_id: Some(b), ..entry("2026-12-01", "dinner", Some(r)) }).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn add_entry_rejects_a_batch_without_its_recipe() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    let b = db.start_batch(r, 1.0).await.unwrap().batch.id;
    let input = EntryInput { batch_id: Some(b), title: "x".into(), ..entry("2026-12-01", "dinner", None) };
    assert!(matches!(db.add_entry(input).await, Err(Error::Validation(_))));
}

#[tokio::test]
async fn meal_plan_is_inclusive_and_ordered_by_date_then_slot() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    for (d, s) in [("2026-12-02", "breakfast"), ("2026-12-01", "dessert"), ("2026-12-01", "breakfast"), ("2026-12-01", "side"), ("2026-12-01", "dinner"), ("2026-12-03", "lunch"), ("2026-11-30", "lunch")] {
        db.add_entry(entry(d, s, Some(r))).await.unwrap();
    }
    let got: Vec<_> = db.meal_plan("2026-12-01", "2026-12-02").await.unwrap().into_iter().map(|e| (e.date, e.slot)).collect();
    let want: Vec<(String, String)> = [("2026-12-01", "breakfast"), ("2026-12-01", "dinner"), ("2026-12-01", "side"), ("2026-12-01", "dessert"), ("2026-12-02", "breakfast")]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn update_entry_changes_it_and_validates() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    let e = db.add_entry(entry("2026-12-01", "dinner", Some(r))).await.unwrap();
    let u = db.update_entry(e.id, entry("2026-12-05", "lunch", Some(r))).await.unwrap();
    assert_eq!((u.id, u.date.as_str(), u.slot.as_str()), (e.id, "2026-12-05", "lunch"));
    assert!(matches!(db.update_entry(e.id, entry("2026-12-05", "nope", Some(r))).await, Err(Error::Validation(_))));
    assert!(matches!(db.update_entry(99, entry("2026-12-05", "lunch", Some(r))).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn delete_entry_removes_it() {
    let db = Db::open_memory().await.unwrap();
    let r = recipe(&db, "Soup").await;
    let e = db.add_entry(entry("2026-12-01", "dinner", Some(r))).await.unwrap();
    db.delete_entry(e.id).await.unwrap();
    assert!(db.meal_plan("2026-12-01", "2026-12-01").await.unwrap().is_empty());
    assert!(matches!(db.delete_entry(e.id).await, Err(Error::NotFound(_))));
}
