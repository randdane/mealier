mod common;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt;
use mealier::fetch::Fetcher;
use mealier::{Chain, Db, Dimension, ImportService, IngredientInput, Llm, LlmConfig, RecipeInput, api, fill_unparsed};
use serde_json::{Value, json};
use tower::ServiceExt;

const KEY: &str = "sk-feature-KEY-456";

fn loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

/// An anthropic-shaped reply whose text is `v`.
fn reply(v: Value) -> String {
    json!({"content": [{"type": "text", "text": v.to_string()}]}).to_string()
}

type Seen = Arc<Mutex<Vec<Value>>>;
type Replies = Arc<Mutex<Vec<(u16, String)>>>;

/// A fake anthropic endpoint: answers with `replies` in order (the last repeats) and records request bodies.
async fn mock(replies: Vec<(u16, String)>) -> (String, Seen) {
    let seen: Seen = Default::default();
    let replies: Replies = Arc::new(Mutex::new(replies));
    let app = Router::new()
        .fallback(|State((seen, replies)): State<(Seen, Replies)>, body: String| async move {
            let n = {
                let mut s = seen.lock().unwrap();
                s.push(serde_json::from_str(&body).unwrap());
                s.len()
            };
            let r = replies.lock().unwrap();
            let (status, text) = r[(n - 1).min(r.len() - 1)].clone();
            (StatusCode::from_u16(status).unwrap(), [("content-type", "application/json")], text)
        })
        .with_state((seen.clone(), replies));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn llm(base: &str) -> Arc<Llm> {
    use std::os::unix::fs::PermissionsExt;
    let p = tempfile::tempdir().unwrap().keep().join("key");
    std::fs::write(&p, KEY).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    let vars = [("LLM_PROVIDER", "anthropic"), ("LLM_MODEL", "claude-test"), ("LLM_API_KEY_FILE", p.to_str().unwrap()), ("LLM_BASE_URL", base)];
    Arc::new(Llm::new(LlmConfig::from_vars(|k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())).unwrap().unwrap()))
}

/// The user message of the nth request the mock saw.
fn user_text(seen: &Seen, n: usize) -> String {
    seen.lock().unwrap()[n]["messages"][0]["content"].as_str().unwrap().to_string()
}

