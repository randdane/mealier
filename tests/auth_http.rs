//! S26: the auth middleware, through the router.
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
    alice: i64, // plain user
    root: i64,  // admin
}

async fn add_user(db: &Db, name: &str, role: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO users (username, email, role) VALUES (?, ?, ?) RETURNING id")
        .bind(name)
        .bind(format!("{name}@x"))
        .bind(role)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn ctx() -> Ctx {
    let db = Db::open_memory().await.unwrap();
    let (alice, root) = (add_user(&db, "alice", "user").await, add_user(&db, "root", "admin").await);
    let auth = AuthConfig { base_url: Some(BASE.into()), ..AuthConfig::default() };
    let app = api::router_with_auth(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, ImportService::default(), auth);
    Ctx { app, db, alice, root }
}

async fn go(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/x-www-form-urlencoded");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn cookie(value: &str) -> String {
    format!("mealier_session={value}")
}

fn is_login_redirect(status: StatusCode, h: &HeaderMap) -> bool {
    status == StatusCode::SEE_OTHER && h["location"] == "/login"
}

#[tokio::test]
async fn unauthenticated_html_requests_are_redirected_to_login_with_303() {
    let c = ctx().await;
    for (m, uri) in [("GET", "/"), ("GET", "/recipes/1"), ("GET", "/settings/webhooks"), ("GET", "/images/x.png"), ("GET", "/nope"), ("POST", "/recipes")] {
        let (status, h, _) = go(&c.app, m, uri, &[], None).await;
        assert!(is_login_redirect(status, &h), "{m} {uri} -> {status} {h:?}");
    }
}

#[tokio::test]
async fn unauthenticated_api_requests_get_401_json() {
    let c = ctx().await;
    for (m, uri) in [("GET", "/api/recipes"), ("POST", "/api/recipes"), ("GET", "/api/openapi.json"), ("GET", "/api/me"), ("GET", "/api/webhooks")] {
        let (status, h, body) = go(&c.app, m, uri, &[], None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{m} {uri}");
        assert!(h["content-type"].to_str().unwrap().starts_with("application/json"));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()), "{body}");
    }
}

