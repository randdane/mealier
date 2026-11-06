//! Labels over HTTP: the JSON API and the two pages (settings, list sections).
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, ItemInput, LabelInput, api};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn site() -> (Router, Db) {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    (api::router(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }), db)
}

/// Status, `location` header and body text. `json` bodies go as JSON, `form` bodies urlencoded.
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

#[tokio::test]
async fn label_api_creates_lists_updates_and_deletes() {
    let (app, _) = site().await;
    let (s, b) = api_json(&app, "POST", "/api/labels", Some(json!({"name": "Dairy", "color": "#0000FF"}))).await;
    assert_eq!((s, b["name"].as_str(), b["color"].as_str()), (StatusCode::CREATED, Some("Dairy"), Some("#0000ff")), "{b}");
    let id = b["id"].as_i64().unwrap();
    let (s, b) = api_json(&app, "POST", "/api/labels", Some(json!({"name": "Bad", "color": "blue"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let (s, _) = api_json(&app, "POST", "/api/labels", Some(json!({"name": "Dairy", "color": "#000000"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "duplicate name");
    let (s, b) = api_json(&app, "GET", "/api/labels", None).await;
    assert_eq!((s, b.as_array().unwrap().len()), (StatusCode::OK, 1));
    let (s, b) = api_json(&app, "PUT", &format!("/api/labels/{id}"), Some(json!({"name": "Milk", "color": "#112233"}))).await;
    assert_eq!((s, b["name"].as_str()), (StatusCode::OK, Some("Milk")));
    let (s, _) = api_json(&app, "DELETE", &format!("/api/labels/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api_json(&app, "DELETE", &format!("/api/labels/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn food_label_and_section_order_show_in_the_list_json() {
    let (app, db) = site().await;
    let sc = common::scope(&db).await;
    let a = db.add_label(&sc, LabelInput { name: "A".into(), color: "#aa0000".into() }).await.unwrap();
    let b = db.add_label(&sc, LabelInput { name: "B".into(), color: "#00bb00".into() }).await.unwrap();
    let (fa, fb) = (db.add_food(&sc, "fa").await.unwrap(), db.add_food(&sc, "fb").await.unwrap());
    let list = db.create_list(&sc, "L").await.unwrap();
    for f in [fa, fb] {
        db.add_item(&sc, list.id, ItemInput { food_id: Some(f), ..Default::default() }).await.unwrap();
    }
    let (s, _) = api_json(&app, "PUT", &format!("/api/foods/{fa}/label"), Some(json!({"label_id": a.id}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api_json(&app, "PUT", &format!("/api/foods/{fb}/label"), Some(json!({"label_id": b.id}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, got) = api_json(&app, "GET", &format!("/api/shopping-lists/{}", list.id), None).await;
    let titles = |v: &Value| v["sections"].as_array().unwrap().iter().map(|s| s["label"]["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(titles(&got), vec!["A", "B"]);
    assert_eq!(got["sections"][0]["label"]["color"], "#aa0000");
    assert_eq!(got["sections"][0]["items"][0]["food_id"], fa);

    let (s, got) = api_json(&app, "PUT", &format!("/api/shopping-lists/{}/section-order", list.id), Some(json!({"label_ids": [b.id, a.id]}))).await;
    assert_eq!((s, titles(&got)), (StatusCode::OK, vec!["B".to_string(), "A".to_string()]));
    let (s, _) = api_json(&app, "PUT", &format!("/api/shopping-lists/{}/section-order", list.id), Some(json!({"label_ids": [999]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // clearing
    let (s, _) = api_json(&app, "PUT", &format!("/api/foods/{fa}/label"), Some(json!({"label_id": null}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(db.food_label(&sc, fa).await.unwrap(), None);
}

#[tokio::test]
async fn settings_page_manages_labels_with_a_colour_picker() {
    let (app, db) = site().await;
    let sc = common::scope(&db).await;
    let (s, loc, _) = go(&app, "POST", "/settings/labels", None, Some("name=Dairy&color=%230000ff")).await;
    assert_eq!((s, loc.as_str()), (StatusCode::SEE_OTHER, "/settings/labels"));
    let (s, _, html) = go(&app, "GET", "/settings/labels", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("Dairy") && html.contains("type=\"color\"") && html.contains("value=\"#0000ff\""), "{html}");
    let id = db.labels(&sc).await.unwrap()[0].id;
    let (s, _, _) = go(&app, "POST", &format!("/settings/labels/{id}"), None, Some("name=Milk&color=%23ff0000")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(db.labels(&sc).await.unwrap()[0].name, "Milk");
    let (s, _, html) = go(&app, "POST", "/settings/labels", None, Some("name=Bad&color=red")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{html}");
    let (s, _, _) = go(&app, "POST", &format!("/settings/labels/{id}/delete"), None, Some("")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(db.labels(&sc).await.unwrap().is_empty());
}

#[tokio::test]
async fn list_page_shows_sections_with_colour_borders_and_moves_them() {
    let (app, db) = site().await;
    let sc = common::scope(&db).await;
    let a = db.add_label(&sc, LabelInput { name: "Aisle A".into(), color: "#aa0000".into() }).await.unwrap();
    let b = db.add_label(&sc, LabelInput { name: "Aisle B".into(), color: "#00bb00".into() }).await.unwrap();
    let list = db.create_list(&sc, "L").await.unwrap();
    for (label, note) in [(a.id, "apples"), (b.id, "beans")] {
        db.add_item(&sc, list.id, ItemInput { note: note.into(), label_id: Some(label), ..Default::default() }).await.unwrap();
    }
    db.add_item(&sc, list.id, ItemInput { note: "string".into(), ..Default::default() }).await.unwrap();
    let page = || async { go(&app, "GET", &format!("/lists/{}", list.id), None, None).await.2 };
    let html = page().await;
    let at = |h: &str, needle: &str| h.find(needle).unwrap_or_else(|| panic!("{needle} missing: {h}"));
    assert!(html.contains("border-left: 4px solid #aa0000") && html.contains("border-left: 4px solid #00bb00"), "{html}");
    assert!(at(&html, "Aisle A") < at(&html, "Aisle B") && at(&html, "Aisle B") < at(&html, "No label"), "{html}");
    assert!(html.contains(&format!("/lists/{}/sections/{}/move", list.id, b.id)), "{html}");

    let (s, loc, _) = go(&app, "POST", &format!("/lists/{}/sections/{}/move", list.id, b.id), None, Some("dir=up")).await;
    assert_eq!((s, loc), (StatusCode::SEE_OTHER, format!("/lists/{}", list.id)));
    let html = page().await;
    assert!(at(&html, "Aisle B") < at(&html, "Aisle A"), "{html}");
}
