use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mealier::{Db, Error, WebhookInput, api};
use tower::ServiceExt;

fn input(name: &str) -> WebhookInput {
    WebhookInput {
        name: name.into(),
        kind: "json".into(),
        url: "http://127.0.0.1:9/hook".into(),
        events: vec!["batch_logged".into()],
        ntfy_tags: String::new(),
        enabled: true,
    }
}

// S22
#[tokio::test]
async fn create_then_get_and_list_round_trips_every_field() {
    let db = Db::open_memory().await.unwrap();
    let mut i = input("phone");
    i.kind = "ntfy".into();
    i.events = vec!["import_failed".into(), "batch_logged".into()];
    i.ntfy_tags = "cooking,tada".into();
    let w = db.create_webhook(i).await.unwrap();
    assert_eq!((w.name.as_str(), w.kind.as_str(), w.ntfy_tags.as_str()), ("phone", "ntfy", "cooking,tada"));
    assert_eq!(w.events, ["import_failed", "batch_logged"]);
    assert!(w.enabled && !w.created_at.is_empty());
    assert_eq!(db.get_webhook(w.id).await.unwrap().url, "http://127.0.0.1:9/hook");
    assert_eq!(db.list_webhooks().await.unwrap().len(), 1);
}

#[tokio::test]
async fn update_replaces_fields_including_enabled() {
    let db = Db::open_memory().await.unwrap();
    let w = db.create_webhook(input("a")).await.unwrap();
    let mut i = input("b");
    i.enabled = false;
    i.url = "https://example.test/x".into();
    let u = db.update_webhook(w.id, i).await.unwrap();
    assert_eq!((u.name.as_str(), u.url.as_str(), u.enabled), ("b", "https://example.test/x", false));
    assert!(matches!(db.update_webhook(99, input("c")).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn delete_removes_and_a_second_delete_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    let w = db.create_webhook(input("a")).await.unwrap();
    db.delete_webhook(w.id).await.unwrap();
    assert!(db.list_webhooks().await.unwrap().is_empty());
    assert!(matches!(db.delete_webhook(w.id).await, Err(Error::NotFound(_))));
    assert!(matches!(db.get_webhook(w.id).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn validation_rejects_bad_url_kind_events_and_name() {
    let db = Db::open_memory().await.unwrap();
    for bad in ["ftp://x.test/", "not a url", "javascript:alert(1)", ""] {
        let mut i = input("a");
        i.url = bad.into();
        assert!(matches!(db.create_webhook(i).await, Err(Error::Validation(_))), "{bad}");
    }
    let mut i = input("a");
    i.kind = "slack".into();
    assert!(matches!(db.create_webhook(i).await, Err(Error::Validation(_))));
    let mut i = input("a");
    i.events = vec![];
    assert!(matches!(db.create_webhook(i).await, Err(Error::Validation(_))));
    let mut i = input("a");
    i.events = vec!["batch_logged".into(), "nope".into()];
    assert!(matches!(db.create_webhook(i).await, Err(Error::Validation(_))));
    let mut i = input("  ");
    i.name = "  ".into();
    assert!(matches!(db.create_webhook(i).await, Err(Error::Validation(_))));
    assert!(db.list_webhooks().await.unwrap().is_empty());
}

// ---- S23: delivery ----
mod common;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use mealier::{ImportService, Notifier, RecipeInput};
use serde_json::{Value, json};

#[derive(Clone)]
struct Hit {
    path: String,
    headers: HeaderMap,
    body: String,
}

#[derive(Clone)]
struct Rx {
    hits: Arc<Mutex<Vec<Hit>>>,
    status: u16,
    delay: Duration,
}

/// A receiver on 127.0.0.1 that records every request. Returns its base URL.
async fn receiver(status: u16, delay: Duration) -> (String, Arc<Mutex<Vec<Hit>>>) {
    let rx = Rx { hits: Default::default(), status, delay };
    let hits = rx.hits.clone();
    async fn h(State(rx): State<Rx>, uri: axum::http::Uri, headers: HeaderMap, body: String) -> StatusCode {
        rx.hits.lock().unwrap().push(Hit { path: uri.path().into(), headers, body });
        tokio::time::sleep(rx.delay).await;
        StatusCode::from_u16(rx.status).unwrap()
    }
    let app = Router::new().route("/{*p}", post(h)).with_state(rx);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, hits)
}

/// Waits (up to 3 s) for `n` requests, then a beat more so extras would show.
async fn hits(h: &Arc<Mutex<Vec<Hit>>>, n: usize) -> Vec<Hit> {
    let end = Instant::now() + Duration::from_secs(3);
    while h.lock().unwrap().len() < n && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.lock().unwrap().clone()
}

fn notifier(base: Option<&str>) -> Notifier {
    Notifier::new(Duration::from_millis(500), base.map(String::from))
}

async fn hook(db: &Db, url: &str, kind: &str, events: &[&str], enabled: bool) {
    let mut i = input("t");
    i.kind = kind.into();
    i.url = url.into();
    i.events = events.iter().map(|e| e.to_string()).collect();
    i.ntfy_tags = "cooking,tada".into();
    i.enabled = enabled;
    db.create_webhook(i).await.unwrap();
}

/// A recipe named "Soup" with a done batch rated 4; returns the batch id.
async fn finished_batch(db: &Db) -> (i64, i64) {
    let r = db.create_recipe(&common::scope(db).await, RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let b = db.start_batch(&common::scope(db).await, r.id, 1.0).await.unwrap();
    db.update_batch(&common::scope(db).await, b.batch.id, mealier::BatchInput { made_on: Some("2026-11-01".into()), scale: 1.0, rating: Some(4), ..Default::default() }).await.unwrap();
    db.finish_batch(&common::scope(db).await, b.batch.id).await.unwrap();
    (b.batch.id, r.id)
}

#[tokio::test]
async fn json_webhook_gets_batch_logged_with_literal_fields() {
    let (url, h) = receiver(200, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(Some("https://meals.test/")));
    hook(&db, &format!("{url}/in"), "json", &["batch_logged"], true).await;
    let (bid, rid) = finished_batch(&db).await;

    let got = hits(&h, 1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].path, "/in");
    assert_eq!(got[0].headers["content-type"], "application/json");
    let v: Value = serde_json::from_str(&got[0].body).unwrap();
    assert_eq!(v["event"], "batch_logged");
    assert!(v["occurred_at"].as_str().unwrap().ends_with('Z'), "{v}");
    assert_eq!(
        v["data"],
        json!({"batch_id": bid, "recipe_id": rid, "recipe_name": "Soup", "rating": 4, "made_on": "2026-11-01", "url": format!("https://meals.test/batches/{bid}")})
    );
}

#[tokio::test]
async fn json_webhook_gets_import_failed_when_an_import_fails() {
    let (url, h) = receiver(200, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(None));
    hook(&db, &format!("{url}/in"), "json", &["import_failed"], true).await;

    let err = ImportService::default().import(&db, &common::scope(&db).await, &tempfile::tempdir().unwrap().keep(), "http://127.0.0.1:1/r").await.unwrap_err();

    let got = hits(&h, 1).await;
    assert_eq!(got.len(), 1);
    let v: Value = serde_json::from_str(&got[0].body).unwrap();
    assert_eq!(v["event"], "import_failed");
    assert_eq!(v["data"], json!({"url": "http://127.0.0.1:1/r", "reason": err.to_string()}));
}

#[tokio::test]
async fn ntfy_webhook_gets_title_tags_click_and_text_body() {
    let (url, h) = receiver(200, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(Some("https://meals.test")));
    hook(&db, &format!("{url}/mealier"), "ntfy", &["batch_logged", "import_failed"], true).await;
    let (bid, _) = finished_batch(&db).await;
    let _ = ImportService::default().import(&db, &common::scope(&db).await, &tempfile::tempdir().unwrap().keep(), "http://127.0.0.1:1/r").await;

    let got = hits(&h, 2).await;
    assert_eq!(got.len(), 2);
    let made = got.iter().find(|g| g.headers["title"] == "Made: Soup").expect("batch_logged");
    assert_eq!(made.body, "Rated 4/5");
    assert_eq!(made.headers["tags"], "cooking,tada");
    assert_eq!(made.headers["click"], format!("https://meals.test/batches/{bid}").as_str());
    let failed = got.iter().find(|g| g.headers["title"] == "Import failed").expect("import_failed");
    assert!(failed.body.starts_with("http://127.0.0.1:1/r: "), "{}", failed.body);
    assert_eq!(failed.headers["tags"], "cooking,tada");
}

#[tokio::test]
async fn ntfy_without_base_url_omits_click_and_unrated_says_logged() {
    let (url, h) = receiver(200, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(None));
    hook(&db, &format!("{url}/m"), "ntfy", &["batch_logged"], true).await;
    let r = db.create_recipe(&common::scope(&db).await, RecipeInput { name: "Stew".into(), ..Default::default() }).await.unwrap();
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    db.finish_batch(&common::scope(&db).await, b.batch.id).await.unwrap();

    let got = hits(&h, 1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, "Logged");
    assert!(got[0].headers.get("click").is_none());
}

#[tokio::test]
async fn disabled_webhooks_and_unsubscribed_events_are_not_called() {
    let (url, h) = receiver(200, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(None));
    hook(&db, &format!("{url}/off"), "json", &["batch_logged"], false).await;
    hook(&db, &format!("{url}/other"), "json", &["import_failed"], true).await;
    hook(&db, &format!("{url}/on"), "json", &["batch_logged"], true).await;
    finished_batch(&db).await;

    let got = hits(&h, 1).await;
    assert_eq!(got.iter().map(|g| g.path.as_str()).collect::<Vec<_>>(), ["/on"]);
}

#[tokio::test]
async fn a_failing_or_hanging_receiver_never_fails_or_delays_the_request() {
    for (status, delay) in [(500, Duration::ZERO), (200, Duration::from_secs(30))] {
        let (url, h) = receiver(status, delay).await;
        let db = Db::open_memory().await.unwrap().with_notifier(notifier(None));
        hook(&db, &format!("{url}/x"), "json", &["batch_logged", "import_failed"], true).await;

        let t = Instant::now();
        finished_batch(&db).await; // unwraps: finish_batch must succeed
        let r = ImportService::default().import(&db, &common::scope(&db).await, &tempfile::tempdir().unwrap().keep(), "http://127.0.0.1:1/r").await;
        assert!(r.is_err(), "the import's own error comes through");
        assert!(t.elapsed() < Duration::from_secs(1), "took {:?} with status {status} delay {delay:?}", t.elapsed());
        assert_eq!(hits(&h, 2).await.len(), 2, "both deliveries were attempted");
    }
}

#[tokio::test]
async fn send_test_delivers_to_a_disabled_hook_whatever_its_filter_and_reports_status() {
    let (url, h) = receiver(204, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(None));
    hook(&db, &format!("{url}/t"), "json", &["import_failed"], false).await;
    let id = db.list_webhooks().await.unwrap()[0].id;

    assert_eq!(db.send_test(id).await.unwrap(), 204);
    let v: Value = serde_json::from_str(&hits(&h, 1).await[0].body).unwrap();
    assert_eq!(v["event"], "test");
    assert!(matches!(db.send_test(99).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn ntfy_title_keeps_utf8_and_turns_control_characters_into_spaces() {
    let (url, h) = receiver(200, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(None));
    hook(&db, &format!("{url}/m"), "ntfy", &["batch_logged"], true).await;
    let r = db.create_recipe(&common::scope(&db).await, RecipeInput { name: "Crème\nbrûlée".into(), ..Default::default() }).await.unwrap();
    let b = db.start_batch(&common::scope(&db).await, r.id, 1.0).await.unwrap();
    db.finish_batch(&common::scope(&db).await, b.batch.id).await.unwrap();

    let got = hits(&h, 1).await;
    assert_eq!(got.len(), 1, "a recipe name must not break delivery");
    assert_eq!(got[0].headers["title"].as_bytes(), "Made: Crème brûlée".as_bytes());
}

// ---- S24: HTTP ----
async fn app(db: Db) -> Router {
    common::sign_in(&db).await;
    api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() })
}

async fn call(app: &Router, method: &str, uri: &str, json: Option<Value>, form: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", common::BEARER);
    let body = if let Some(v) = json {
        req = req.header("content-type", "application/json");
        Body::from(v.to_string())
    } else if let Some(f) = form {
        req = req.header("content-type", "application/x-www-form-urlencoded");
        Body::from(f.to_string())
    } else {
        Body::empty()
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn body(url: &str) -> Value {
    json!({"name": "phone", "kind": "json", "url": url, "events": ["batch_logged"]})
}

#[tokio::test]
async fn api_post_creates_201_with_enabled_default_and_bad_input_is_422() {
    let app = app(Db::open_memory().await.unwrap()).await;
    let (s, _, b) = call(&app, "POST", "/api/webhooks", Some(body("http://127.0.0.1:9/x")), None).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!((v["name"].as_str(), v["enabled"].as_bool()), (Some("phone"), Some(true)));
    let (s, _, b) = call(&app, "POST", "/api/webhooks", Some(body("ftp://x")), None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(b.contains("url"), "{b}");
}

#[tokio::test]
async fn api_get_list_and_get_one_and_missing_is_404() {
    let app = app(Db::open_memory().await.unwrap()).await;
    let (_, _, b) = call(&app, "POST", "/api/webhooks", Some(body("http://127.0.0.1:9/x")), None).await;
    let id = serde_json::from_str::<Value>(&b).unwrap()["id"].as_i64().unwrap();
    let (s, _, b) = call(&app, "GET", "/api/webhooks", None, None).await;
    assert_eq!((s, serde_json::from_str::<Value>(&b).unwrap().as_array().unwrap().len()), (StatusCode::OK, 1));
    let (s, _, b) = call(&app, "GET", &format!("/api/webhooks/{id}"), None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap()["id"], id);
    let (s, _, b) = call(&app, "GET", "/api/webhooks/99", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(b.contains("webhook not found"), "{b}");
}

#[tokio::test]
async fn api_put_replaces_200_missing_404_invalid_422() {
    let app = app(Db::open_memory().await.unwrap()).await;
    let (_, _, b) = call(&app, "POST", "/api/webhooks", Some(body("http://127.0.0.1:9/x")), None).await;
    let id = serde_json::from_str::<Value>(&b).unwrap()["id"].as_i64().unwrap();
    let mut upd = body("http://127.0.0.1:9/y");
    upd["enabled"] = json!(false);
    let (s, _, b) = call(&app, "PUT", &format!("/api/webhooks/{id}"), Some(upd.clone()), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v: Value = serde_json::from_str(&b).unwrap();
    assert_eq!((v["url"].as_str(), v["enabled"].as_bool()), (Some("http://127.0.0.1:9/y"), Some(false)));
    assert_eq!(call(&app, "PUT", "/api/webhooks/99", Some(upd), None).await.0, StatusCode::NOT_FOUND);
    let mut bad = body("http://127.0.0.1:9/y");
    bad["events"] = json!([]);
    assert_eq!(call(&app, "PUT", &format!("/api/webhooks/{id}"), Some(bad), None).await.0, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn api_delete_204_then_404() {
    let app = app(Db::open_memory().await.unwrap()).await;
    let (_, _, b) = call(&app, "POST", "/api/webhooks", Some(body("http://127.0.0.1:9/x")), None).await;
    let id = serde_json::from_str::<Value>(&b).unwrap()["id"].as_i64().unwrap();
    assert_eq!(call(&app, "DELETE", &format!("/api/webhooks/{id}"), None, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, "DELETE", &format!("/api/webhooks/{id}"), None, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_test_posts_a_test_event_200_and_reports_receiver_status() {
    let (url, h) = receiver(503, Duration::ZERO).await;
    let app = app(Db::open_memory().await.unwrap().with_notifier(notifier(None))).await;
    let (_, _, b) = call(&app, "POST", "/api/webhooks", Some(body(&format!("{url}/x"))), None).await;
    let id = serde_json::from_str::<Value>(&b).unwrap()["id"].as_i64().unwrap();
    let (s, _, b) = call(&app, "POST", &format!("/api/webhooks/{id}/test"), None, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap(), json!({"ok": false, "status": 503}));
    assert_eq!(hits(&h, 1).await.len(), 1);
}

#[tokio::test]
async fn api_test_missing_is_404_and_unreachable_receiver_is_422() {
    let app = app(Db::open_memory().await.unwrap().with_notifier(notifier(None))).await;
    assert_eq!(call(&app, "POST", "/api/webhooks/99/test", None, None).await.0, StatusCode::NOT_FOUND);
    let (_, _, b) = call(&app, "POST", "/api/webhooks", Some(body("http://127.0.0.1:1/x")), None).await;
    let id = serde_json::from_str::<Value>(&b).unwrap()["id"].as_i64().unwrap();
    let (s, _, b) = call(&app, "POST", &format!("/api/webhooks/{id}/test"), None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(b.contains("delivery failed"), "{b}");
}

#[tokio::test]
async fn page_lists_webhooks_and_escapes_names() {
    let db = Db::open_memory().await.unwrap();
    let mut i = input("<b>loud</b>");
    i.enabled = false;
    db.create_webhook(i).await.unwrap();
    let (s, _, b) = call(&app(db).await, "GET", "/settings/webhooks", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("&#60;b&#62;loud") && !b.contains("<b>loud"), "{b}");
}

#[tokio::test]
async fn page_create_redirects_and_stores_with_checked_events() {
    let db = Db::open_memory().await.unwrap();
    let app = app(db.clone()).await;
    let f = "name=ntfy+me&kind=ntfy&url=http%3A%2F%2F127.0.0.1%3A9%2Ft&ntfy_tags=a%2Cb&ev_import_failed=on&ev_batch_logged=on";
    let (s, h, b) = call(&app, "POST", "/settings/webhooks", None, Some(f)).await;
    assert_eq!(s, StatusCode::SEE_OTHER, "{b}");
    assert_eq!(h["location"], "/settings/webhooks");
    let w = &db.list_webhooks().await.unwrap()[0];
    assert_eq!((w.kind.as_str(), w.ntfy_tags.as_str(), w.enabled), ("ntfy", "a,b", true));
    assert_eq!(w.events, ["import_failed", "batch_logged"]);
}

#[tokio::test]
async fn page_create_without_events_is_422() {
    let app = app(Db::open_memory().await.unwrap()).await;
    let (s, ..) = call(&app, "POST", "/settings/webhooks", None, Some("name=x&kind=json&url=http%3A%2F%2Fa.test%2F")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn page_toggle_flips_enabled_and_delete_removes_missing_404() {
    let db = Db::open_memory().await.unwrap();
    let w = db.create_webhook(input("a")).await.unwrap();
    let app = app(db.clone()).await;
    let (s, h, _) = call(&app, "POST", &format!("/settings/webhooks/{}/toggle", w.id), None, Some("")).await;
    assert_eq!((s, h["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/settings/webhooks"));
    assert!(!db.get_webhook(w.id).await.unwrap().enabled);
    call(&app, "POST", &format!("/settings/webhooks/{}/toggle", w.id), None, Some("")).await;
    assert!(db.get_webhook(w.id).await.unwrap().enabled);
    let (s, ..) = call(&app, "POST", &format!("/settings/webhooks/{}/delete", w.id), None, Some("")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(db.list_webhooks().await.unwrap().is_empty());
    assert_eq!(call(&app, "POST", "/settings/webhooks/99/delete", None, Some("")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, "POST", "/settings/webhooks/99/toggle", None, Some("")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn page_send_test_redirects_with_the_outcome_and_the_page_shows_it() {
    let (url, h) = receiver(200, Duration::ZERO).await;
    let db = Db::open_memory().await.unwrap().with_notifier(notifier(None));
    let mut i = input("a");
    i.url = format!("{url}/x");
    let w = db.create_webhook(i).await.unwrap();
    let app = app(db).await;
    let (s, hd, _) = call(&app, "POST", &format!("/settings/webhooks/{}/test", w.id), None, Some("")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let loc = hd["location"].to_str().unwrap().to_string();
    assert!(loc.starts_with("/settings/webhooks?msg="), "{loc}");
    assert_eq!(hits(&h, 1).await.len(), 1);
    let (_, _, page) = call(&app, "GET", &loc, None, None).await;
    assert!(page.contains("200"), "{page}");
    assert_eq!(call(&app, "POST", "/settings/webhooks/99/test", None, Some("")).await.0, StatusCode::NOT_FOUND);
}
