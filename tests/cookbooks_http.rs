//! Cookbooks, meal-plan rules and suggestions over HTTP: the JSON API and the pages.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, RecipeInput, RuleInput, api};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn site() -> (Router, Db) {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    (api::router(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }), db)
}

async fn go(app: &Router, method: &str, uri: &str, json: Option<Value>, form: Option<&str>) -> (StatusCode, String, String) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", common::BEARER);
    let body = match (json, form) {
        (Some(v), _) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        (_, Some(f)) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(f.to_string())
        }
        _ => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, loc) = (resp.status(), resp.headers().get("location").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default());
    (status, loc, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

async fn api_json(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (s, _, b) = go(app, method, uri, body, None).await;
    (s, serde_json::from_str(&b).unwrap_or(Value::Null))
}

async fn recipe(db: &Db, name: &str, tags: &[&str]) -> i64 {
    let sc = common::scope(db).await;
    db.create_recipe(&sc, RecipeInput { name: name.into(), tags: tags.iter().map(|t| t.to_string()).collect(), ..Default::default() }).await.unwrap().id
}

fn recipe_names(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect()
}

// ---------- cookbooks: API ----------

#[tokio::test]
async fn cookbook_api_creates_lists_updates_and_deletes() {
    let (app, db) = site().await;
    recipe(&db, "Pancakes", &["quick"]).await;
    recipe(&db, "Stew", &["slow"]).await;
    let (s, b) = api_json(&app, "POST", "/api/cookbooks", Some(json!({"name": "Quick", "description": "fast", "public": true, "query_filter": r#"tags.name IN ["quick"]"#}))).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!((b["name"].as_str(), b["slug"].as_str(), b["position"].as_i64(), b["public"].as_bool(), b["filter_supported"].as_bool()), (Some("Quick"), Some("quick"), Some(1), Some(true), Some(true)), "{b}");
    let id = b["id"].as_i64().unwrap();

    let (s, b) = api_json(&app, "GET", "/api/cookbooks", None).await;
    assert_eq!((s, b.as_array().unwrap().len()), (StatusCode::OK, 1));
    let (s, b) = api_json(&app, "GET", &format!("/api/cookbooks/{id}"), None).await;
    assert_eq!((s, b["query_filter"].as_str()), (StatusCode::OK, Some(r#"tags.name IN ["quick"]"#)));
    let (s, b) = api_json(&app, "GET", &format!("/api/cookbooks/{id}/recipes"), None).await;
    assert_eq!((s, recipe_names(&b)), (StatusCode::OK, vec!["Pancakes"]), "{b}");

    let (s, b) = api_json(&app, "PUT", &format!("/api/cookbooks/{id}"), Some(json!({"name": "Everything", "query_filter": ""}))).await;
    assert_eq!((s, b["name"].as_str(), b["public"].as_bool()), (StatusCode::OK, Some("Everything"), Some(false)), "{b}");
    let (_, b) = api_json(&app, "GET", &format!("/api/cookbooks/{id}/recipes"), None).await;
    assert_eq!(recipe_names(&b), vec!["Pancakes", "Stew"]);

    let (s, _) = api_json(&app, "DELETE", &format!("/api/cookbooks/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api_json(&app, "GET", &format!("/api/cookbooks/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = api_json(&app, "GET", &format!("/api/cookbooks/{id}/recipes"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cookbook_api_refuses_a_filter_it_cannot_read_naming_the_token() {
    let (app, _) = site().await;
    let (s, b) = api_json(&app, "POST", "/api/cookbooks", Some(json!({"name": "Bad", "query_filter": r#"household_id = "x""#}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(b["error"].as_str().unwrap().contains("household_id"), "{b}");
    let (s, _) = api_json(&app, "POST", "/api/cookbooks", Some(json!({"name": "", "query_filter": ""}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, b) = api_json(&app, "GET", "/api/cookbooks", None).await;
    assert!(b.as_array().unwrap().is_empty());
}

// ---------- cookbooks: pages ----------

#[tokio::test]
async fn the_cookbook_pages_list_create_show_and_delete() {
    let (app, db) = site().await;
    recipe(&db, "Pancakes", &["quick"]).await;
    recipe(&db, "Stew", &["slow"]).await;
    let (s, _, html) = go(&app, "GET", "/cookbooks", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("No cookbooks yet"), "{html}");

    let (s, loc, _) = go(&app, "POST", "/cookbooks", None, Some("name=Quick&description=fast&query_filter=tags.name+IN+%5B%22quick%22%5D")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(loc.starts_with("/cookbooks/"), "{loc}");
    let (s, _, html) = go(&app, "GET", &loc, None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("Quick") && html.contains("Pancakes") && !html.contains("Stew"), "{html}");
    let (_, _, list) = go(&app, "GET", "/cookbooks", None, None).await;
    assert!(list.contains("Quick") && list.contains(&loc), "{list}");

    let (s, _, html) = go(&app, "POST", "/cookbooks", None, Some("name=Bad&query_filter=name+IN+%5B%22a%22%5D")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(html.contains("IN"), "{html}");

    let (s, loc2, _) = go(&app, "POST", &format!("{loc}/delete"), None, Some("")).await;
    assert_eq!((s, loc2.as_str()), (StatusCode::SEE_OTHER, "/cookbooks"));
    let (s, ..) = go(&app, "GET", &loc, None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unsupported_imported_filter_says_so_and_lists_nothing() {
    let (app, db) = site().await;
    recipe(&db, "Pancakes", &["quick"]).await;
    let sc = common::scope(&db).await;
    sqlx::query("INSERT INTO cookbooks (id, group_id, name, slug, query_filter, filter_json) VALUES (70, ?, 'Imported', 'imported', 'household_id = \"x\"', NULL)").bind(sc.group_id).execute(db.pool()).await.unwrap();
    let (s, _, html) = go(&app, "GET", "/cookbooks/70", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("filter not supported") && html.contains("household_id"), "{html}");
    assert!(!html.contains("Pancakes"), "{html}");
}

#[tokio::test]
async fn the_edit_page_builder_writes_a_mealie_filter_and_a_plain_edit_changes_the_rest() {
    let (app, db) = site().await;
    recipe(&db, "Pancakes", &["quick", "breakfast"]).await;
    recipe(&db, "Stew", &["slow"]).await;
    let sc = common::scope(&db).await;
    db.create_recipe(&sc, RecipeInput { name: "Soup".into(), categories: vec!["Mains".into()], tools: vec!["pot".into()], ..Default::default() }).await.unwrap();
    let book = db.add_cookbook(&sc, mealier::CookbookInput { name: "Mixed".into(), ..Default::default() }).await.unwrap();
    let tag = |n: &str| sqlx::query_scalar::<_, i64>("SELECT id FROM tags WHERE name = ?").bind(n.to_string());
    let (quick, breakfast) = (tag("quick").fetch_one(db.pool()).await.unwrap(), tag("breakfast").fetch_one(db.pool()).await.unwrap());

    let (s, _, html) = go(&app, "GET", &format!("/cookbooks/{}/edit", book.id), None, None).await;
    assert_eq!(s, StatusCode::OK);
    for name in ["quick", "breakfast", "slow", "name=\"tags\"", "name=\"categories\"", "name=\"tools\"", "tags_mode", "query_filter"] {
        assert!(html.contains(name), "the edit page should offer {name}: {html}");
    }

    let form = format!("tags={quick}&tags={breakfast}&tags_mode=all");
    let (s, loc, _) = go(&app, "POST", &format!("/cookbooks/{}/builder", book.id), None, Some(&form)).await;
    assert_eq!((s, loc.as_str()), (StatusCode::SEE_OTHER, format!("/cookbooks/{}/edit", book.id).as_str()));
    let saved = db.cookbook(&sc, book.id).await.unwrap();
    assert_eq!(saved.query_filter, format!(r#"tags.id CONTAINS ALL ["{quick}", "{breakfast}"]"#));
    assert!(saved.filter_supported);
    let names = |v: Vec<mealier::RecipeSummary>| v.into_iter().map(|r| r.name).collect::<Vec<_>>();
    assert_eq!(names(db.cookbook_recipes(&sc, book.id).await.unwrap()), ["Pancakes"]);
    // the page shows the string, and the builder keeps the picks
    let (_, _, html) = go(&app, "GET", &format!("/cookbooks/{}/edit", book.id), None, None).await;
    assert!(html.contains("tags.id CONTAINS ALL"), "{html}");
    assert!(html.contains(&format!("value=\"{quick}\" checked")), "the pick stays ticked: {html}");

    // any, not all
    let (s, ..) = go(&app, "POST", &format!("/cookbooks/{}/builder", book.id), None, Some(&format!("tags={quick}&tags_mode=any&tools=999999"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a tool that is not the group's");
    assert!(db.cookbook(&sc, book.id).await.unwrap().query_filter.contains("CONTAINS ALL"), "nothing changed");
    let (s, ..) = go(&app, "POST", &format!("/cookbooks/{}/builder", book.id), None, Some(&format!("tags={quick}&tags=0&tags_mode=any"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a tag that does not exist");
    // nothing ticked clears the filter
    let (s, ..) = go(&app, "POST", &format!("/cookbooks/{}/builder", book.id), None, Some("tags_mode=any")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(db.cookbook(&sc, book.id).await.unwrap().query_filter, "");

    // the plain edit: name, description, position, public, raw filter
    let (s, loc, _) = go(&app, "POST", &format!("/cookbooks/{}/edit", book.id), None, Some("name=Renamed&description=d&position=4&public=on&query_filter=name+LIKE+%22%25stew%25%22")).await;
    assert_eq!((s, loc.as_str()), (StatusCode::SEE_OTHER, format!("/cookbooks/{}", book.id).as_str()));
    let c = db.cookbook(&sc, book.id).await.unwrap();
    assert_eq!((c.name.as_str(), c.description.as_str(), c.position, c.public, c.query_filter.as_str()), ("Renamed", "d", 4, true, "name LIKE \"%stew%\""));
    assert_eq!(names(db.cookbook_recipes(&sc, book.id).await.unwrap()), ["Stew"]);
    let (s, ..) = go(&app, "POST", &format!("/cookbooks/{}/edit", book.id), None, Some("name=Renamed&query_filter=rating+LIKE+1")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(db.cookbook(&sc, book.id).await.unwrap().query_filter, "name LIKE \"%stew%\"");
}

// ---------- rules and suggestions: API ----------

#[tokio::test]
async fn rule_api_creates_lists_updates_and_deletes() {
    let (app, db) = site().await;
    let home = common::household(&db).await;
    let (s, b) = api_json(&app, "POST", "/api/meal-plan-rules", Some(json!({"day": "monday", "slot": "dinner", "query_filter": r#"tags.name IN ["quick"]"#, "household_ids": [home]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!((b["day"].as_str(), b["slot"].as_str(), b["filter_supported"].as_bool(), b["household_ids"].clone()), (Some("monday"), Some("dinner"), Some(true), json!([home])), "{b}");
    let id = b["id"].as_i64().unwrap();
    let (s, b) = api_json(&app, "GET", "/api/meal-plan-rules", None).await;
    assert_eq!((s, b.as_array().unwrap().len()), (StatusCode::OK, 1));
    let (s, b) = api_json(&app, "GET", &format!("/api/meal-plan-rules/{id}"), None).await;
    assert_eq!((s, b["slot"].as_str()), (StatusCode::OK, Some("dinner")));
    let (s, b) = api_json(&app, "PUT", &format!("/api/meal-plan-rules/{id}"), Some(json!({"slot": "lunch"}))).await;
    assert_eq!((s, b["day"].as_str(), b["slot"].as_str(), b["household_ids"].clone()), (StatusCode::OK, Some("unset"), Some("lunch"), json!([])), "{b}");
    for bad in [json!({"day": "someday"}), json!({"slot": "brunch"}), json!({"query_filter": "name IN ["}), json!({"query_filter": r#"tags.id IN ["abc"]"#})] {
        let (s, b) = api_json(&app, "POST", "/api/meal-plan-rules", Some(bad.clone())).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
    }
    let (s, _) = api_json(&app, "POST", "/api/meal-plan-rules", Some(json!({"household_ids": [99999]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = api_json(&app, "DELETE", &format!("/api/meal-plan-rules/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api_json(&app, "DELETE", &format!("/api/meal-plan-rules/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn suggest_api_picks_a_recipe_the_rule_allows_or_null() {
    let (app, db) = site().await;
    let only = recipe(&db, "Quick Soup", &["quick"]).await;
    recipe(&db, "Slow Stew", &["slow"]).await;
    let (s, b) = api_json(&app, "POST", "/api/meal-plan/suggest", Some(json!({"date": "2026-11-02", "slot": "dinner"}))).await;
    assert_eq!((s, b["recipe"].clone()), (StatusCode::OK, Value::Null), "no rule: {b}");
    db.add_rule(&common::scope(&db).await, RuleInput { query_filter: r#"tags.name IN ["quick"]"#.into(), ..Default::default() }).await.unwrap();
    for _ in 0..5 {
        let (s, b) = api_json(&app, "POST", "/api/meal-plan/suggest", Some(json!({"date": "2026-11-02", "slot": "dinner"}))).await;
        assert_eq!((s, b["recipe"]["id"].as_i64(), b["recipe"]["name"].as_str()), (StatusCode::OK, Some(only), Some("Quick Soup")), "{b}");
    }
    for bad in [json!({"date": "2026-11-31x", "slot": "dinner"}), json!({"date": "2026-11-02", "slot": "brunch"}), json!({"slot": "dinner"})] {
        let (s, b) = api_json(&app, "POST", "/api/meal-plan/suggest", Some(bad.clone())).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
    }
}

// ---------- the plan page and the rules page ----------

#[tokio::test]
async fn an_empty_slot_on_the_plan_offers_suggest_and_fills_the_add_form() {
    let (app, db) = site().await;
    let soup = recipe(&db, "Quick Soup", &["quick"]).await;
    recipe(&db, "Slow Stew", &["slow"]).await;
    let sc = common::scope(&db).await;
    db.add_rule(&sc, RuleInput { query_filter: r#"tags.name IN ["quick"]"#.into(), ..Default::default() }).await.unwrap();
    // 2026-11-02 is a Monday; fill its dinner so only the other slots are offered
    db.add_entry(&sc, mealier::EntryInput { date: "2026-11-02".into(), slot: "dinner".into(), recipe_id: Some(soup), ..Default::default() }).await.unwrap();
    let (s, _, html) = go(&app, "GET", "/plan?week=2026-11-02", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("Suggest"), "{html}");
    assert!(html.contains("action=\"/plan/suggest\""), "{html}");

    let (s, loc, _) = go(&app, "GET", "/plan/suggest?date=2026-11-02&slot=lunch", None, None).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(loc, format!("/plan?week=2026-11-02&date=2026-11-02&slot=lunch&recipe={soup}"));
    let (_, _, html) = go(&app, "GET", &loc, None, None).await;
    assert!(html.contains(&format!("<option value=\"{soup}\" selected>")), "the add form has the suggestion selected: {html}");
    assert!(html.contains("<option value=\"lunch\" selected>"), "{html}");
    assert!(html.contains("<details open"), "{html}");

    // nothing to suggest: back to the plan with a note, no recipe
    let (s, loc, _) = go(&app, "GET", "/plan/suggest?date=2026-11-02&slot=lunch", None, None).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    sqlx::query("DELETE FROM meal_plan_rules").execute(db.pool()).await.unwrap();
    let (s, loc2, _) = go(&app, "GET", "/plan/suggest?date=2026-11-02&slot=lunch", None, None).await;
    assert_eq!((s, loc2.as_str()), (StatusCode::SEE_OTHER, "/plan?week=2026-11-02&date=2026-11-02&slot=lunch&none=1"), "{loc}");
    let (_, _, html) = go(&app, "GET", &loc2, None, None).await;
    assert!(html.contains("No rule has a suggestion"), "{html}");
    let (s, ..) = go(&app, "GET", "/plan/suggest?date=nope&slot=lunch", None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_rules_page_creates_edits_and_deletes_rules() {
    let (app, db) = site().await;
    let sc = common::scope(&db).await;
    let home = common::household(&db).await;
    let (s, _, html) = go(&app, "GET", "/settings/rules", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("No rules yet"), "{html}");

    let (s, loc, _) = go(&app, "POST", "/settings/rules", None, Some(&format!("day=monday&slot=dinner&query_filter=name+LIKE+%22%25soup%25%22&households={home}"))).await;
    assert_eq!((s, loc.as_str()), (StatusCode::SEE_OTHER, "/settings/rules"));
    let rules = db.rules(&sc).await.unwrap();
    assert_eq!((rules[0].day.as_str(), rules[0].slot.as_str(), rules[0].household_ids.as_slice()), ("monday", "dinner", &[home][..]));
    let (_, _, html) = go(&app, "GET", "/settings/rules", None, None).await;
    assert!(html.contains("monday") && html.contains("%soup%"), "{html}");

    let id = rules[0].id;
    let (s, ..) = go(&app, "POST", &format!("/settings/rules/{id}"), None, Some("day=unset&slot=unset&query_filter=")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let r = db.rule(&sc, id).await.unwrap();
    assert_eq!((r.day.as_str(), r.slot.as_str(), r.query_filter.as_str(), r.household_ids.len()), ("unset", "unset", "", 0));
    let (s, ..) = go(&app, "POST", "/settings/rules", None, Some("day=someday")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, ..) = go(&app, "POST", &format!("/settings/rules/{id}/delete"), None, Some("")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(db.rules(&sc).await.unwrap().is_empty());
}
