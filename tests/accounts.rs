//! Users admin (API and page) and the per-user tokens page.
mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use mealier::{AuthConfig, Db, ImportService, api};
use serde_json::Value;
use tower::ServiceExt;

const BASE: &str = "https://mealier.test";

struct Ctx {
    app: Router,
    db: Db,
    alice: i64,
    root: i64,
    alice_cookie: String,
    root_cookie: String,
}

async fn ctx() -> Ctx {
    let db = Db::open_memory().await.unwrap();
    let add = |n: &'static str, role: &'static str| {
        let db = db.clone();
        async move { sqlx::query_scalar::<_, i64>("INSERT INTO users (username, email, role, household_id) VALUES (?, ?, ?, ?) RETURNING id").bind(n).bind(format!("{n}@x")).bind(role).bind(common::household(&db).await).fetch_one(db.pool()).await.unwrap() }
    };
    let (alice, root) = (add("alice", "user").await, add("root", "admin").await);
    let (a, r) = (db.create_session(alice).await.unwrap(), db.create_session(root).await.unwrap());
    let auth = AuthConfig { base_url: Some(BASE.into()), ..AuthConfig::default() };
    let app = api::router_with_auth(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, ImportService::default(), auth);
    Ctx { app, db, alice, root, alice_cookie: format!("mealier_session={a}"), root_cookie: format!("mealier_session={r}") }
}

