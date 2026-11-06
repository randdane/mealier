//! Build-order step 15, part 1: the notifier events beyond `batch_logged` and `import_failed`
//! (recipes, meal plan, shopping lists, cookbooks), and the optional per-group filter.
mod common;
use std::time::Duration;

use axum::http::StatusCode;
use mealier::{CookbookInput, Db, EntryInput, Error, ItemInput, Notifier, RecipeInput, Scope, WebhookInput};
use serde_json::{Value, json};

const ALL: [&str; 12] = [
    "import_failed",
    "batch_logged",
    "recipe_created",
    "recipe_updated",
    "recipe_deleted",
    "mealplan_entry_created",
    "shopping_list_created",
    "shopping_list_updated",
    "shopping_list_deleted",
    "cookbook_created",
    "cookbook_updated",
    "cookbook_deleted",
];

fn db_with(base: Option<&str>) -> impl std::future::Future<Output = Db> {
    let base = base.map(String::from);
    async move { Db::open_memory().await.unwrap().with_notifier(Notifier::new(Duration::from_millis(500), base)) }
}

fn input(url: &str, kind: &str, events: &[&str], group_id: Option<i64>) -> WebhookInput {
    WebhookInput {
        name: "t".into(),
        kind: kind.into(),
        url: url.into(),
        events: events.iter().map(|e| e.to_string()).collect(),
        ntfy_tags: String::new(),
        enabled: true,
        group_id,
        household_id: None,
        time_of_day: None,
    }
}

async fn subscribe(db: &Db, url: &str, events: &[&str]) {
    db.create_webhook(input(&format!("{url}/in"), "json", events, None)).await.unwrap();
}

/// The bodies received, parsed, in arrival order.
async fn bodies(h: &std::sync::Arc<std::sync::Mutex<Vec<common::Hit>>>, n: usize) -> Vec<Value> {
    common::hits(h, n).await.iter().map(|g| serde_json::from_str(&g.body).unwrap()).collect()
}

fn of(all: &[Value], event: &str) -> Value {
    all.iter().find(|v| v["event"] == event).unwrap_or_else(|| panic!("no {event} in {all:?}"))["data"].clone()
}

#[tokio::test]
async fn recipe_created_updated_and_archived_are_sent_with_literal_data() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(Some("https://meals.test/")).await;
    subscribe(&db, &url, &["recipe_created", "recipe_updated", "recipe_deleted"]).await;
    let sc = common::scope(&db).await;

    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    db.update_recipe(&sc, r.id, RecipeInput { name: "Better soup".into(), ..Default::default() }).await.unwrap();
    db.archive_recipe(&sc, r.id).await.unwrap();

    let got = bodies(&h, 3).await;
    assert_eq!(got.len(), 3, "{got:?}");
    let want = |name: &str| json!({"recipe_id": r.id, "name": name, "group_id": sc.group_id, "url": format!("https://meals.test/recipes/{}", r.id)});
    assert_eq!(of(&got, "recipe_created"), want("Soup"));
    assert_eq!(of(&got, "recipe_updated"), want("Better soup"));
    assert_eq!(of(&got, "recipe_deleted"), want("Better soup"));
}

#[tokio::test]
async fn a_meal_plan_entry_is_announced_with_its_household_and_slot() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(Some("https://meals.test")).await;
    subscribe(&db, &url, &["mealplan_entry_created"]).await;
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();

    let by_recipe = db.add_entry(&sc, EntryInput { date: "2026-12-01".into(), slot: "dinner".into(), recipe_id: Some(r.id), ..Default::default() }).await.unwrap();
    let by_title = db.add_entry(&sc, EntryInput { date: "2026-12-02".into(), slot: "lunch".into(), title: "Out".into(), ..Default::default() }).await.unwrap();

    let got = bodies(&h, 2).await;
    assert_eq!(got.len(), 2, "{got:?}");
    let a = got.iter().find(|v| v["data"]["mealplan_entry_id"] == by_recipe.id).unwrap();
    assert_eq!(
        a["data"],
        json!({"mealplan_entry_id": by_recipe.id, "name": "Soup", "group_id": sc.group_id, "household_id": sc.household_id, "date": "2026-12-01", "slot": "dinner", "recipe_id": r.id, "url": "https://meals.test/plan"})
    );
    let b = got.iter().find(|v| v["data"]["mealplan_entry_id"] == by_title.id).unwrap();
    assert_eq!((b["data"]["name"].as_str(), b["data"]["recipe_id"].clone()), (Some("Out"), Value::Null));
}

