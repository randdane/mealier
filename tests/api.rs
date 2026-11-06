use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, api};
use serde_json::{Value, json};
use tower::ServiceExt;

/// The router over `db`, with a throwaway images dir.
fn router_for(db: Db) -> Router {
    api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() })
}

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
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "POST", "/api/recipes", Some(json!({"name": "Soup"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "Soup");
    assert!(body["id"].as_i64().is_some());
}

#[tokio::test]
async fn post_recipes_with_empty_name_returns_422_with_error() {
    let app = router_for(Db::open_memory().await.unwrap());
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
    let app = router_for(db);

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
    let app = router_for(db);

    let (status, body) = send(&app, "GET", &format!("/api/recipes/{}", r.id), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Soup");
    assert_eq!(body["id"], r.id);
}

#[tokio::test]
async fn get_recipe_missing_returns_404_with_error() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "GET", "/api/recipes/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn put_recipe_returns_200_with_the_updated_recipe() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Pancakes".into(), ..Default::default() }).await.unwrap();
    let app = router_for(db);

    let (status, body) =
        send(&app, "PUT", &format!("/api/recipes/{}", r.id), Some(json!({"name": "Waffles", "tags": ["breakfast"]})))
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Waffles");
    assert_eq!(body["tags"], json!(["breakfast"]));
}

