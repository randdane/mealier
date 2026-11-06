mod common;
use mealier::{Db, Error, ItemInput, LabelInput};

fn l(name: &str, color: &str) -> LabelInput {
    LabelInput { name: name.into(), color: color.into() }
}

fn item(food: Option<i64>, note: &str) -> ItemInput {
    ItemInput { food_id: food, note: note.into(), ..Default::default() }
}

/// Section titles of a list, `-` for the unlabelled one.
fn titles(list: &mealier::ShoppingList) -> Vec<String> {
    list.sections.iter().map(|s| s.label.as_ref().map_or("-".into(), |l| l.name.clone())).collect()
}

#[tokio::test]
async fn label_crud_validates_colour_and_keeps_names_unique_per_group() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let dairy = db.add_label(&t.a, l("Dairy", "#AABBCC")).await.unwrap();
    assert_eq!((dairy.name.as_str(), dairy.color.as_str()), ("Dairy", "#aabbcc"));
    db.add_label(&t.a, l("Baking", "#000000")).await.unwrap();
    assert_eq!(db.labels(&t.a).await.unwrap().iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["Baking", "Dairy"]);

    for bad in ["red", "#12345", "#12345g", "123456", ""] {
        assert!(matches!(db.add_label(&t.a, l("X", bad)).await, Err(Error::Validation(_))), "{bad}");
    }
    assert!(matches!(db.add_label(&t.a, l("  ", "#000000")).await, Err(Error::Validation(_))));
    assert!(matches!(db.add_label(&t.a, l("Dairy", "#111111")).await, Err(Error::Validation(_))), "duplicate name");
    // another group may reuse it; the same group's other household sees the same labels
    db.add_label(&t.c, l("Dairy", "#111111")).await.unwrap();
    assert_eq!(db.labels(&t.b).await.unwrap().len(), 2);

    let got = db.update_label(&t.a, dairy.id, l("Milk", "#ff0000")).await.unwrap();
    assert_eq!((got.name.as_str(), got.color.as_str()), ("Milk", "#ff0000"));
    assert!(matches!(db.update_label(&t.a, dairy.id, l("Baking", "#ff0000")).await, Err(Error::Validation(_))));
    assert!(matches!(db.update_label(&t.c, dairy.id, l("Z", "#ff0000")).await, Err(Error::NotFound(_))));
    assert!(matches!(db.delete_label(&t.c, dairy.id).await, Err(Error::NotFound(_))));
    db.delete_label(&t.a, dairy.id).await.unwrap();
    assert_eq!(db.labels(&t.a).await.unwrap().len(), 1);
}

#[tokio::test]
async fn items_group_by_their_label_else_their_foods_else_none() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let (veg, dairy) = (db.add_label(&sc, l("Vegetables", "#00aa00")).await.unwrap(), db.add_label(&sc, l("Dairy", "#0000aa")).await.unwrap());
    let (onion, milk, rock) = (db.add_food(&sc, "onion").await.unwrap(), db.add_food(&sc, "milk").await.unwrap(), db.add_food(&sc, "rock").await.unwrap());
    db.set_food_label(&sc, onion, Some(veg.id)).await.unwrap();
    db.set_food_label(&sc, milk, Some(dairy.id)).await.unwrap();
    assert_eq!(db.food_label(&sc, onion).await.unwrap(), Some(veg.clone()));
    assert_eq!(db.food_label(&sc, rock).await.unwrap(), None);

    let list = db.create_list(&sc, "Weekly").await.unwrap();
    let a = db.add_item(&sc, list.id, item(Some(onion), "")).await.unwrap();
    let b = db.add_item(&sc, list.id, item(Some(milk), "")).await.unwrap();
    // milk, but this item says vegetables: its own label wins
    let c = db.add_item(&sc, list.id, ItemInput { label_id: Some(veg.id), ..item(Some(milk), "") }).await.unwrap();
    let d = db.add_item(&sc, list.id, item(Some(rock), "")).await.unwrap();
    let e = db.add_item(&sc, list.id, item(None, "string")).await.unwrap();

    let got = db.shopping_list(&sc, list.id).await.unwrap();
    // new lists start in name order: Dairy, Vegetables; no label last
    assert_eq!(titles(&got), vec!["Dairy", "Vegetables", "-"]);
    let ids = |i: usize| got.sections[i].items.iter().map(|x| x.id).collect::<Vec<_>>();
    assert_eq!((ids(0), ids(1), ids(2)), (vec![b.id], vec![a.id, c.id], vec![d.id, e.id]));
    assert_eq!(got.sections[1].label.as_ref().unwrap().color, "#00aa00");
    assert_eq!(got.items.len(), 5, "the flat list stays");

    // clearing the food's label moves its items to the end; deleting a label does the same
    db.set_food_label(&sc, onion, None).await.unwrap();
    db.delete_label(&sc, dairy.id).await.unwrap();
    let got = db.shopping_list(&sc, list.id).await.unwrap();
    assert_eq!(titles(&got), vec!["Vegetables", "-"]);
    assert_eq!(got.sections[1].items.len(), 4);
}