#[tokio::test]
async fn shopping_list_created_updated_and_deleted() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(Some("https://meals.test")).await;
    subscribe(&db, &url, &["shopping_list_created", "shopping_list_updated", "shopping_list_deleted"]).await;
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();

    let list = db.create_list(&sc, "Weekly").await.unwrap();
    let item = db.add_item(&sc, list.id, ItemInput { note: "milk".into(), ..Default::default() }).await.unwrap();
    db.update_item(&sc, item.id, ItemInput { note: "oat milk".into(), ..Default::default() }).await.unwrap();
    db.set_item_checked(&sc, item.id, true).await.unwrap();
    db.add_recipe_to_list(&sc, list.id, r.id, 1.0).await.unwrap();
    db.delete_item(&sc, item.id).await.unwrap();
    db.delete_list(&sc, list.id).await.unwrap();

    let got = bodies(&h, 7).await;
    assert_eq!(got.iter().filter(|v| v["event"] == "shopping_list_created").count(), 1, "{got:?}");
    assert_eq!(got.iter().filter(|v| v["event"] == "shopping_list_updated").count(), 5, "add, update, check, add recipe, delete item: {got:?}");
    let base = |url: &str| json!({"shopping_list_id": list.id, "name": "Weekly", "group_id": sc.group_id, "household_id": sc.household_id, "url": url});
    assert_eq!(of(&got, "shopping_list_created"), base(&format!("https://meals.test/lists/{}", list.id)));
    assert_eq!(of(&got, "shopping_list_deleted"), base("https://meals.test/lists"));
    assert!(matches!(db.shopping_list(&sc, list.id).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn cookbook_created_updated_and_deleted() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(Some("https://meals.test")).await;
    subscribe(&db, &url, &["cookbook_created", "cookbook_updated", "cookbook_deleted"]).await;
    let sc = common::scope(&db).await;

    let c = db.add_cookbook(&sc, CookbookInput { name: "Soups".into(), ..Default::default() }).await.unwrap();
    db.update_cookbook(&sc, c.id, CookbookInput { name: "Stews".into(), ..Default::default() }).await.unwrap();
    db.delete_cookbook(&sc, c.id).await.unwrap();

    let got = bodies(&h, 3).await;
    assert_eq!(got.len(), 3, "{got:?}");
    let want = |name: &str, url: String| json!({"cookbook_id": c.id, "name": name, "group_id": sc.group_id, "url": url});
    assert_eq!(of(&got, "cookbook_created"), want("Soups", format!("https://meals.test/cookbooks/{}", c.id)));
    assert_eq!(of(&got, "cookbook_updated"), want("Stews", format!("https://meals.test/cookbooks/{}", c.id)));
    assert_eq!(of(&got, "cookbook_deleted"), want("Stews", "https://meals.test/cookbooks".into()));
}

#[tokio::test]
async fn nothing_is_sent_when_the_change_failed() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    subscribe(&db, &url, &ALL).await;
    let sc = common::scope(&db).await;
    let c = db.add_cookbook(&sc, CookbookInput { name: "Soups".into(), ..Default::default() }).await.unwrap();
    assert_eq!(bodies(&h, 1).await.len(), 1);

    assert!(db.add_cookbook(&sc, CookbookInput { name: "Soups".into(), ..Default::default() }).await.is_err(), "duplicate name");
    assert!(db.delete_cookbook(&sc, c.id + 99).await.is_err());
    assert!(db.archive_recipe(&sc, 99).await.is_err());
    assert!(db.create_list(&sc, " ").await.is_err());
    assert!(db.delete_list(&sc, 99).await.is_err());
    assert!(db.add_entry(&sc, EntryInput { date: "nope".into(), slot: "dinner".into(), title: "x".into(), ..Default::default() }).await.is_err());
    assert_eq!(bodies(&h, 2).await.len(), 1, "only the first cookbook");
}

#[tokio::test]
async fn ntfy_gets_a_title_the_name_and_a_click_link() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(Some("https://meals.test")).await;
    db.create_webhook(input(&format!("{url}/topic"), "ntfy", &["recipe_created", "shopping_list_deleted"], None)).await.unwrap();
    let sc = common::scope(&db).await;
    let r = db.create_recipe(&sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let l = db.create_list(&sc, "Weekly").await.unwrap();
    db.delete_list(&sc, l.id).await.unwrap();

    let got = common::hits(&h, 2).await;
    let made = got.iter().find(|g| g.headers["title"] == "Recipe created").expect("recipe_created");
    assert_eq!((made.body.as_str(), made.headers["click"].to_str().unwrap()), ("Soup", format!("https://meals.test/recipes/{}", r.id).as_str()));
    let gone = got.iter().find(|g| g.headers["title"] == "Shopping list deleted").expect("shopping_list_deleted");
    assert_eq!((gone.body.as_str(), gone.headers["click"].to_str().unwrap()), ("Weekly", "https://meals.test/lists"));
}

// ---- the group filter ----