#[tokio::test]
async fn put_recipe_missing_returns_404() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "PUT", "/api/recipes/999", Some(json!({"name": "Ghost"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn put_recipe_with_empty_name_returns_422() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let app = router_for(db);
    let (status, body) = send(&app, "PUT", &format!("/api/recipes/{}", r.id), Some(json!({"name": ""}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn archive_returns_204_and_hides_the_recipe_from_the_default_list() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let app = router_for(db);

    let (status, _) = send(&app, "POST", &format!("/api/recipes/{}/archive", r.id), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(&app, "GET", "/api/recipes", None).await;
    assert!(list.as_array().unwrap().is_empty(), "{list}");
}

#[tokio::test]
async fn get_recipe_snapshots_returns_200_newest_first() {
    let db = Db::open_memory().await.unwrap();
    let r = db.create_recipe(mealier::RecipeInput { name: "Pancakes".into(), ..Default::default() }).await.unwrap();
    let app = router_for(db);
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
    let app = router_for(db);

    let (status, body) = send(&app, "GET", &format!("/api/snapshots/{}", r.current_snapshot_id), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"]["name"], "Pancakes");
}

#[tokio::test]
async fn get_snapshot_missing_returns_404() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "GET", "/api/snapshots/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn get_recipe_snapshots_missing_returns_404() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, _) = send(&app, "GET", "/api/recipes/999/snapshots", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_recipes_q_filters_by_name_or_description() {
    let db = Db::open_memory().await.unwrap();
    db.create_recipe(mealier::RecipeInput { name: "Tomato Soup".into(), ..Default::default() }).await.unwrap();
    db.create_recipe(mealier::RecipeInput { name: "Stew".into(), description: "with TOMATOES".into(), ..Default::default() })
        .await
        .unwrap();
    db.create_recipe(mealier::RecipeInput { name: "Pie".into(), ..Default::default() }).await.unwrap();
    let app = router_for(db);

    let (status, body) = send(&app, "GET", "/api/recipes?q=tomato", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = body.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Stew", "Tomato Soup"]);
}

#[tokio::test]
async fn post_recipes_with_reversed_range_returns_422() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, body) = send(
        &app,
        "POST",
        "/api/recipes",
        Some(json!({"name": "Bread", "ingredients": [{"qty_min": 3.0, "qty_max": 2.0}]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn openapi_lists_every_recipe_route() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, body) = send(&app, "GET", "/api/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["openapi"].as_str().unwrap().starts_with("3."), "{body}");

    let mut found: Vec<String> = Vec::new();
    for (path, item) in body["paths"].as_object().unwrap() {
        for method in ["get", "post", "put", "patch", "delete"] {
            if item.get(method).is_some() {
                found.push(format!("{} {}", method.to_uppercase(), path));
            }
        }
    }
    found.sort();

    let mut expected = vec![
        "GET /api/recipes",
        "POST /api/recipes",
        "POST /api/recipes/import",
        "GET /api/recipes/{id}",
        "PUT /api/recipes/{id}",
        "POST /api/recipes/{id}/archive",
        "GET /api/recipes/{id}/snapshots",
        "GET /api/snapshots/{id}",
        "POST /api/recipes/{id}/batches",
        "GET /api/recipes/{id}/batches",
        "GET /api/batches/{id}",
        "PUT /api/batches/{id}",
        "POST /api/batches/{id}/lines",
        "POST /api/batches/{id}/finish",
        "POST /api/batches/{id}/photos",
        "POST /api/parse",
    ];
    expected.sort();
    assert_eq!(found, expected);
}

// --- batch log ---

/// A recipe with one ingredient (2 eggs); returns the app, the db and the recipe id.
async fn app_with_recipe() -> (Router, Db, i64) {
    let db = Db::open_memory().await.unwrap();
    let r = db
        .create_recipe(mealier::RecipeInput {
            name: "Omelette".into(),
            ingredients: vec![mealier::IngredientInput { qty_min: Some(2.0), qty_max: Some(2.0), ..Default::default() }],
            ..Default::default()
        })
        .await
        .unwrap();
    (router_for(db.clone()), db, r.id)
}

#[tokio::test]
async fn post_recipe_batches_returns_201_with_a_planned_batch_and_scaled_lines() {
    let (app, _, rid) = app_with_recipe().await;
    let (status, body) = send(&app, "POST", &format!("/api/recipes/{rid}/batches"), Some(json!({"scale": 1.5}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], "planned");
    assert_eq!(body["scale"], 1.5);
    assert_eq!(body["lines"][0]["qty_min"], 3.0);
}

#[tokio::test]
async fn post_recipe_batches_with_scale_zero_returns_422() {
    let (app, _, rid) = app_with_recipe().await;
    let (status, body) = send(&app, "POST", &format!("/api/recipes/{rid}/batches"), Some(json!({"scale": 0}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("scale"), "{body}");
}

#[tokio::test]
async fn post_recipe_batches_for_a_missing_recipe_returns_404() {
    let (app, ..) = app_with_recipe().await;
    let (status, _) = send(&app, "POST", "/api/recipes/999/batches", Some(json!({"scale": 1}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_batch_returns_200_and_404() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap();
    let (status, body) = send(&app, "GET", &format!("/api/batches/{}", b.batch.id), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], b.batch.id);
    assert_eq!(body["lines"].as_array().unwrap().len(), 1);
    let (status, _) = send(&app, "GET", "/api/batches/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_batch_returns_200_with_the_saved_batch() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap();
    let line = b.lines[0].id;
    let put = json!({"scale": 1, "rating": 5, "notes": "good", "made_on": "2026-11-02",
                     "lines": [{"id": line, "qty_min": 3, "substitution": "duck eggs"}]});
    let (status, body) = send(&app, "PUT", &format!("/api/batches/{}", b.batch.id), Some(put)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!((body["rating"].clone(), body["notes"].clone()), (json!(5), json!("good")));
    assert_eq!(body["lines"][0]["qty_max"], 3.0);
    assert_eq!(body["lines"][0]["substitution"], "duck eggs");
}

#[tokio::test]
async fn put_batch_with_rating_6_returns_422() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap();
    let put = json!({"scale": 1, "rating": 6, "lines": [{"id": b.lines[0].id}]});
    let (status, body) = send(&app, "PUT", &format!("/api/batches/{}", b.batch.id), Some(put)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("rating"), "{body}");
}

#[tokio::test]
async fn put_batch_missing_returns_404() {
    let (app, ..) = app_with_recipe().await;
    let (status, _) = send(&app, "PUT", "/api/batches/999", Some(json!({"scale": 1}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_batch_lines_returns_201_with_the_added_line() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap();
    let (status, body) = send(&app, "POST", &format!("/api/batches/{}/lines", b.batch.id), Some(json!({"qty_min": 1, "substitution": "pinch of salt"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["lines"].as_array().unwrap().len(), 2);
    assert_eq!(body["lines"][1]["added"], true);
}

#[tokio::test]
async fn post_batch_lines_with_negative_qty_returns_422_and_missing_batch_404() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap();
    let (status, _) = send(&app, "POST", &format!("/api/batches/{}/lines", b.batch.id), Some(json!({"qty_min": -1}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = send(&app, "POST", "/api/batches/999/lines", Some(json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_batch_finish_returns_200_done_then_422_the_second_time() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap();
    let uri = format!("/api/batches/{}/finish", b.batch.id);
    let (status, body) = send(&app, "POST", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "done");
    assert!(body["made_on"].is_string(), "{body}");
    let (status, body) = send(&app, "POST", &uri, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("done"), "{body}");
}

#[tokio::test]
async fn post_batch_finish_missing_returns_404() {
    let (app, ..) = app_with_recipe().await;
    let (status, _) = send(&app, "POST", "/api/batches/999/finish", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_recipe_batches_returns_200_newest_first_and_404_for_a_missing_recipe() {
    let (app, db, rid) = app_with_recipe().await;
    let first = db.start_batch(rid, 1.0).await.unwrap().batch.id;
    let second = db.start_batch(rid, 2.0).await.unwrap().batch.id;
    let (status, body) = send(&app, "GET", &format!("/api/recipes/{rid}/batches"), None).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<i64> = body.as_array().unwrap().iter().map(|b| b["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![second, first]);
    let (status, _) = send(&app, "GET", "/api/recipes/999/batches", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// POST of one multipart file field named `photo`.
async fn upload(app: &Router, uri: &str, field: &str, bytes: &[u8]) -> (StatusCode, Value) {
    let mut body = format!("--XB\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"p\"\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n--XB--\r\n");
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "multipart/form-data; boundary=XB")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nhello";

#[tokio::test]
async fn post_batch_photos_returns_201_and_stores_the_file() {
    let db = Db::open_memory().await.unwrap();
    let rid = db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap().id;
    let b = db.start_batch(rid, 1.0).await.unwrap().batch.id;
    let dir = tempfile::tempdir().unwrap();
    let app = api::router(api::AppState { db: db.clone(), images_dir: dir.path().to_path_buf() });

    let (status, body) = upload(&app, &format!("/api/batches/{b}/photos"), "photo", PNG).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["photos"][0]["hash"], "0e8754cfeb908584f7a2f2ccd08e37fda8d3d540cbaf2e16ac559c7ea59a7151");
    assert!(dir.path().join("0e8754cfeb908584f7a2f2ccd08e37fda8d3d540cbaf2e16ac559c7ea59a7151.png").exists());
}

#[tokio::test]
async fn post_batch_photos_rejects_non_images_a_missing_field_and_a_sixth_photo_with_422() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap().batch.id;
    let uri = format!("/api/batches/{b}/photos");
    let (status, _) = upload(&app, &uri, "photo", b"not an image").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = upload(&app, &uri, "other", PNG).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    for n in 0..5u8 {
        let mut p = PNG.to_vec();
        p.push(n);
        assert_eq!(upload(&app, &uri, "photo", &p).await.0, StatusCode::CREATED);
    }
    let mut sixth = PNG.to_vec();
    sixth.push(9);
    let (status, body) = upload(&app, &uri, "photo", &sixth).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("5 photos"), "{body}");
}

#[tokio::test]
async fn post_batch_photos_over_10_mb_returns_422() {
    let (app, db, rid) = app_with_recipe().await;
    let b = db.start_batch(rid, 1.0).await.unwrap().batch.id;
    let mut big = PNG.to_vec();
    big.resize(10 * 1024 * 1024 + 1, 0);
    let (status, _) = upload(&app, &format!("/api/batches/{b}/photos"), "photo", &big).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn post_batch_photos_missing_batch_returns_404() {
    let (app, ..) = app_with_recipe().await;
    let (status, _) = upload(&app, "/api/batches/999/photos", "photo", PNG).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn raw(app: &Router, method: &str, uri: &str, form: Option<&str>) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match form {
        Some(f) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(f.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn cook_page_shows_every_step_scaled_ingredients_and_requests_a_wake_lock() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (_, r) = send(&app, "POST", "/api/recipes", Some(json!({"name": "Soup", "steps": ["Chop the onion", "Simmer 20 min"], "ingredients": [{"original_text": "2 onions", "qty_min": 2.0, "qty_max": 2.0}]}))).await;
    let uri = format!("/recipes/{}/cook?scale=3", r["id"]);
    let (status, _, html) = raw(&app, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Chop the onion") && html.contains("Simmer 20 min"), "{html}");
    assert!(html.contains("navigator.wakeLock.request('screen')"));
    assert!(html.contains("<li>6</li>"), "scaled amount missing: {html}");
    assert!(html.contains("cook-normal"));
}

#[tokio::test]
async fn cook_theme_post_rejects_unknown_themes_and_sets_the_cookie_for_known_ones() {
    let app = router_for(Db::open_memory().await.unwrap());
    let (status, _, _) = raw(&app, "POST", "/settings/cook-theme", Some("theme=neon&back=%2F")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, headers, _) = raw(&app, "POST", "/settings/cook-theme", Some("theme=max-white&back=%2Frecipes%2F1%2Fcook")).await;
    assert!(status.is_redirection());
    assert_eq!(headers["location"], "/recipes/1/cook");
    assert!(headers["set-cookie"].to_str().unwrap().starts_with("cook_theme=max-white;"));

    let (_, headers, _) = raw(&app, "POST", "/settings/cook-theme", Some("theme=normal&back=%2F%2Fevil.example")).await;
    assert_eq!(headers["location"], "/");
}

#[tokio::test]
async fn post_parse_returns_a_parsed_entry_per_line() {
    let db = Db::open_memory().await.unwrap();
    let cup = db.add_unit("cup", mealier::Dimension::Volume, Some(236.5882365)).await.unwrap();
    let flour = db.add_food("flour").await.unwrap();
    let app = router_for(db);
    let (status, body) = send(&app, "POST", "/api/parse", Some(json!({"lines": ["1 1/2 cups flour, sifted", "???"]}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([
            {"qty_min": 1.5, "qty_max": 1.5, "unit_id": cup, "food_id": flour, "note": "sifted",
             "original_text": "1 1/2 cups flour, sifted", "confidence": "full"},
            {"qty_min": null, "qty_max": null, "unit_id": null, "food_id": null, "note": "",
             "original_text": "???", "confidence": "none"},
        ])
    );
}