#[tokio::test]
async fn healthz_login_and_static_need_no_user() {
    let c = ctx().await;
    let (status, _, body) = go(&c.app, "GET", "/healthz", &[], None).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));
    for uri in ["/login", "/static/app.css", "/static/htmx.min.js"] {
        let (status, _, _) = go(&c.app, "GET", uri, &[], None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    // The OIDC entry points are public too: they answer for themselves, never with the login redirect.
    for uri in ["/login/oidc", "/auth/callback"] {
        let (status, h, _) = go(&c.app, "GET", uri, &[], None).await;
        assert!(!is_login_redirect(status, &h) && status != StatusCode::UNAUTHORIZED, "{uri} -> {status}");
    }
}

#[tokio::test]
async fn public_paths_match_whole_segments_only() {
    let c = ctx().await;
    for uri in ["/loginx", "/login-admin", "/staticfoo", "/healthz/x", "/auth/callbackx", "/auth"] {
        let (status, h, _) = go(&c.app, "GET", uri, &[], None).await;
        assert!(is_login_redirect(status, &h), "{uri} -> {status}");
    }
}

#[tokio::test]
async fn a_bearer_token_authenticates_and_me_says_who() {
    let c = ctx().await;
    let (_, token) = c.db.create_api_token(c.alice, "t").await.unwrap();
    let (status, _, body) = go(&c.app, "GET", "/api/me", &[("authorization", &format!("Bearer {token}"))], None).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((v["username"].as_str(), v["role"].as_str()), (Some("alice"), Some("user")));
    assert!(!body.contains(&token));
}

#[tokio::test]
async fn a_session_cookie_authenticates_among_other_cookies() {
    let c = ctx().await;
    let s = c.db.create_session(c.alice).await.unwrap();
    let jar = format!("a=b; {}; c=d", cookie(&s));
    let (status, _, body) = go(&c.app, "GET", "/api/me", &[("cookie", &jar)], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["username"], "alice");
    let (status, _, _) = go(&c.app, "GET", "/api/me", &[("cookie", "mealier_session=bogus")], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_bad_authorization_header_is_401_even_with_a_good_cookie() {
    let c = ctx().await;
    let s = c.db.create_session(c.alice).await.unwrap();
    for auth in ["Bearer nope", "Basic YTpi", "Bearer", "bearer"] {
        let (status, _, _) = go(&c.app, "GET", "/api/me", &[("authorization", auth), ("cookie", &cookie(&s))], None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth}");
    }
}

#[tokio::test]
async fn a_disabled_users_token_and_session_stop_working() {
    let c = ctx().await;
    let (_, token) = c.db.create_api_token(c.alice, "t").await.unwrap();
    let s = c.db.create_session(c.alice).await.unwrap();
    let bearer = format!("Bearer {token}");
    assert_eq!(go(&c.app, "GET", "/api/me", &[("authorization", &bearer)], None).await.0, StatusCode::OK);

    c.db.set_user_disabled(c.alice, true).await.unwrap();
    assert_eq!(go(&c.app, "GET", "/api/me", &[("authorization", &bearer)], None).await.0, StatusCode::UNAUTHORIZED);
    let (status, h, _) = go(&c.app, "GET", "/", &[("cookie", &cookie(&s))], None).await;
    assert!(is_login_redirect(status, &h));
}

#[tokio::test]
async fn a_user_disabled_behind_the_stores_back_is_still_refused() {
    let c = ctx().await;
    let (_, token) = c.db.create_api_token(c.alice, "t").await.unwrap();
    sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?").bind(c.alice).execute(c.db.pool()).await.unwrap();
    let (status, _, _) = go(&c.app, "GET", "/api/me", &[("authorization", &format!("Bearer {token}"))], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_and_user_admin_routes_are_admin_only() {
    let c = ctx().await;
    let (_, alice) = c.db.create_api_token(c.alice, "t").await.unwrap();
    let (_, root) = c.db.create_api_token(c.root, "t").await.unwrap();
    let (a, r) = (format!("Bearer {alice}"), format!("Bearer {root}"));
    let routes = [
        ("GET", "/api/webhooks"),
        ("POST", "/api/webhooks"),
        ("GET", "/api/webhooks/1"),
        ("PUT", "/api/webhooks/1"),
        ("DELETE", "/api/webhooks/1"),
        ("POST", "/api/webhooks/1/test"),
        ("GET", "/settings/webhooks"),
        ("POST", "/settings/webhooks"),
        ("POST", "/settings/webhooks/1/delete"),
        ("POST", "/settings/webhooks/1/toggle"),
        ("POST", "/settings/webhooks/1/test"),
        ("GET", "/api/users"),
        ("POST", "/api/users/999/disable"),
        ("POST", "/api/users/999/enable"),
        ("GET", "/settings/users"),
        ("POST", "/settings/users/999/disable"),
        ("POST", "/settings/users/999/enable"),
    ];
    for (m, uri) in routes {
        let (status, _, body) = go(&c.app, m, uri, &[("authorization", &a)], None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{m} {uri} as a plain user");
        if uri.starts_with("/api") {
            assert!(serde_json::from_str::<Value>(&body).unwrap()["error"].is_string());
        }
        let (status, _, _) = go(&c.app, m, uri, &[("authorization", &r)], None).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{m} {uri} as admin");
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{m} {uri} as admin");
    }
}

// A guard, not a red-first test: nothing refused this before step 8 either.
#[tokio::test]
async fn any_user_may_import_a_recipe_url() {
    let c = ctx().await;
    let (_, token) = c.db.create_api_token(c.alice, "t").await.unwrap();
    let (status, _, _) = go(&c.app, "POST", "/api/recipes/import", &[("authorization", &format!("Bearer {token}")), ("content-type", "application/json")], Some("{}")).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn csrf_cookie_posts_from_another_origin_are_refused() {
    let c = ctx().await;
    let s = c.db.create_session(c.alice).await.unwrap();
    let ck = cookie(&s);
    let form = Some("name=Soup");
    for origin in ["https://evil.example", "http://mealier.test", "https://mealier.test:8443", "null", "garbage"] {
        let (status, _, _) = go(&c.app, "POST", "/lists", &[("cookie", &ck), ("origin", origin)], form).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Origin {origin}");
    }
    for m in ["PUT", "DELETE", "PATCH"] {
        let (status, _, _) = go(&c.app, m, "/api/recipes/1", &[("cookie", &ck), ("origin", "https://evil.example")], None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{m}");
    }
    let (status, _, _) = go(&c.app, "POST", "/lists", &[("cookie", &ck), ("origin", BASE)], form).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "the app's own origin must pass");
    // Reads are never blocked.
    let (status, _, _) = go(&c.app, "GET", "/", &[("cookie", &ck), ("origin", "https://evil.example")], None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn csrf_without_an_origin_header_falls_back_to_sec_fetch_site() {
    let c = ctx().await;
    let ck = cookie(&c.db.create_session(c.alice).await.unwrap());
    for site in ["cross-site", "same-site", "", "weird"] {
        let (status, _, _) = go(&c.app, "POST", "/lists", &[("cookie", &ck), ("sec-fetch-site", site)], Some("name=x")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Sec-Fetch-Site {site:?}");
    }
    for site in ["same-origin", "none"] {
        let (status, _, _) = go(&c.app, "POST", "/lists", &[("cookie", &ck), ("sec-fetch-site", site)], Some("name=x")).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "Sec-Fetch-Site {site}");
    }
    // Neither header (curl, old browsers): allowed, SameSite=Lax is the cover.
    let (status, _, _) = go(&c.app, "POST", "/lists", &[("cookie", &ck)], Some("name=x")).await;
    assert_ne!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn csrf_origin_wins_over_sec_fetch_site_and_bearer_requests_are_exempt() {
    let c = ctx().await;
    let ck = cookie(&c.db.create_session(c.alice).await.unwrap());
    let (status, _, _) = go(&c.app, "POST", "/lists", &[("cookie", &ck), ("origin", "https://evil.example"), ("sec-fetch-site", "same-origin")], Some("name=x")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, token) = c.db.create_api_token(c.alice, "t").await.unwrap();
    let (status, _, _) = go(&c.app, "POST", "/api/shopping-lists", &[("authorization", &format!("Bearer {token}")), ("origin", "https://evil.example"), ("content-type", "application/json")], Some("{\"name\":\"x\"}")).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn csrf_also_guards_the_public_login_post() {
    let c = ctx().await;
    let (status, _, _) = go(&c.app, "POST", "/login/local", &[("origin", "https://evil.example")], Some("username=admin&password=x")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_deletes_the_session_clears_the_cookie_and_redirects() {
    let c = ctx().await;
    let s = c.db.create_session(c.alice).await.unwrap();
    let (status, h, _) = go(&c.app, "POST", "/logout", &[("cookie", &cookie(&s)), ("origin", BASE)], Some("")).await;
    assert!(is_login_redirect(status, &h), "{status}");
    let set = h["set-cookie"].to_str().unwrap();
    assert!(set.starts_with("mealier_session=;") && set.contains("Max-Age=0"), "{set}");
    assert!(c.db.session_user(&s).await.unwrap().is_none());
    let (status, h, _) = go(&c.app, "GET", "/", &[("cookie", &cookie(&s))], None).await;
    assert!(is_login_redirect(status, &h));
}

#[tokio::test]
async fn logout_from_another_origin_does_not_log_you_out() {
    let c = ctx().await;
    let s = c.db.create_session(c.alice).await.unwrap();
    let (status, _, _) = go(&c.app, "POST", "/logout", &[("cookie", &cookie(&s)), ("origin", "https://evil.example")], Some("")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(c.db.session_user(&s).await.unwrap().is_some());
}

#[tokio::test]
async fn the_cook_page_uses_the_signed_in_users_theme_and_the_form_saves_it_to_the_user() {
    let c = ctx().await;
    let r = c.db.create_recipe(mealier::RecipeInput { name: "Soup".into(), ..Default::default() }).await.unwrap();
    let ck = cookie(&c.db.create_session(c.alice).await.unwrap());
    let other = cookie(&c.db.create_session(c.root).await.unwrap());

    let (status, h, _) = go(&c.app, "POST", "/settings/cook-theme", &[("cookie", &ck), ("origin", BASE)], Some("theme=max-white&back=%2Frecipes%2F1%2Fcook")).await;
    assert!(status.is_redirection());
    assert_eq!(h["location"], "/recipes/1/cook");
    assert!(h.get("set-cookie").is_none(), "no theme cookie any more");

    let uri = format!("/recipes/{}/cook", r.id);
    let (_, _, html) = go(&c.app, "GET", &uri, &[("cookie", &ck)], None).await;
    assert!(html.contains("cook-max-white"), "{html}");
    let (_, _, html) = go(&c.app, "GET", &uri, &[("cookie", &other)], None).await;
    assert!(html.contains("cook-normal"), "another user keeps their own theme");
    // A leftover cookie from before step 8 changes nothing.
    let (_, _, html) = go(&c.app, "GET", &uri, &[("cookie", &format!("{ck}; cook_theme=high-contrast"))], None).await;
    assert!(html.contains("cook-max-white") && !html.contains("cook-high-contrast"));

    let (status, _, _) = go(&c.app, "POST", "/settings/cook-theme", &[("cookie", &ck), ("origin", BASE)], Some("theme=neon&back=%2F")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn csrf_without_a_base_url_compares_the_origin_to_the_host_header() {
    let db = Db::open_memory().await.unwrap();
    let alice = add_user(&db, "alice", "user").await;
    let app = api::router_with_auth(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, ImportService::default(), AuthConfig::default());
    let ck = cookie(&db.create_session(alice).await.unwrap());
    let h = |origin| [("cookie", ck.as_str()), ("host", "lab.home:9000"), ("origin", origin)];
    assert_eq!(go(&app, "POST", "/lists", &h("http://evil.example"), Some("name=x")).await.0, StatusCode::FORBIDDEN);
    assert_eq!(go(&app, "POST", "/lists", &h("http://lab.home"), Some("name=x")).await.0, StatusCode::FORBIDDEN);
    assert_ne!(go(&app, "POST", "/lists", &h("http://lab.home:9000"), Some("name=x")).await.0, StatusCode::FORBIDDEN);
}