async fn send(app: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<(&str, String)>) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", t);
    }
    let body = match body {
        Some((ct, b)) => {
            req = req.header("content-type", ct);
            Body::from(b)
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let loc = resp.headers().get("location").map(|v| v.to_str().unwrap().to_string());
    (status, loc, String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
}

async fn api_json(app: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let (s, _, t) = send(app, method, uri, token, body.map(|b| ("application/json", b.to_string()))).await;
    (s, serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

async fn app_with(db: &Db, svc: ImportService) -> Router {
    common::sign_in(db).await;
    api::router_with(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, svc)
}

// S31
const PAGE: &str = "<html><head><title>T</title><style>.x{color:red}</style><script>var SECRETSCRIPT=1</script></head>\
<body><h1>Stew page</h1><p>Mix the <b>flour</b>.</p><noscript>NOSCRIPTTEXT</noscript></body></html>";

const LD: &str = r#"<html><head><script type="application/ld+json">{"@type":"Recipe","name":"Real LD","recipeIngredient":["1 egg"]}</script></head></html>"#;

async fn pages() -> String {
    let long = format!("<html><body><p>{}</p></body></html>", "a".repeat(40_000));

    let app = Router::new()
        .route("/r", get(|| async { PAGE }))
        .route("/long", get(move || { let l = long.clone(); async move { l } }))
        .route("/ld", get(|| async { LD }))
        .route("/img.jpg", get(|| async { vec![0xffu8, 0xd8, 0xff, 0xe0, 1, 2, 3] }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

fn llm_service(llm: &Arc<Llm>) -> ImportService {
    ImportService::new(Fetcher::with_allow(loopback), Chain::build_with("jsonld,microdata,llm", None, Some(llm.clone())).unwrap()).with_llm(llm.clone())
}

#[test]
fn the_llm_importer_needs_an_llm() {
    let e = Chain::build("llm", None).err().unwrap();
    assert!(e.contains("llm") && e.contains("LLM_PROVIDER"), "{e}");
}

#[tokio::test]
async fn llm_importer_turns_page_text_into_a_recipe_and_ignores_urls_in_the_output() {
    let site = pages().await;
    let ld = json!({"@type": "Recipe", "name": "Mock Stew", "description": "Warm.", "url": "http://evil.example/steal", "recipeIngredient": ["2 cups flour"], "recipeInstructions": ["Stir."], "image": format!("{site}/img.jpg")});
    let (base, seen) = mock(vec![(200, reply(ld))]).await;
    let db = Db::open_memory().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    common::sign_in(&db).await;
    let app = api::router_with(api::AppState { db: db.clone(), images_dir: dir.path().to_path_buf() }, llm_service(&llm(&base)));
    let (s, v) = api_json(&app, "POST", "/api/recipes/import", Some(common::BEARER), Some(json!({"url": format!("{site}/r")}))).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!((v["name"].as_str(), v["description"].as_str()), (Some("Mock Stew"), Some("Warm.")));
    assert_eq!(v["source_url"], format!("{site}/r"));
    assert_eq!(v["steps"], json!(["Stir."]));
    assert_eq!(v["ingredients"][0]["original_text"], "2 cups flour");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "image fetched through the fetcher");

    let text = user_text(&seen, 0);
    assert!(text.contains("Stew page") && text.contains("Mix the flour"), "{text}");
    for hidden in ["SECRETSCRIPT", "color:red", "NOSCRIPTTEXT", "<h1>"] {
        assert!(!text.contains(hidden), "{hidden} leaked into the prompt");
    }
    assert!(seen.lock().unwrap()[0]["system"].as_str().unwrap().contains("Recipe"));
}

#[tokio::test]
async fn llm_importer_caps_the_page_text_and_rejects_non_recipes() {
    let site = pages().await;
    let (base, seen) = mock(vec![(200, reply(json!({})))]).await;
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, llm_service(&llm(&base))).await;
    let (s, v) = api_json(&app, "POST", "/api/recipes/import", Some(common::BEARER), Some(json!({"url": format!("{site}/long")}))).await;
    assert_eq!((s.as_u16(), v["error"].as_str().unwrap()), (422, "no recipe found on that page"));
    assert_eq!(user_text(&seen, 0).chars().count(), 30_000);
}

#[tokio::test]
async fn llm_importer_is_not_asked_when_jsonld_already_found_a_recipe() {
    let site = pages().await;
    let (base, seen) = mock(vec![(500, "{}".into())]).await;
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, llm_service(&llm(&base))).await;
    let (s, v) = api_json(&app, "POST", "/api/recipes/import", Some(common::BEARER), Some(json!({"url": format!("{site}/ld")}))).await;
    assert_eq!((s.as_u16(), v["name"].as_str()), (201, Some("Real LD")));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn llm_failure_during_import_is_a_clean_import_error() {
    let site = pages().await;
    let (base, _) = mock(vec![(500, format!("boom {KEY}"))]).await;
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, llm_service(&llm(&base))).await;
    let (s, v) = api_json(&app, "POST", "/api/recipes/import", Some(common::BEARER), Some(json!({"url": format!("{site}/r")}))).await;
    assert_eq!((s.as_u16(), v["error"].as_str().unwrap()), (422, "no recipe found on that page"));
    assert!(!v.to_string().contains(KEY));
}

// S32
fn line(t: &str) -> IngredientInput {
    IngredientInput { original_text: t.into(), ..Default::default() }
}

async fn tables() -> (Db, i64, i64) {
    let db = Db::open_memory().await.unwrap();
    let cup = db.add_unit("cup", Dimension::Volume, Some(236.588)).await.unwrap();
    let flour = db.add_food("flour").await.unwrap();
    (db, cup, flour)
}

#[tokio::test]
async fn fallback_fills_only_unparsed_lines_by_index_and_keeps_unknown_names_as_note() {
    let (db, cup, flour) = tables().await;
    let answer = json!({"lines": [
        {"index": 1, "qty_min": 2, "qty_max": 2, "unit": "cup", "food": "sugar", "note": ""},
        {"index": 2, "qty_min": 1, "qty_max": null, "unit": "handful", "food": "Flour", "note": "heaped"},
        {"index": 0, "qty_min": 99, "qty_max": 99, "unit": "cup", "food": "flour", "note": "rule-parsed lines are not the model's to change"},
        {"index": 42, "qty_min": 5, "qty_max": 5, "unit": null, "food": null, "note": "out of range"},
    ]});
    let (base, seen) = mock(vec![(200, reply(answer))]).await;
    let (units, foods) = (db.units().await.unwrap(), db.foods().await.unwrap());
    let mut lines = vec![line("2 cups flour"), line("two cups sugar"), line("a handful of zorblax"), line("1 cup flour")];
    // As parsed_input leaves them: rules fill 0 and 3; 1 and 2 are untouched.
    for i in [0, 3] {
        let p = mealier::parse_line(&lines[i].original_text.clone(), &units, &foods);
        lines[i] = IngredientInput { qty_min: p.qty_min, qty_max: p.qty_max, unit_id: p.unit_id, food_id: p.food_id, note: p.note, original_text: p.original_text };
    }
    let filled = fill_unparsed(&llm(&base), &mut lines, &units, &foods).await;
    assert_eq!(filled, [1, 2]);
    assert_eq!((lines[1].qty_min, lines[1].qty_max, lines[1].unit_id, lines[1].food_id, lines[1].note.as_str()), (Some(2.0), Some(2.0), Some(cup), None, "sugar"));
    assert_eq!((lines[2].qty_min, lines[2].qty_max, lines[2].unit_id, lines[2].food_id, lines[2].note.as_str()), (Some(1.0), Some(1.0), None, Some(flour), "handful, heaped"));
    assert_eq!((lines[0].qty_min, lines[3].qty_min), (Some(2.0), Some(1.0)));
    assert_eq!(lines[1].original_text, "two cups sugar");

    // One request, only the unparsed lines, numbered by position.
    assert_eq!(seen.lock().unwrap().len(), 1);
    let text = user_text(&seen, 0);
    assert!(text.contains("1: two cups sugar") && text.contains("2: a handful of zorblax"), "{text}");
    assert!(!text.contains("2 cups flour") && !text.contains("1 cup flour"), "{text}");
    // Never auto-creates.
    assert_eq!((db.units().await.unwrap().len(), db.foods().await.unwrap().len()), (units.len(), foods.len()));
}

#[tokio::test]
async fn fallback_leaves_lines_alone_when_the_llm_fails_or_has_nothing_to_do() {
    let (db, ..) = tables().await;
    let (units, foods) = (db.units().await.unwrap(), db.foods().await.unwrap());
    let (base, seen) = mock(vec![(500, "{}".into())]).await;
    let mut lines = vec![line("a handful of zorblax")];
    assert!(fill_unparsed(&llm(&base), &mut lines, &units, &foods).await.is_empty());
    assert_eq!((lines[0].qty_min, lines[0].note.as_str()), (None, ""));
    assert_eq!(seen.lock().unwrap().len(), 1);

    let mut done = vec![IngredientInput { qty_min: Some(1.0), original_text: "1 egg".into(), ..Default::default() }];
    assert!(fill_unparsed(&llm(&base), &mut done, &units, &foods).await.is_empty());
    assert_eq!(seen.lock().unwrap().len(), 1, "no unparsed lines, no request");
}

#[tokio::test]
async fn bad_numbers_from_the_model_are_dropped() {
    let (db, ..) = tables().await;
    let (units, foods) = (db.units().await.unwrap(), db.foods().await.unwrap());
    let answer = json!({"lines": [{"index": 0, "qty_min": -3, "qty_max": "lots", "unit": null, "food": "zorblax", "note": ""}]});
    let (base, _) = mock(vec![(200, reply(answer))]).await;
    let mut lines = vec![line("zorblax")];
    fill_unparsed(&llm(&base), &mut lines, &units, &foods).await;
    assert_eq!((lines[0].qty_min, lines[0].qty_max), (None, None));
}

#[tokio::test]
async fn form_save_uses_the_fallback_and_the_review_screen_marks_suggested_lines() {
    let (db, ..) = tables().await;
    let answer = json!({"lines": [{"index": 1, "qty_min": 1, "qty_max": 1, "unit": "handful", "food": "zorblax", "note": ""}]});
    let (base, _) = mock(vec![(200, reply(answer))]).await;
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let form = "name=Z&description=&ingredients=2+cups+flour%0Aa+handful+of+zorblax";
    let (s, loc, _) = send(&app, "POST", "/recipes", Some(common::BEARER), Some(("application/x-www-form-urlencoded", form.into()))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let loc = loc.unwrap();
    assert_eq!(loc, "/recipes/1/ingredients/review?suggested=1");
    let (s, _, html) = send(&app, "GET", &loc, Some(common::BEARER), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(html.matches(">suggested<").count(), 1, "{html}");
    assert!(html.contains("handful zorblax"));

    // Without an LLM nothing is suggested.
    let app = app_with(&db, ImportService::default()).await;
    let (_, loc, _) = send(&app, "POST", "/recipes", Some(common::BEARER), Some(("application/x-www-form-urlencoded", form.into()))).await;
    assert_eq!(loc.unwrap(), "/recipes/2/ingredients/review");
}

// S33
async fn stew(db: &Db) -> i64 {
    let r = db
        .create_recipe(RecipeInput { name: "Lentil Stew".into(), ingredients: vec![IngredientInput { qty_min: Some(1.0), original_text: "1 cup lentils".into(), ..Default::default() }], ..Default::default() })
        .await
        .unwrap();
    r.id
}

async fn snapshots(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM recipe_snapshots").fetch_one(db.pool()).await.unwrap()
}

#[tokio::test]
async fn nutrition_estimate_returns_numbers_and_saves_nothing() {
    let db = Db::open_memory().await.unwrap();
    let id = stew(&db).await;
    let (base, seen) = mock(vec![(200, reply(json!({"calories": 420.5, "protein": "21 g", "fat": 12, "carbs": 55})))]).await;
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let before = snapshots(&db).await;
    let (s, v) = api_json(&app, "POST", &format!("/api/recipes/{id}/nutrition/estimate"), Some(common::BEARER), None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, json!({"calories": 420.5, "protein": 21.0, "fat": 12.0, "carbs": 55.0}));
    assert_eq!(snapshots(&db).await, before);
    assert_eq!(db.get_recipe(id).await.unwrap().unwrap().content.nutrition, None);
    let text = user_text(&seen, 0);
    assert!(text.contains("Lentil Stew") && text.contains("1 cup lentils"), "{text}");
}

#[tokio::test]
async fn nutrition_estimate_errors() {
    let db = Db::open_memory().await.unwrap();
    let id = stew(&db).await;
    let (base, seen) = mock(vec![(200, reply(json!({"calories": 1, "protein": 1, "fat": 1, "carbs": 1})))]).await;
    let none = app_with(&db, ImportService::default()).await;
    let (s, v) = api_json(&none, "POST", &format!("/api/recipes/{id}/nutrition/estimate"), Some(common::BEARER), None).await;
    assert_eq!((s.as_u16(), v["error"].as_str().unwrap().contains("LLM")), (422, true));
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let (s, _) = api_json(&app, "POST", "/api/recipes/999/nutrition/estimate", Some(common::BEARER), None).await;
    assert_eq!(s, 404);
    assert!(seen.lock().unwrap().is_empty(), "unknown recipe must not cost a request");
    let (s, _) = api_json(&app, "POST", &format!("/api/recipes/{id}/nutrition/estimate"), None, None).await;
    assert_eq!(s, 401);
}

#[tokio::test]
async fn edit_page_has_nutrition_fields_and_an_estimate_button_that_fills_them() {
    let db = Db::open_memory().await.unwrap();
    let id = stew(&db).await;
    let (base, _) = mock(vec![(200, reply(json!({"calories": 420.5, "protein": 21, "fat": 12, "carbs": 55})))]).await;
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let (_, _, page) = send(&app, "GET", &format!("/recipes/{id}/edit"), Some(common::BEARER), None).await;
    assert!(page.contains(&format!("hx-post=\"/recipes/{id}/nutrition/estimate\"")) && page.contains("name=\"calories\""), "{page}");

    let before = snapshots(&db).await;
    let (s, _, frag) = send(&app, "POST", &format!("/recipes/{id}/nutrition/estimate"), Some(common::BEARER), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(frag.contains("value=\"420.5\"") && frag.contains("name=\"protein_g\"") && frag.contains("id=\"nutrition\""), "{frag}");
    assert!(!frag.contains("<html"), "a fragment, not a page");
    assert_eq!(snapshots(&db).await, before, "the estimate is not saved");

    // Saving the form stores the fields; a form without them keeps what was there.
    let form = "name=Lentil+Stew&description=&ingredients=1+cup+lentils&calories=420.5&protein_g=21&fat_g=&carbs_g=55";
    let (s, ..) = send(&app, "POST", &format!("/recipes/{id}/edit"), Some(common::BEARER), Some(("application/x-www-form-urlencoded", form.into()))).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let n = db.get_recipe(id).await.unwrap().unwrap().content.nutrition.unwrap();
    assert_eq!((n.calories, n.protein_g, n.fat_g, n.carbs_g), (Some(420.5), Some(21.0), None, Some(55.0)));
    let form = "name=Lentil+Stew&description=&ingredients=1+cup+lentils";
    send(&app, "POST", &format!("/recipes/{id}/edit"), Some(common::BEARER), Some(("application/x-www-form-urlencoded", form.into()))).await;
    assert_eq!(db.get_recipe(id).await.unwrap().unwrap().content.nutrition.unwrap().calories, Some(420.5));
}

// S34
async fn user_token(db: &Db) -> String {
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role) VALUES ('plain', 'plain@x', 'user') RETURNING id").fetch_one(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO api_tokens (hash, user_id, name) VALUES (?, ?, 'plain')").bind(mealier::hash_token("mlr_plain-user")).bind(id).execute(db.pool()).await.unwrap();
    "Bearer mlr_plain-user".into()
}

#[tokio::test]
async fn llm_settings_are_admin_only() {
    let db = Db::open_memory().await.unwrap();
    let (base, seen) = mock(vec![(200, reply(json!({"ok": true})))]).await;
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let plain = user_token(&db).await;
    for (m, uri) in [("GET", "/api/llm"), ("POST", "/api/llm/test"), ("GET", "/settings/llm"), ("POST", "/settings/llm/test")] {
        let (s, ..) = send(&app, m, uri, Some(&plain), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {uri} as user");
        let (s, ..) = send(&app, m, uri, None, None).await;
        assert!(matches!(s, StatusCode::UNAUTHORIZED | StatusCode::SEE_OTHER), "{m} {uri} anonymous: {s}");
    }
    assert!(seen.lock().unwrap().is_empty(), "a refused test must not reach the provider");
    // Estimating is for every signed-in user.
    let id = stew(&db).await;
    let (s, _) = api_json(&app, "POST", &format!("/api/recipes/{id}/nutrition/estimate"), Some(&plain), None).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn api_llm_reports_provider_and_model_but_never_the_key() {
    let db = Db::open_memory().await.unwrap();
    let (base, _) = mock(vec![(200, reply(json!({"ok": true})))]).await;
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let (s, v) = api_json(&app, "GET", "/api/llm", Some(common::BEARER), None).await;
    assert_eq!((s.as_u16(), &v), (200, &json!({"provider": "anthropic", "model": "claude-test", "enabled": true})));
    let off = app_with(&db, ImportService::default()).await;
    let (_, v) = api_json(&off, "GET", "/api/llm", Some(common::BEARER), None).await;
    assert_eq!(v, json!({"provider": null, "model": null, "enabled": false}));
    let (s, v) = api_json(&off, "POST", "/api/llm/test", Some(common::BEARER), None).await;
    assert_eq!((s.as_u16(), v["ok"].clone()), (200, json!(false)));
}

#[tokio::test]
async fn api_llm_test_sends_a_tiny_prompt_and_reports_ok_or_the_error() {
    let db = Db::open_memory().await.unwrap();
    let (base, seen) = mock(vec![(200, reply(json!({"ok": true}))), (401, format!("denied {KEY}"))]).await;
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let (s, v) = api_json(&app, "POST", "/api/llm/test", Some(common::BEARER), None).await;
    assert_eq!((s.as_u16(), &v), (200, &json!({"ok": true, "error": null})));
    assert!(user_text(&seen, 0).len() < 200);
    let (s, v) = api_json(&app, "POST", "/api/llm/test", Some(common::BEARER), None).await;
    assert_eq!((s.as_u16(), v["ok"].clone()), (200, json!(false)));
    let e = v["error"].as_str().unwrap();
    assert!(e.contains("401") && !e.contains(KEY), "{e}");
}

#[tokio::test]
async fn settings_page_shows_provider_and_model_and_a_test_button() {
    let db = Db::open_memory().await.unwrap();
    let (base, _) = mock(vec![(200, reply(json!({"ok": true}))), (500, "{}".into())]).await;
    let app = app_with(&db, ImportService::default().with_llm(llm(&base))).await;
    let (s, _, html) = send(&app, "GET", "/settings/llm", Some(common::BEARER), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(html.contains("anthropic") && html.contains("claude-test") && html.contains("action=\"/settings/llm/test\""), "{html}");
    assert!(!html.contains(KEY));
    let (_, _, html) = send(&app, "POST", "/settings/llm/test", Some(common::BEARER), None).await;
    assert!(html.contains("Test passed"), "{html}");
    let (_, _, html) = send(&app, "POST", "/settings/llm/test", Some(common::BEARER), None).await;
    assert!(html.contains("Test failed") && html.contains("500") && !html.contains(KEY), "{html}");
    let off = app_with(&db, ImportService::default()).await;
    let (_, _, html) = send(&off, "GET", "/settings/llm", Some(common::BEARER), None).await;
    assert!(html.contains("not configured"), "{html}");
}