#[tokio::test]
async fn a_group_filtered_webhook_gets_only_that_groups_events() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    let t = common::tenants(&db).await;
    db.create_webhook(input(&format!("{url}/one"), "json", &["recipe_created", "shopping_list_created"], Some(t.a.group_id))).await.unwrap();
    db.create_webhook(input(&format!("{url}/all"), "json", &["recipe_created", "shopping_list_created"], None)).await.unwrap();

    db.create_recipe(&t.c, RecipeInput { name: "Theirs".into(), ..Default::default() }).await.unwrap();
    db.create_list(&t.c, "Theirs").await.unwrap();
    db.create_recipe(&t.a, RecipeInput { name: "Ours".into(), ..Default::default() }).await.unwrap();
    db.create_list(&t.b, "Ours too").await.unwrap(); // another household of the same group

    let got = common::hits(&h, 6).await;
    let names = |path: &str| {
        let mut n: Vec<String> = got.iter().filter(|g| g.path == path).map(|g| serde_json::from_str::<Value>(&g.body).unwrap()["data"]["name"].as_str().unwrap().to_string()).collect();
        n.sort();
        n
    };
    assert_eq!(names("/one"), ["Ours", "Ours too"]);
    assert_eq!(names("/all"), ["Ours", "Ours too", "Theirs", "Theirs"]);
}

#[tokio::test]
async fn the_group_filter_covers_batch_logged_and_import_failed_too() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    let t = common::tenants(&db).await;
    db.create_webhook(input(&format!("{url}/one"), "json", &["batch_logged", "import_failed"], Some(t.a.group_id))).await.unwrap();

    for sc in [&t.c, &t.a] {
        let r = db.create_recipe(sc, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
        let b = db.start_batch(sc, r.id, 1.0).await.unwrap();
        db.finish_batch(sc, b.batch.id).await.unwrap();
        let _ = mealier::ImportService::default().import(&db, sc, &tempfile::tempdir().unwrap().keep(), "http://127.0.0.1:1/r").await;
    }
    let got = bodies(&h, 4).await;
    let mut seen: Vec<&str> = got.iter().map(|v| v["event"].as_str().unwrap()).collect();
    seen.sort();
    assert_eq!(seen, ["batch_logged", "import_failed"], "{got:?}");
    assert_eq!(of(&got, "batch_logged")["recipe_name"], "Soup");
}

#[tokio::test]
async fn the_test_button_ignores_the_group_filter() {
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    let t = common::tenants(&db).await;
    let w = db.create_webhook(input(&format!("{url}/one"), "json", &["recipe_created"], Some(t.a.group_id))).await.unwrap();
    assert_eq!(db.send_test(w.id).await.unwrap(), 200);
    assert_eq!(common::hits(&h, 1).await.len(), 1);
}

#[tokio::test]
async fn validation_takes_the_new_events_and_a_real_group() {
    let db = Db::open_memory().await.unwrap();
    let g = common::scope(&db).await.group_id;
    let w = db.create_webhook(input("http://127.0.0.1:9/x", "json", &ALL, Some(g))).await.unwrap();
    assert_eq!((w.events.len(), w.group_id), (12, Some(g)));
    assert_eq!(db.get_webhook(w.id).await.unwrap().group_id, Some(g), "stored");
    for bad in [&["recipe_archived"][..], &["mealplan_entry_deleted"], &["tag_created"]] {
        assert!(matches!(db.create_webhook(input("http://127.0.0.1:9/x", "json", bad, None)).await, Err(Error::Validation(_))), "{bad:?}");
    }
    assert!(matches!(db.create_webhook(input("http://127.0.0.1:9/x", "json", &["recipe_created"], Some(g + 99))).await, Err(Error::Validation(_))));
    // the filter can be changed and cleared
    let u = db.update_webhook(w.id, input("http://127.0.0.1:9/x", "json", &["recipe_created"], None)).await.unwrap();
    assert_eq!(u.group_id, None);
}

// ---- through the router ----

#[tokio::test]
async fn deleting_a_shopping_list_over_http_is_204_then_404_and_sends_the_event() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let (url, h) = common::receiver(200).await;
    let db = db_with(None).await;
    subscribe(&db, &url, &["shopping_list_deleted"]).await;
    common::sign_in(&db).await;
    let admin = db.users().await.unwrap().into_iter().find(|u| u.username == "suite-admin").unwrap();
    let list = db.create_list(&Scope::from(&admin), "Weekly").await.unwrap();
    let app = mealier::api::router(mealier::api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() });
    let del = |id: i64| Request::builder().method("DELETE").uri(format!("/api/shopping-lists/{id}")).header("authorization", common::BEARER).body(Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(del(list.id)).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(app.clone().oneshot(del(list.id)).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_eq!(bodies(&h, 1).await.len(), 1);
}
