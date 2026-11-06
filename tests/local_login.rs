//! S28: the break-glass local admin.
use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{AuthConfig, Db, Error, ImportService, api, hash_password};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

/// Hashing is deliberately slow; do it once for the whole file.
fn hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| hash_password(PASSWORD).unwrap())
}

fn app_with(db: &Db, cfg: AuthConfig) -> Router {
    api::router_with_auth(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, ImportService::default(), cfg)
}

fn enabled() -> AuthConfig {
    AuthConfig { admin_hash: Some(hash().to_string()), ..AuthConfig::default() }
}

async fn post(app: &Router, uri: &str, form: &str) -> (StatusCode, HeaderMap, String) {
    send(app, "POST", uri, &[("content-type", "application/x-www-form-urlencoded")], form).await
}

async fn send(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: &str) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let (status, h) = (resp.status(), resp.headers().clone());
    (status, h, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

fn login_form(password: &str) -> String {
    serde_urlencoded::to_string([("username", "admin"), ("password", password)]).unwrap()
}

async fn count(db: &Db, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(db.pool()).await.unwrap()
}

#[test]
fn hash_password_makes_an_argon2id_phc_string_and_refuses_short_passwords() {
    assert!(hash().starts_with("$argon2id$"), "{}", hash());
    assert_ne!(hash(), hash_password(PASSWORD).unwrap(), "salted");
    assert!(matches!(hash_password("short"), Err(Error::Validation(_))));
}

#[tokio::test]
async fn the_right_password_signs_in_the_local_admin() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, enabled());
    let (status, h, _) = post(&app, "/login/local", &login_form(PASSWORD)).await;
    assert_eq!((status, h["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/"));

    let set = h["set-cookie"].to_str().unwrap();
    for attr in ["mealier_session=", "HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(set.contains(attr), "{attr} missing from {set}");
    }
    let value = set.split(';').next().unwrap().to_string();
    let (status, _, body) = send(&app, "GET", "/api/me", &[("cookie", &value)], "").await;
    assert_eq!(status, StatusCode::OK);
    let me: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!((me["username"].as_str(), me["role"].as_str()), (Some("admin"), Some("admin")));
    assert!(me["oidc_issuer"].is_null() && me["oidc_subject"].is_null());
    assert_eq!(count(&db, "users WHERE local_admin = 1").await, 1);
}

#[tokio::test]
async fn signing_in_twice_reuses_the_one_local_admin_row() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, enabled());
    post(&app, "/login/local", &login_form(PASSWORD)).await;
    post(&app, "/login/local", &login_form(PASSWORD)).await;
    assert_eq!((count(&db, "users").await, count(&db, "sessions").await), (1, 2));
}

#[tokio::test]
async fn insecure_cookies_drops_only_the_secure_attribute() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, AuthConfig { secure_cookies: false, ..enabled() });
    let (_, h, _) = post(&app, "/login/local", &login_form(PASSWORD)).await;
    let set = h["set-cookie"].to_str().unwrap();
    assert!(!set.contains("Secure") && set.contains("HttpOnly") && set.contains("SameSite=Lax"), "{set}");
}

#[tokio::test]
async fn a_wrong_password_or_username_is_401_with_no_session() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, enabled());
    for form in [login_form("wrong password!!"), login_form(""), serde_urlencoded::to_string([("username", "root"), ("password", PASSWORD)]).unwrap()] {
        let (status, h, _) = post(&app, "/login/local", &form).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{form}");
        assert!(h.get("set-cookie").is_none());
    }
    assert_eq!((count(&db, "sessions").await, count(&db, "users").await), (0, 0), "a failed login creates nothing");
}

#[tokio::test]
async fn the_sixth_failure_in_a_row_is_429_and_then_even_the_right_password_waits() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, enabled());
    for i in 1..=5 {
        let (status, ..) = post(&app, "/login/local", &login_form("wrong password!!")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i}");
    }
    let (status, h, _) = post(&app, "/login/local", &login_form("wrong password!!")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(h.get("set-cookie").is_none());
    let (status, h, _) = post(&app, "/login/local", &login_form(PASSWORD)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "locked out, not just slowed");
    assert!(h.get("set-cookie").is_none());
    assert_eq!(count(&db, "sessions").await, 0);
}

#[tokio::test]
async fn four_failures_then_the_right_password_still_signs_in() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, enabled());
    for _ in 0..4 {
        post(&app, "/login/local", &login_form("wrong password!!")).await;
    }
    let (status, ..) = post(&app, "/login/local", &login_form(PASSWORD)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn without_a_password_file_local_login_is_disabled() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, AuthConfig::default());
    let (status, h, _) = post(&app, "/login/local", &login_form(PASSWORD)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(h.get("set-cookie").is_none());
    assert_eq!(count(&db, "users").await, 0);

    let (_, _, page) = send(&app, "GET", "/login", &[], "").await;
    assert!(!page.contains("name=\"password\""), "no form when disabled");
    let (_, _, page) = send(&app_with(&db, enabled()), "GET", "/login", &[], "").await;
    assert!(page.contains("name=\"password\""));
}

#[tokio::test]
async fn a_disabled_local_admin_cannot_sign_in() {
    let db = Db::open_memory().await.unwrap();
    let app = app_with(&db, enabled());
    post(&app, "/login/local", &login_form(PASSWORD)).await;
    sqlx::query("UPDATE users SET disabled = 1").execute(db.pool()).await.unwrap();
    let (status, h, _) = post(&app, "/login/local", &login_form(PASSWORD)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(h.get("set-cookie").is_none());
}

#[tokio::test]
async fn an_imported_user_named_admin_is_not_taken_over() {
    let db = Db::open_memory().await.unwrap();
    let imported: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role) VALUES ('admin', 'old-admin@x', 'user') RETURNING id").fetch_one(db.pool()).await.unwrap();
    let app = app_with(&db, enabled());
    let (status, h, _) = post(&app, "/login/local", &login_form(PASSWORD)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let value = h["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let (_, _, body) = send(&app, "GET", "/api/me", &[("cookie", &value)], "").await;
    let me: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_ne!(me["id"], imported);
    assert_eq!(me["role"], "admin");
    let after = db.user(imported).await.unwrap();
    assert_eq!((after.username.as_str(), after.role.as_str()), ("admin", "user"));
}
