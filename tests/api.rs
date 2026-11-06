use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, api};
use serde_json::{Value, json};
use tower::ServiceExt;

/// One request through the router. Returns the status and the JSON body
/// (`Value::Null` when the body is empty).
async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, json)
}

#[tokio::test]
async fn post_recipes_returns_201_with_the_recipe() {
    let app = api::router(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "POST", "/api/recipes", Some(json!({"name": "Soup"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "Soup");
    assert!(body["id"].as_i64().is_some());
}

#[tokio::test]
async fn post_recipes_with_empty_name_returns_422_with_error() {
    let app = api::router(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "POST", "/api/recipes", Some(json!({"name": "  "}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("name"), "{body}");
}

#[tokio::test]
async fn get_recipes_returns_200_and_honours_include_archived() {
    let db = Db::open_memory().await.unwrap();
    db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let gone = db.create_recipe(mealier::RecipeInput { name: "Stew".into(), ..Default::default() }).await.unwrap();
    db.archive_recipe(gone.id).await.unwrap();
    let app = api::router(db);

    let (status, body) = send(&app, "GET", "/api/recipes?include_archived=false", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");

    let (status, body) = send(&app, "GET", "/api/recipes?include_archived=true", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2, "{body}");
}

#[tokio::test]
async fn get_recipe_returns_200_with_the_recipe() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let app = api::router(db);

    let (status, body) = send(&app, "GET", &format!("/api/recipes/{}", r.id), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Soup");
    assert_eq!(body["id"], r.id);
}

#[tokio::test]
async fn get_recipe_missing_returns_404_with_error() {
    let app = api::router(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "GET", "/api/recipes/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn put_recipe_returns_200_with_the_updated_recipe() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Pancakes".into(), ..Default::default() }).await.unwrap();
    let app = api::router(db);

    let (status, body) =
        send(&app, "PUT", &format!("/api/recipes/{}", r.id), Some(json!({"name": "Waffles", "tags": ["breakfast"]})))
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Waffles");
    assert_eq!(body["tags"], json!(["breakfast"]));
}

#[tokio::test]
async fn put_recipe_missing_returns_404() {
    let app = api::router(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "PUT", "/api/recipes/999", Some(json!({"name": "Ghost"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn put_recipe_with_empty_name_returns_422() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let app = api::router(db);
    let (status, body) = send(&app, "PUT", &format!("/api/recipes/{}", r.id), Some(json!({"name": ""}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn archive_returns_204_and_hides_the_recipe_from_the_default_list() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let app = api::router(db);

    let (status, _) = send(&app, "POST", &format!("/api/recipes/{}/archive", r.id), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(&app, "GET", "/api/recipes", None).await;
    assert!(list.as_array().unwrap().is_empty(), "{list}");
}

#[tokio::test]
async fn get_recipe_snapshots_returns_200_newest_first() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Pancakes".into(), ..Default::default() }).await.unwrap();
    let app = api::router(db);
    let (_, updated) = send(&app, "PUT", &format!("/api/recipes/{}", r.id), Some(json!({"name": "Waffles"}))).await;

    let (status, body) = send(&app, "GET", &format!("/api/recipes/{}/snapshots", r.id), None).await;
    assert_eq!(status, StatusCode::OK);
    let snaps = body.as_array().unwrap();
    assert_eq!(snaps.len(), 2, "{body}");
    assert_eq!(snaps[0]["id"], updated["current_snapshot_id"], "newest first");
}

#[tokio::test]
async fn get_snapshot_returns_200_with_its_content() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Pancakes".into(), ..Default::default() }).await.unwrap();
    let app = api::router(db);

    let (status, body) = send(&app, "GET", &format!("/api/snapshots/{}", r.current_snapshot_id), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"]["name"], "Pancakes");
}

#[tokio::test]
async fn get_snapshot_missing_returns_404() {
    let app = api::router(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "GET", "/api/snapshots/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn get_recipe_snapshots_missing_returns_404() {
    let app = api::router(Db::open_memory().await.unwrap());
    let (status, _) = send(&app, "GET", "/api/recipes/999/snapshots", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