#[tokio::test]
async fn section_order_follows_the_lists_positions_then_name() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let mut ids = vec![];
    let mut foods = vec![];
    for n in ["A", "B", "C"] {
        let label = db.add_label(&sc, l(n, "#123456")).await.unwrap();
        let food = db.add_food(&sc, &format!("food {n}")).await.unwrap();
        db.set_food_label(&sc, food, Some(label.id)).await.unwrap();
        ids.push(label.id);
        foods.push(food);
    }
    let list = db.create_list(&sc, "L").await.unwrap();
    for f in &foods {
        db.add_item(&sc, list.id, item(Some(*f), "")).await.unwrap();
    }
    assert_eq!(titles(&db.shopping_list(&sc, list.id).await.unwrap()), vec!["A", "B", "C"]);

    let got = db.set_section_order(&sc, list.id, &[ids[2], ids[0]]).await.unwrap();
    assert_eq!(titles(&got), vec!["C", "A", "B"], "unnamed labels follow by name");
    assert_eq!(titles(&db.shopping_list(&sc, list.id).await.unwrap()), vec!["C", "A", "B"]);

    // another list is untouched; a label made later has no place, so it sorts by name after the placed ones
    let other = db.create_list(&sc, "M").await.unwrap();
    let z = db.add_label(&sc, l("0 late", "#123456")).await.unwrap();
    let zf = db.add_food(&sc, "zf").await.unwrap();
    db.set_food_label(&sc, zf, Some(z.id)).await.unwrap();
    db.add_item(&sc, list.id, item(Some(zf), "")).await.unwrap();
    assert_eq!(titles(&db.shopping_list(&sc, list.id).await.unwrap()), vec!["C", "A", "0 late", "B"]);
    db.add_item(&sc, other.id, item(Some(foods[1]), "")).await.unwrap();
    assert_eq!(titles(&db.shopping_list(&sc, other.id).await.unwrap()), vec!["B"]);

    // move up / down among the sections that have items; at the ends nothing happens
    db.move_section(&sc, list.id, ids[0], true).await.unwrap();
    assert_eq!(titles(&db.shopping_list(&sc, list.id).await.unwrap()), vec!["A", "C", "0 late", "B"]);
    db.move_section(&sc, list.id, ids[0], true).await.unwrap();
    db.move_section(&sc, list.id, ids[1], false).await.unwrap();
    assert_eq!(titles(&db.shopping_list(&sc, list.id).await.unwrap()), vec!["A", "C", "0 late", "B"]);
    db.move_section(&sc, list.id, z.id, false).await.unwrap();
    assert_eq!(titles(&db.shopping_list(&sc, list.id).await.unwrap()), vec!["A", "C", "B", "0 late"]);

    assert!(matches!(db.set_section_order(&sc, list.id, &[ids[0], ids[0]]).await, Err(Error::Validation(_))), "duplicate");
}

#[tokio::test]
async fn labels_are_group_scoped_and_other_tenants_get_not_found() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let mine = db.add_label(&t.a, l("Mine", "#123456")).await.unwrap();
    let theirs = db.add_label(&t.c, l("Theirs", "#123456")).await.unwrap();
    let food = db.add_food(&t.a, "f").await.unwrap();
    let list = db.create_list(&t.a, "L").await.unwrap();
    let nf = |r: Result<(), Error>| assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    nf(db.set_food_label(&t.c, food, Some(theirs.id)).await);
    nf(db.set_food_label(&t.a, food, Some(theirs.id)).await);
    nf(db.set_food_label(&t.a, food, Some(mine.id)).await.and(db.set_food_label(&t.c, food, None).await));
    assert!(matches!(db.set_section_order(&t.b, list.id, &[mine.id]).await, Err(Error::NotFound(_))), "another household's list");
    assert!(matches!(db.set_section_order(&t.a, list.id, &[theirs.id]).await, Err(Error::NotFound(_))));
    nf(db.move_section(&t.b, list.id, mine.id, true).await);
    let bad = ItemInput { label_id: Some(theirs.id), note: "x".into(), ..Default::default() };
    assert!(matches!(db.add_item(&t.a, list.id, bad).await, Err(Error::NotFound(_))));
}
