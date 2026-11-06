//! One mapping from errors to responses: JSON under /api, a page everywhere else. The path decides, never `Accept`.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{Db, api};
use tower::ServiceExt;

async fn app() -> Router {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() })
}

async fn call(app: &Router, method: &str, uri: &str, accept: Option<&str>, form: Option<&str>) -> (StatusCode, String, String) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", common::BEARER);
    if let Some(a) = accept {
        req = req.header("accept", a);
    }
    if form.is_some() {
        req = req.header("content-type", "application/x-www-form-urlencoded");
    }
    let resp = app.clone().oneshot(req.body(Body::from(form.unwrap_or_default().to_string())).unwrap()).await.unwrap();
    let status = resp.status();
    let ct = resp.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    (status, ct, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

#[tokio::test]
async fn a_missing_thing_on_a_page_is_an_html_page_with_the_message() {
    let (status, ct, body) = call(&app().await, "GET", "/recipes/999", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(ct.starts_with("text/html"), "{ct}");
    assert!(body.contains("recipe not found"), "{body}");
    assert!(body.contains("404"), "the status is on the page");
    assert!(body.contains("Back") && body.contains("href=\"/plan\""), "a way back and the navigation: {body}");
}

#[tokio::test]
async fn a_missing_thing_under_api_stays_json() {
    let (status, ct, body) = call(&app().await, "GET", "/api/recipes/999", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(ct.starts_with("application/json"), "{ct}");
    assert_eq!(body, r#"{"error":"recipe not found"}"#);
}

#[tokio::test]
async fn the_path_decides_not_the_accept_header() {
    let app = app().await;
    let (_, ct, body) = call(&app, "GET", "/api/recipes/999", Some("text/html"), None).await;
    assert!(ct.starts_with("application/json") && body.starts_with('{'), "{ct} {body}");
    let (_, ct, body) = call(&app, "GET", "/recipes/999", Some("application/json"), None).await;
    assert!(ct.starts_with("text/html") && body.contains("<html"), "{ct} {body}");
}

#[tokio::test]
async fn other_statuses_get_pages_too_and_the_message_is_escaped() {
    let app = app().await;
    // 422 from a form, with text the user typed in the message.
    let (status, ct, body) = call(&app, "POST", "/plan/entries", None, Some("date=%3Cb%3Ex%3C%2Fb%3E&slot=dinner&title=x")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(ct.starts_with("text/html"), "{ct}");
    assert!(body.contains("422") && body.contains("YYYY-MM-DD"), "{body}");
    let (status, _, body) = call(&app, "POST", "/recipes/1/batches", None, Some("scale=%3Cb%3Ex%3C%2Fb%3E")).await;
    assert!(status.is_client_error());
    assert!(!body.contains("<b>x</b>"), "user text must be escaped: {body}");
}

#[tokio::test]
async fn forbidden_pages_are_pages_and_unauthenticated_pages_still_redirect_to_the_login() {
    let db = Db::open_memory().await.unwrap();
    common::sign_in(&db).await;
    let uid: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES ('plain', 'plain@x', 'user', ?) RETURNING id").bind(common::household(&db).await).fetch_one(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO api_tokens (hash, user_id, name) VALUES (?, ?, 't')").bind(mealier::hash_token("mlr_plain")).bind(uid).execute(db.pool()).await.unwrap();
    let app = api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() });

    let req = |uri: &str, auth: Option<&str>| {
        let mut r = Request::builder().uri(uri);
        if let Some(a) = auth {
            r = r.header("authorization", a);
        }
        r.body(Body::empty()).unwrap()
    };
    let resp = app.clone().oneshot(req("/settings/webhooks", Some("Bearer mlr_plain"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let body = String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned();
    assert!(body.contains("admins only"), "{body}");

    let resp = app.clone().oneshot(req("/recipes/1", None)).await.unwrap();
    assert_eq!((resp.status(), resp.headers()["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/login"));
    let resp = app.clone().oneshot(req("/api/recipes", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("application/json"));
}