async fn go(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], form: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri).header("origin", BASE);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match form {
        Some(f) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(f.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, h) = (resp.status(), resp.headers().clone());
    (status, h, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

// ---------- users admin ----------

#[tokio::test]
async fn the_api_lists_users_for_admins() {
    let c = ctx().await;
    let (status, _, body) = go(&c.app, "GET", "/api/users", &[("cookie", &c.root_cookie)], None).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let names: Vec<_> = v.as_array().unwrap().iter().map(|u| u["username"].as_str().unwrap()).collect();
    assert_eq!(names, ["alice", "root"]);
    assert_eq!(v[0]["disabled"], false);
}

#[tokio::test]
async fn disabling_through_the_api_revokes_sessions_and_tokens_and_enabling_restores_the_account() {
    let c = ctx().await;
    let (_, token) = c.db.create_api_token(c.alice, "t").await.unwrap();
    let bearer = format!("Bearer {token}");
    assert_eq!(go(&c.app, "GET", "/api/me", &[("authorization", &bearer)], None).await.0, StatusCode::OK);

    let uri = format!("/api/users/{}/disable", c.alice);
    let (status, _, body) = go(&c.app, "POST", &uri, &[("cookie", &c.root_cookie)], None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["disabled"], true);
    assert_eq!(go(&c.app, "GET", "/api/me", &[("authorization", &bearer)], None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(go(&c.app, "GET", "/api/me", &[("cookie", &c.alice_cookie)], None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(go(&c.app, "GET", "/api/me", &[("authorization", &bearer)], None).await.0, StatusCode::UNAUTHORIZED, "the token row is gone for good");

    let (status, _, body) = go(&c.app, "POST", &format!("/api/users/{}/enable", c.alice), &[("cookie", &c.root_cookie)], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["disabled"], false);
    assert_eq!(go(&c.app, "GET", "/api/me", &[("authorization", &bearer)], None).await.0, StatusCode::UNAUTHORIZED, "enabling does not bring the old token back");
}

#[tokio::test]
async fn an_admin_cannot_disable_themselves_and_unknown_users_are_404() {
    let c = ctx().await;
    let (status, _, body) = go(&c.app, "POST", &format!("/api/users/{}/disable", c.root), &[("cookie", &c.root_cookie)], None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(!c.db.user(c.root).await.unwrap().disabled);
    for action in ["disable", "enable"] {
        let (status, ..) = go(&c.app, "POST", &format!("/api/users/999/{action}"), &[("cookie", &c.root_cookie)], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn the_users_page_lists_everyone_with_a_button_that_works() {
    let c = ctx().await;
    let (status, _, html) = go(&c.app, "GET", "/settings/users", &[("cookie", &c.root_cookie)], None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("alice") && html.contains("root"), "{html}");
    assert!(html.contains(&format!("/settings/users/{}/disable", c.alice)));

    let (status, h, _) = go(&c.app, "POST", &format!("/settings/users/{}/disable", c.alice), &[("cookie", &c.root_cookie)], Some("")).await;
    assert_eq!((status, h["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/settings/users"));
    assert!(c.db.user(c.alice).await.unwrap().disabled);
    let (_, _, html) = go(&c.app, "GET", "/settings/users", &[("cookie", &c.root_cookie)], None).await;
    assert!(html.contains(&format!("/settings/users/{}/enable", c.alice)));
    let (status, ..) = go(&c.app, "POST", &format!("/settings/users/{}/enable", c.alice), &[("cookie", &c.root_cookie)], Some("")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(!c.db.user(c.alice).await.unwrap().disabled);
}

// ---------- tokens page ----------

#[tokio::test]
async fn creating_a_token_shows_it_once_and_it_then_works_as_a_bearer() {
    let c = ctx().await;
    let (status, h, html) = go(&c.app, "POST", "/settings/tokens", &[("cookie", &c.alice_cookie)], Some("name=my+script")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h["cache-control"], "no-store");
    let secret = html.split("mlr_").nth(1).map(|r| format!("mlr_{}", &r[..64])).expect("the token is on the page");
    assert!(html.contains("my script"));

    let (status, _, body) = go(&c.app, "GET", "/api/me", &[("authorization", &format!("Bearer {secret}"))], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["username"], "alice");

    let (_, _, again) = go(&c.app, "GET", "/settings/tokens", &[("cookie", &c.alice_cookie)], None).await;
    assert!(again.contains("my script") && !again.contains(&secret), "never shown a second time");
}

#[tokio::test]
async fn tokens_are_listed_and_revoked_per_user() {
    let c = ctx().await;
    let (mine, my_secret) = c.db.create_api_token(c.alice, "mine").await.unwrap();
    let (theirs, their_secret) = c.db.create_api_token(c.root, "theirs").await.unwrap();

    let (_, _, html) = go(&c.app, "GET", "/settings/tokens", &[("cookie", &c.alice_cookie)], None).await;
    assert!(html.contains("mine") && !html.contains("theirs"), "{html}");

    let (status, ..) = go(&c.app, "POST", &format!("/settings/tokens/{}/revoke", theirs.id), &[("cookie", &c.alice_cookie)], Some("")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not yours to revoke");
    assert!(c.db.token_user(&their_secret).await.unwrap().is_some());

    let (status, h, _) = go(&c.app, "POST", &format!("/settings/tokens/{}/revoke", mine.id), &[("cookie", &c.alice_cookie)], Some("")).await;
    assert_eq!((status, h["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/settings/tokens"));
    assert!(c.db.token_user(&my_secret).await.unwrap().is_none());
}

#[tokio::test]
async fn a_blank_token_name_is_a_422_and_creates_nothing() {
    let c = ctx().await;
    let (status, ..) = go(&c.app, "POST", "/settings/tokens", &[("cookie", &c.alice_cookie)], Some("name=+++")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(c.db.api_tokens(c.alice).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_token_cannot_mint_or_revoke_tokens() {
    let c = ctx().await;
    let (row, secret) = c.db.create_api_token(c.alice, "t").await.unwrap();
    let bearer = format!("Bearer {secret}");
    for (m, uri, form) in [("GET", "/settings/tokens".to_string(), None), ("POST", "/settings/tokens".to_string(), Some("name=x")), ("POST", format!("/settings/tokens/{}/revoke", row.id), Some(""))] {
        let (status, ..) = go(&c.app, m, &uri, &[("authorization", &bearer)], form).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{m} {uri}");
    }
    assert_eq!(c.db.api_tokens(c.alice).await.unwrap().len(), 1);
}
