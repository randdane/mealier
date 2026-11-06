//! Step 14, part A, through the API: the new recipe fields in the JSON, the lock, asset upload and
//! download (always an attachment, never inline), comments, food substitutions and extras.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, api};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn app() -> Router {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() })
}

async fn raw(app: &Router, method: &str, uri: &str, content_type: Option<&str>, body: Vec<u8>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", common::BEARER);
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let (s, h) = (resp.status(), resp.headers().clone());
    (s, h, resp.into_body().collect().await.unwrap().to_bytes().to_vec())
}

async fn json_req(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (s, _, b) = raw(app, method, uri, body.as_ref().map(|_| "application/json"), body.map(|v| v.to_string().into_bytes()).unwrap_or_default()).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn upload_body(file_name: &str, bytes: &[u8], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut b = Vec::new();
    for (k, v) in fields {
        b.extend_from_slice(format!("--XX\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    b.extend_from_slice(format!("--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: text/html\r\n\r\n").as_bytes());
    b.extend_from_slice(bytes);
    b.extend_from_slice(b"\r\n--XX--\r\n");
    b
}

async fn upload(app: &Router, recipe: i64, file_name: &str, bytes: &[u8], fields: &[(&str, &str)]) -> (StatusCode, Value) {
    let (s, _, b) = raw(app, "POST", &format!("/api/recipes/{recipe}/assets"), Some("multipart/form-data; boundary=XX"), upload_body(file_name, bytes, fields)).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn recipe(app: &Router, body: Value) -> i64 {
    let (s, v) = json_req(app, "POST", "/api/recipes", Some(body)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v["id"].as_i64().unwrap()
}

#[tokio::test]
async fn the_recipe_json_carries_the_new_fields_and_a_locked_recipe_answers_422() {
    let app = app().await;
    let (s, v) = json_req(
        &app,
        "POST",
        "/api/recipes",
        Some(json!({
            "name": "Soup",
            "ingredients": [{"original_text": "2 onions"}, {"original_text": "oil"}],
            "ingredient_details": [{"section": "Base"}, {"substitutions": [{"qty": 2, "unit": "tbsp", "food": "butter", "note": "ok"}]}],
            "steps": ["Chop", "Fry"],
            "step_details": [{"title": "Prep"}],
            "notes": [{"title": "Tip", "text": "Salt late"}],
            "extras": {"source": "gran"},
            "settings": {"locked": true, "hide_nutrition": true, "public": true}
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    let uid = v["ingredients"][0]["uid"].as_str().unwrap().to_string();
    assert!(!uid.is_empty());
    assert_eq!(v["ingredients"][0]["section"], "Base");
    assert_eq!(v["ingredients"][1]["substitutions"][0], json!({"qty": 2.0, "unit": "tbsp", "food": "butter", "note": "ok"}));
    assert_eq!(v["step_details"][0]["title"], "Prep");
    assert_eq!(v["step_details"][1], json!({"title": "", "ingredient_uids": []}));
    assert_eq!(v["notes"][0], json!({"title": "Tip", "text": "Salt late"}));
    assert_eq!(v["extras"], json!({"source": "gran"}));
    assert_eq!(v["settings"]["locked"], true);
    assert_eq!(v["settings"]["public"], true);
    assert_eq!(v["settings"]["landscape_view"], false);

    let (s, v) = json_req(&app, "PUT", &format!("/api/recipes/{id}"), Some(json!({"name": "Edited"}))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("recipe is locked")));
    let (s, v) = upload(&app, id, "a.txt", b"hello", &[]).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("recipe is locked")));
    let (s, v) = json_req(&app, "PUT", &format!("/api/recipes/{id}"), Some(json!({"name": "Edited", "settings": {"locked": false}}))).await;
    assert_eq!((s, v["name"].as_str(), v["settings"]["locked"].as_bool()), (StatusCode::OK, Some("Edited"), Some(false)));
    assert_eq!(v["notes"].as_array().unwrap().len(), 1, "notes left out of the body are kept");
    assert_eq!(upload(&app, id, "a.txt", b"hello", &[]).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn an_asset_is_downloaded_as_an_attachment_with_a_clean_name_and_never_inline() {
    let app = app().await;
    let id = recipe(&app, json!({"name": "Soup"})).await;
    let other = recipe(&app, json!({"name": "Stew"})).await;
    let bytes = b"<script>alert(1)</script>";
    let (s, v) = upload(&app, id, "../x;y.html", bytes, &[("name", "Menu"), ("icon", "mdi-file")]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!((v["name"].as_str(), v["icon"].as_str(), v["file_name"].as_str(), v["size"].as_i64()), (Some("Menu"), Some("mdi-file"), Some("x_y.html"), Some(bytes.len() as i64)));
    let hash = v["hash"].as_str().unwrap().to_string();

    let (s, h, body) = raw(&app, "GET", &format!("/api/assets/{hash}?recipe={id}"), None, vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, bytes);
    assert_eq!(h["content-disposition"], "attachment; filename=\"x_y.html\"");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["content-type"], "application/octet-stream");

    for uri in [format!("/api/assets/{hash}"), format!("/api/assets/{hash}?recipe={other}"), format!("/api/assets/{hash}?recipe=999"), format!("/api/assets/{}?recipe={id}", "0".repeat(64)), format!("/api/assets/..%2Fsecret?recipe={id}")] {
        assert_eq!(raw(&app, "GET", &uri, None, vec![]).await.0, StatusCode::NOT_FOUND, "{uri}");
    }

    let (s, list) = json_req(&app, "GET", &format!("/api/recipes/{id}/assets"), None).await;
    assert_eq!((s, list[0]["hash"].as_str()), (StatusCode::OK, Some(hash.as_str())));
    let (_, r) = json_req(&app, "GET", &format!("/api/recipes/{id}"), None).await;
    assert_eq!(r["assets"], json!([hash]));

    let (s, ..) = raw(&app, "DELETE", &format!("/api/assets/{hash}?recipe={id}"), None, vec![]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(raw(&app, "GET", &format!("/api/assets/{hash}?recipe={id}"), None, vec![]).await.0, StatusCode::NOT_FOUND);
    let (_, r) = json_req(&app, "GET", &format!("/api/recipes/{id}"), None).await;
    assert_eq!(r["assets"], json!([]));
}

#[tokio::test]
async fn an_upload_needs_a_file_and_stops_at_20_mb() {
    let app = app().await;
    let id = recipe(&app, json!({"name": "Soup"})).await;
    let (s, v) = raw(&app, "POST", &format!("/api/recipes/{id}/assets"), Some("multipart/form-data; boundary=XX"), b"--XX\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nx\r\n--XX--\r\n".to_vec()).await.pipe_json();
    assert_eq!((s, v["error"].as_str().map(|e| e.contains("file"))), (StatusCode::UNPROCESSABLE_ENTITY, Some(true)), "{v}");
    let (s, v) = upload(&app, id, "big.bin", &vec![1u8; 20 * 1024 * 1024 + 1], &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"].as_str().unwrap().contains("20 MB"), "{v}");
    assert_eq!(upload(&app, id, "ok.bin", &vec![1u8; 20 * 1024 * 1024], &[]).await.0, StatusCode::CREATED);
}

trait PipeJson {
    fn pipe_json(self) -> (StatusCode, Value);
}
impl PipeJson for (StatusCode, HeaderMap, Vec<u8>) {
    fn pipe_json(self) -> (StatusCode, Value) {
        (self.0, serde_json::from_slice(&self.2).unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn comments_can_be_posted_unless_the_recipe_disables_them() {
    let app = app().await;
    let open = recipe(&app, json!({"name": "Soup"})).await;
    let shut = recipe(&app, json!({"name": "Stew", "settings": {"disable_comments": true}})).await;
    let (s, v) = json_req(&app, "POST", &format!("/api/recipes/{open}/comments"), Some(json!({"text": "lovely"}))).await;
    assert_eq!((s, v["text"].as_str()), (StatusCode::CREATED, Some("lovely")), "{v}");
    let (s, v) = json_req(&app, "GET", &format!("/api/recipes/{open}/comments"), None).await;
    assert_eq!((s, v.as_array().map(|a| a.len())), (StatusCode::OK, Some(1)));
    let (s, v) = json_req(&app, "POST", &format!("/api/recipes/{shut}/comments"), Some(json!({"text": "hi"}))).await;
    assert_eq!((s, v["error"].as_str().map(|e| e.contains("comments are disabled"))), (StatusCode::UNPROCESSABLE_ENTITY, Some(true)), "{v}");
}

#[tokio::test]
async fn food_substitutions_and_extras_have_their_own_routes() {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    let sc = common::scope(&db).await;
    let (butter, oil) = (db.add_food(&sc, "butter").await.unwrap(), db.add_food(&sc, "oil").await.unwrap());
    let app2 = api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() });

    let (s, v) = json_req(&app2, "POST", &format!("/api/foods/{butter}/substitutions"), Some(json!({"food_id": oil, "note": "1:1"}))).await;
    assert_eq!((s, v[0]["name"].as_str(), v[0]["note"].as_str()), (StatusCode::CREATED, Some("oil"), Some("1:1")), "{v}");
    let entry = v[0]["id"].as_i64().unwrap();
    let (s, v) = json_req(&app2, "POST", &format!("/api/foods/{butter}/substitutions"), Some(json!({"food_id": butter}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "a food cannot substitute itself: {v}");
    let (s, v) = json_req(&app2, "PUT", &format!("/api/food-substitutions/{entry}"), Some(json!({"note": "3:4"}))).await;
    assert_eq!((s, v[0]["note"].as_str()), (StatusCode::OK, Some("3:4")), "{v}");
    let (s, v) = json_req(&app2, "PUT", &format!("/api/foods/{butter}/extras"), Some(json!({"aisle": "3"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (s, v) = json_req(&app2, "GET", &format!("/api/foods/{butter}"), None).await;
    assert_eq!((s, v["name"].as_str(), v["extras"].clone(), v["substitutions"][0]["food_id"].as_i64()), (StatusCode::OK, Some("butter"), json!({"aisle": "3"}), Some(oil)));
    let (s, ..) = raw(&app2, "DELETE", &format!("/api/food-substitutions/{entry}"), None, vec![]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = json_req(&app2, "GET", &format!("/api/foods/{butter}"), None).await;
    assert_eq!(v["substitutions"], json!([]));
}

#[tokio::test]
async fn shopping_lists_and_items_take_extras_and_show_them() {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    let sc = common::scope(&db).await;
    let list = db.create_list(&sc, "Weekly").await.unwrap().id;
    let app = api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() });
    let (s, v) = json_req(&app, "PUT", &format!("/api/shopping-lists/{list}/extras"), Some(json!({"store": "co-op"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (s, v) = json_req(&app, "POST", &format!("/api/shopping-lists/{list}/items"), Some(json!({"note": "milk", "extras": {"brand": "x"}}))).await;
    assert_eq!((s, v["extras"].clone()), (StatusCode::CREATED, json!({"brand": "x"})), "{v}");
    let item = v["id"].as_i64().unwrap();
    let (s, v) = json_req(&app, "PUT", &format!("/api/shopping-list-items/{item}"), Some(json!({"note": "milk 2"}))).await;
    assert_eq!((s, v["extras"].clone()), (StatusCode::OK, json!({"brand": "x"})), "left out: kept");
    let (_, v) = json_req(&app, "GET", &format!("/api/shopping-lists/{list}"), None).await;
    assert_eq!((v["extras"].clone(), v["items"][0]["extras"].clone()), (json!({"store": "co-op"}), json!({"brand": "x"})));
}

#[tokio::test]
async fn openapi_describes_the_new_routes() {
    let app = app().await;
    let (_, spec) = json_req(&app, "GET", "/api/openapi.json", None).await;
    let paths = spec["paths"].as_object().unwrap();
    for (path, method) in [
        ("/api/recipes/{id}/assets", "post"),
        ("/api/recipes/{id}/assets", "get"),
        ("/api/assets/{id}", "delete"),
        ("/api/assets/{id}", "get"),
        ("/api/recipes/{id}/comments", "post"),
        ("/api/recipes/{id}/comments", "get"),
        ("/api/foods/{id}", "get"),
        ("/api/foods/{id}/extras", "put"),
        ("/api/foods/{id}/substitutions", "get"),
        ("/api/foods/{id}/substitutions", "post"),
        ("/api/food-substitutions/{id}", "put"),
        ("/api/food-substitutions/{id}", "delete"),
        ("/api/shopping-lists/{id}/extras", "put"),
    ] {
        assert!(paths.get(path).and_then(|p| p.get(method)).is_some(), "{method} {path} is not in the OpenAPI document");
    }
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    for name in ["Asset", "Settings", "LineDetail", "StepDetail", "Note", "Substitution", "FoodDetail"] {
        assert!(schemas.contains_key(name), "schema {name}");
    }
}
