//! S27: the OIDC code flow against an in-test mock identity provider.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use mealier::{AuthConfig, Db, ImportService, OidcConfig, api};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const BASE: &str = "https://mealier.test";
const CLIENT_ID: &str = "mealier-client";
const SECRET: &str = "top-secret-value";

// ---------- the mock provider ----------

#[derive(Clone, Copy)]
enum Signer {
    Rsa,       // kid rsa1, published
    Ec,        // kid ec1, published
    RsaOther,  // kid rsa1 but a key the JWKS does not hold
    Hmac,      // HS256 keyed with a guessable secret (algorithm confusion)
    NoSig,     // alg none
}

struct Issued {
    challenge: String,
    claims: Value,
    signer: Signer,
}

#[derive(Default)]
struct Inner {
    codes: HashMap<String, Issued>,
    issuer_override: Option<String>,
    token_status: Option<u16>,
    token_calls: u32,
}

#[derive(Clone)]
struct Idp {
    base: String, // http://127.0.0.1:port/o/mealier/
    inner: Arc<Mutex<Inner>>,
}

fn pem(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/oidc/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sign(signer: Signer, claims: &Value) -> String {
    let (alg, kid, key) = match signer {
        Signer::Rsa => (Algorithm::RS256, "rsa1", EncodingKey::from_rsa_pem(&pem("rsa.pem")).unwrap()),
        Signer::Ec => (Algorithm::ES256, "ec1", EncodingKey::from_ec_pem(&pem("ec.pem")).unwrap()),
        Signer::RsaOther => (Algorithm::RS256, "rsa1", EncodingKey::from_rsa_pem(&pem("rsa-other.pem")).unwrap()),
        Signer::Hmac => (Algorithm::HS256, "rsa1", EncodingKey::from_secret(b"guessable")),
        Signer::NoSig => return format!("{}.{}.", b64(br#"{"alg":"none","typ":"JWT"}"#), b64(claims.to_string().as_bytes())),
    };
    let mut h = Header::new(alg);
    h.kid = Some(kid.into());
    jsonwebtoken::encode(&h, claims, &key).unwrap()
}

impl Idp {
    async fn start() -> Idp {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/o/mealier/", l.local_addr().unwrap());
        let idp = Idp { base: base.clone(), inner: Arc::default() };
        let app = Router::new()
            .route("/o/mealier/.well-known/openid-configuration", get(discovery))
            .route("/o/mealier/jwks", get(|| async { Json(serde_json::from_slice::<Value>(&pem("jwks.json")).unwrap()) }))
            .route("/o/mealier/token", post(token))
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        idp
    }

    fn config_url(&self) -> String {
        format!("{}.well-known/openid-configuration", self.base)
    }

    /// What the real provider does after the user approves: remember the challenge, hand back a code.
    fn issue(&self, challenge: &str, claims: Value, signer: Signer) -> String {
        let code = format!("code-{}", self.inner.lock().unwrap().codes.len());
        self.inner.lock().unwrap().codes.insert(code.clone(), Issued { challenge: challenge.into(), claims, signer });
        code
    }
}

async fn discovery(State(idp): State<Idp>) -> Json<Value> {
    let issuer = idp.inner.lock().unwrap().issuer_override.clone().unwrap_or_else(|| idp.base.clone());
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{}authorize", idp.base),
        "token_endpoint": format!("{}token", idp.base),
        "jwks_uri": format!("{}jwks", idp.base),
    }))
}

async fn token(State(idp): State<Idp>, Form(f): Form<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let mut s = idp.inner.lock().unwrap();
    s.token_calls += 1;
    if let Some(code) = s.token_status {
        return (StatusCode::from_u16(code).unwrap(), Json(json!({"error": "server_error"})));
    }
    let bad = || (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
    if get("grant_type") != "authorization_code" || get("client_id") != CLIENT_ID || get("client_secret") != SECRET || get("redirect_uri") != format!("{BASE}/auth/callback") {
        return bad();
    }
    let Some(issued) = s.codes.remove(get("code")) else { return bad() };
    // PKCE: S256 of the verifier must be the challenge sent with the authorization request.
    if b64(&Sha256::digest(get("code_verifier").as_bytes())) != issued.challenge {
        return bad();
    }
    (StatusCode::OK, Json(json!({"access_token": "unused", "token_type": "Bearer", "id_token": sign(issued.signer, &issued.claims)})))
}

// ---------- the browser ----------

struct Ctx {
    app: Router,
    db: Db,
    idp: Idp,
}

async fn ctx() -> Ctx {
    let idp = Idp::start().await;
    let db = Db::open_memory().await.unwrap();
    let oidc = OidcConfig {
        config_url: idp.config_url(),
        client_id: CLIENT_ID.into(),
        client_secret: SECRET.into(),
        user_group: "mealier-users".into(),
        admin_group: "mealier-admins".into(),
    };
    let auth = AuthConfig { base_url: Some(BASE.into()), oidc: Some(oidc), ..AuthConfig::default() };
    let app = api::router_with_auth(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, ImportService::default(), auth);
    Ctx { app, db, idp }
}

async fn get_(app: &Router, uri: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().uri(uri);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    let resp = app.clone().oneshot(req.body(axum::body::Body::empty()).unwrap()).await.unwrap();
    let (status, h) = (resp.status(), resp.headers().clone());
    (status, h, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

struct Flow {
    state: String,
    nonce: String,
    challenge: String,
    cookie: String,
    location: url::Url,
}

/// Click "Sign in with Authentik".
async fn start(c: &Ctx) -> Flow {
    let (status, h, _) = get_(&c.app, "/login/oidc", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "starting the login: {h:?}");
    let location = url::Url::parse(h["location"].to_str().unwrap()).unwrap();
    let q: HashMap<_, _> = location.query_pairs().into_owned().collect();
    let cookie = h.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find(|v| v.starts_with("mealier_oidc=")).expect("a login cookie binds the browser").to_string();
    Flow { state: q["state"].clone(), nonce: q["nonce"].clone(), challenge: q["code_challenge"].clone(), cookie: cookie.split(';').next().unwrap().to_string(), location }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn claims(c: &Ctx, f: &Flow) -> Value {
    json!({
        "iss": c.idp.base, "aud": CLIENT_ID, "sub": "sub-1", "exp": now() + 300, "iat": now(), "nonce": f.nonce,
        "email": "ann@example.test", "email_verified": false, "preferred_username": "ann", "name": "Ann Cook",
        "groups": ["mealier-users"],
    })
}

async fn callback(c: &Ctx, code: &str, state: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, String) {
    get_(&c.app, &format!("/auth/callback?code={code}&state={state}"), cookie).await
}

/// A whole login; `edit` changes the claims the provider signs.
async fn login_with(c: &Ctx, signer: Signer, edit: impl FnOnce(&mut Value)) -> (StatusCode, HeaderMap, String) {
    let f = start(c).await;
    let mut cl = claims(c, &f);
    edit(&mut cl);
    let code = c.idp.issue(&f.challenge, cl, signer);
    callback(c, &code, &f.state, Some(&f.cookie)).await
}

async fn login(c: &Ctx, edit: impl FnOnce(&mut Value)) -> (StatusCode, HeaderMap, String) {
    login_with(c, Signer::Rsa, edit).await
}

fn session_of(h: &HeaderMap) -> Option<String> {
    h.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find(|v| v.starts_with("mealier_session=") && !v.contains("Max-Age=0")).map(|v| v.split(';').next().unwrap().to_string())
}

async fn count(db: &Db, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(db.pool()).await.unwrap()
}

async fn user_by_email(db: &Db, email: &str) -> mealier::User {
    db.users().await.unwrap().into_iter().find(|u| u.email == email).unwrap_or_else(|| panic!("no user {email}"))
}

/// Asserts the login was refused and nothing was created.
async fn assert_rejected(c: &Ctx, r: &(StatusCode, HeaderMap, String), users: i64) {
    assert!(r.0 == StatusCode::BAD_REQUEST || r.0 == StatusCode::FORBIDDEN, "{} {}", r.0, r.2);
    assert!(session_of(&r.1).is_none(), "no session may be issued");
    assert_eq!(count(&c.db, "sessions").await, 0);
    assert_eq!(count(&c.db, "users").await, users, "no user may be created");
}

// ---------- happy path and the request we send ----------

#[tokio::test]
async fn the_authorization_request_carries_pkce_s256_state_nonce_and_our_redirect_uri() {
    let c = ctx().await;
    let f = start(&c).await;
    let q: HashMap<_, _> = f.location.query_pairs().into_owned().collect();
    assert_eq!(f.location.path(), "/o/mealier/authorize");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["redirect_uri"], format!("{BASE}/auth/callback"));
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].split(' ').any(|s| s == "openid"));
    assert!(q["state"].len() >= 32 && q["nonce"].len() >= 32 && q["state"] != q["nonce"]);
    assert_eq!(q["code_challenge"].len(), 43, "base64url of a SHA-256");
    assert!(!f.location.as_str().contains(SECRET), "the client secret never goes through the browser");
    assert_eq!(count(&c.db, "oidc_logins").await, 1);

    let set = f.cookie.clone();
    assert_eq!(set, format!("mealier_oidc={}", f.state), "the cookie carries the state");
    let (_, h, _) = get_(&c.app, "/login/oidc", None).await;
    let full = h.get_all("set-cookie").iter().find(|v| v.to_str().unwrap().starts_with("mealier_oidc=")).unwrap().to_str().unwrap().to_string();
    for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/auth", "Max-Age=600"] {
        assert!(full.contains(attr), "{attr} missing from {full}");
    }
    let stored: String = sqlx::query_scalar("SELECT state_hash FROM oidc_logins LIMIT 1").fetch_one(c.db.pool()).await.unwrap();
    assert_ne!(stored, f.state, "the state is stored hashed");
}

#[tokio::test]
async fn a_first_login_creates_the_user_binds_the_identity_and_signs_in() {
    let c = ctx().await;
    let (status, h, _) = login(&c, |_| {}).await;
    assert_eq!((status, h["location"].to_str().unwrap()), (StatusCode::SEE_OTHER, "/"));

    let u = user_by_email(&c.db, "ann@example.test").await;
    assert_eq!((u.username.as_str(), u.full_name.as_str(), u.role.as_str()), ("ann", "Ann Cook", "user"));
    assert_eq!((u.oidc_issuer.as_deref(), u.oidc_subject.as_deref()), (Some(c.idp.base.as_str()), Some("sub-1")));
    assert!(!u.disabled);

    let session = h.get_all("set-cookie").iter().map(|v| v.to_str().unwrap()).find(|v| v.starts_with("mealier_session=")).unwrap().to_string();
    for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(session.contains(attr), "{attr} missing from {session}");
    }
    let cookie = session_of(&h).unwrap();
    let (status, _, body) = get_(&c.app, "/api/me", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["id"], u.id);
    assert!(h.get_all("set-cookie").iter().any(|v| v.to_str().unwrap().starts_with("mealier_oidc=;")), "the login cookie is cleared");
    assert_eq!(count(&c.db, "oidc_logins").await, 0, "the login row is spent");
}

#[tokio::test]
async fn email_verified_false_is_fine_and_es256_tokens_are_accepted() {
    let c = ctx().await;
    let (status, ..) = login_with(&c, Signer::Ec, |cl| cl["email_verified"] = json!(false)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(count(&c.db, "users").await, 1);
}

#[tokio::test]
async fn a_second_login_finds_the_same_user_by_subject_even_if_the_email_changed() {
    let c = ctx().await;
    login(&c, |_| {}).await;
    let (status, ..) = login(&c, |cl| cl["email"] = json!("ann.new@example.test")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(count(&c.db, "users").await, 1);
    assert_eq!(c.db.users().await.unwrap()[0].email, "ann@example.test", "bound by issuer + subject, not email");
    assert_eq!(count(&c.db, "sessions").await, 2);
}

// ---------- groups ----------

#[tokio::test]
async fn someone_outside_the_users_group_gets_a_403_page_and_nothing_is_created() {
    let c = ctx().await;
    for groups in [json!(["family"]), json!([]), json!("mealier-users"), json!(null), json!(["mealier-admins"])] {
        let r = login(&c, |cl| cl["groups"] = groups.clone()).await;
        assert_eq!(r.0, StatusCode::FORBIDDEN, "{groups}");
        assert_rejected(&c, &r, 0).await;
    }
    let r = login(&c, |cl| {
        cl.as_object_mut().unwrap().remove("groups");
    })
    .await;
    assert_eq!(r.0, StatusCode::FORBIDDEN);
    assert_rejected(&c, &r, 0).await;
}

#[tokio::test]
async fn the_admin_group_grants_admin_and_losing_it_removes_it_on_the_next_login() {
    let c = ctx().await;
    login(&c, |cl| cl["groups"] = json!(["mealier-users", "mealier-admins"])).await;
    assert_eq!(user_by_email(&c.db, "ann@example.test").await.role, "admin");
    login(&c, |_| {}).await;
    assert_eq!(user_by_email(&c.db, "ann@example.test").await.role, "user", "removed");
    login(&c, |cl| cl["groups"] = json!(["mealier-admins", "mealier-users"])).await;
    assert_eq!(user_by_email(&c.db, "ann@example.test").await.role, "admin", "granted again");
    assert_eq!(count(&c.db, "users").await, 1);
}

#[tokio::test]
async fn a_login_outside_the_users_group_does_not_touch_an_existing_users_role() {
    let c = ctx().await;
    login(&c, |cl| cl["groups"] = json!(["mealier-users", "mealier-admins"])).await;
    let r = login(&c, |cl| cl["groups"] = json!([])).await;
    assert_eq!(r.0, StatusCode::FORBIDDEN);
    assert_eq!(user_by_email(&c.db, "ann@example.test").await.role, "admin");
}

#[tokio::test]
async fn configured_group_names_are_used() {
    let idp = Idp::start().await;
    let db = Db::open_memory().await.unwrap();
    let oidc = OidcConfig { config_url: idp.config_url(), client_id: CLIENT_ID.into(), client_secret: SECRET.into(), user_group: "eaters".into(), admin_group: "chefs".into() };
    let auth = AuthConfig { base_url: Some(BASE.into()), oidc: Some(oidc), ..AuthConfig::default() };
    let c = Ctx { app: api::router_with_auth(api::AppState { db: db.clone(), images_dir: tempfile::tempdir().unwrap().keep() }, ImportService::default(), auth), db, idp };
    assert_eq!(login(&c, |_| {}).await.0, StatusCode::FORBIDDEN, "the default group names no longer count");
    assert_eq!(login(&c, |cl| cl["groups"] = json!(["eaters", "chefs"])).await.0, StatusCode::SEE_OTHER);
    assert_eq!(user_by_email(&c.db, "ann@example.test").await.role, "admin");
}

// ---------- identity binding ----------

async fn imported(db: &Db, username: &str, email: &str, role: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO users (username, email, full_name, role) VALUES (?, ?, 'Imported Ann', ?) RETURNING id").bind(username).bind(email).bind(role).fetch_one(db.pool()).await.unwrap()
}

#[tokio::test]
async fn a_first_login_links_to_the_one_imported_user_with_that_email() {
    let c = ctx().await;
    let id = imported(&c.db, "ann-mealie", "Ann@Example.test", "admin").await; // case differs on purpose
    let (status, h, _) = login(&c, |_| {}).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(count(&c.db, "users").await, 1, "linked, not duplicated");
    let u = c.db.user(id).await.unwrap();
    assert_eq!((u.oidc_issuer.as_deref(), u.oidc_subject.as_deref()), (Some(c.idp.base.as_str()), Some("sub-1")));
    assert_eq!((u.username.as_str(), u.full_name.as_str()), ("ann-mealie", "Imported Ann"), "imported details are kept");
    assert_eq!(u.role, "user", "role follows the groups, so an imported admin outside the admin group is demoted");
    let (_, _, body) = get_(&c.app, "/api/me", session_of(&h).as_deref()).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["id"], id);
}

#[tokio::test]
async fn an_email_shared_by_several_users_is_refused_not_guessed() {
    let c = ctx().await;
    imported(&c.db, "a1", "ann@example.test", "user").await;
    imported(&c.db, "a2", "ANN@example.test", "user").await;
    let r = login(&c, |_| {}).await;
    assert_eq!(r.0, StatusCode::FORBIDDEN);
    assert!(r.2.contains("cannot be linked automatically"), "{}", r.2);
    assert_rejected(&c, &r, 2).await;
    assert_eq!(count(&c.db, "users WHERE oidc_issuer IS NOT NULL").await, 0);
}

#[tokio::test]
async fn an_email_already_bound_to_another_identity_is_refused() {
    let c = ctx().await;
    login(&c, |_| {}).await; // sub-1 owns ann@example.test
    let r = login(&c, |cl| cl["sub"] = json!("sub-2")).await;
    assert_eq!(r.0, StatusCode::FORBIDDEN);
    assert_eq!(count(&c.db, "users").await, 1);
    assert_eq!(count(&c.db, "sessions").await, 1, "only the first login has a session");
    assert_eq!(c.db.users().await.unwrap()[0].oidc_subject.as_deref(), Some("sub-1"));
}

#[tokio::test]
async fn the_local_admins_row_can_never_be_claimed_by_email() {
    let c = ctx().await;
    c.db.local_admin().await.unwrap();
    let r = login(&c, |cl| cl["email"] = json!("local-admin@mealier.invalid")).await;
    assert_eq!(r.0, StatusCode::FORBIDDEN);
    let admin = c.db.users().await.unwrap().remove(0);
    assert!(admin.oidc_issuer.is_none() && admin.oidc_subject.is_none());
    assert_eq!(count(&c.db, "users").await, 1);
}

#[tokio::test]
async fn a_disabled_user_cannot_sign_in_by_oidc() {
    let c = ctx().await;
    login(&c, |_| {}).await;
    let id = c.db.users().await.unwrap()[0].id;
    c.db.set_user_disabled(id, true).await.unwrap();
    let r = login(&c, |_| {}).await;
    assert_eq!(r.0, StatusCode::FORBIDDEN);
    assert!(session_of(&r.1).is_none());
    assert!(c.db.user(id).await.unwrap().disabled, "logging in does not re-enable");

    // Same for a disabled imported user, before any link.
    let c = ctx().await;
    let id = imported(&c.db, "ann-mealie", "ann@example.test", "user").await;
    c.db.set_user_disabled(id, true).await.unwrap();
    let r = login(&c, |_| {}).await;
    assert_eq!(r.0, StatusCode::FORBIDDEN);
    assert!(c.db.user(id).await.unwrap().oidc_subject.is_none());
}

#[tokio::test]
async fn a_username_collision_gets_a_suffix_and_a_missing_email_is_refused() {
    let c = ctx().await;
    imported(&c.db, "ann", "someone-else@example.test", "user").await;
    assert_eq!(login(&c, |_| {}).await.0, StatusCode::SEE_OTHER);
    assert_eq!(user_by_email(&c.db, "ann@example.test").await.username, "ann-2");

    let c = ctx().await;
    for edit in [json!(null), json!(""), json!("no-at-sign")] {
        let r = login(&c, |cl| cl["email"] = edit.clone()).await;
        assert_eq!(r.0, StatusCode::FORBIDDEN, "{edit}");
    }
    assert_eq!(count(&c.db, "users").await, 0);
}

// ---------- state, nonce and token checks ----------

#[tokio::test]
async fn an_unknown_state_or_a_missing_or_mismatched_login_cookie_is_rejected() {
    let c = ctx().await;
    let f = start(&c).await;
    let code = c.idp.issue(&f.challenge, claims(&c, &f), Signer::Rsa);

    let r = callback(&c, &code, "not-a-state", Some("mealier_oidc=not-a-state")).await;
    assert_rejected(&c, &r, 0).await;
    let r = callback(&c, &code, &f.state, None).await;
    assert_rejected(&c, &r, 0).await;
    let other = start(&c).await; // a second login in the same browser would overwrite the cookie; an attacker's state must not match ours
    let r = callback(&c, &code, &f.state, Some(&other.cookie)).await;
    assert_rejected(&c, &r, 0).await;
    assert_eq!(c.idp.inner.lock().unwrap().token_calls, 0, "refused before the token endpoint is called");
}

#[tokio::test]
async fn a_state_works_once_and_a_replay_is_rejected() {
    let c = ctx().await;
    let f = start(&c).await;
    let code = c.idp.issue(&f.challenge, claims(&c, &f), Signer::Rsa);
    assert_eq!(callback(&c, &code, &f.state, Some(&f.cookie)).await.0, StatusCode::SEE_OTHER);
    let again = c.idp.issue(&f.challenge, claims(&c, &f), Signer::Rsa);
    let r = callback(&c, &again, &f.state, Some(&f.cookie)).await;
    assert!(r.0 == StatusCode::BAD_REQUEST || r.0 == StatusCode::FORBIDDEN, "{}", r.0);
    assert!(session_of(&r.1).is_none());
    assert_eq!(count(&c.db, "sessions").await, 1);
}

#[tokio::test]
async fn a_failed_callback_still_spends_the_state() {
    let c = ctx().await;
    let f = start(&c).await;
    let mut bad = claims(&c, &f);
    bad["nonce"] = json!("wrong");
    let code = c.idp.issue(&f.challenge, bad, Signer::Rsa);
    callback(&c, &code, &f.state, Some(&f.cookie)).await;
    let good = c.idp.issue(&f.challenge, claims(&c, &f), Signer::Rsa);
    let r = callback(&c, &good, &f.state, Some(&f.cookie)).await;
    assert_rejected(&c, &r, 0).await;
}

#[tokio::test]
async fn a_login_older_than_ten_minutes_is_rejected() {
    let c = ctx().await;
    let f = start(&c).await;
    sqlx::query("UPDATE oidc_logins SET expires_at = '2000-01-01T00:00:00.000Z'").execute(c.db.pool()).await.unwrap();
    let code = c.idp.issue(&f.challenge, claims(&c, &f), Signer::Rsa);
    let r = callback(&c, &code, &f.state, Some(&f.cookie)).await;
    assert_rejected(&c, &r, 0).await;
    let secs: f64 = {
        let c2 = ctx().await;
        start(&c2).await;
        sqlx::query_scalar("SELECT (julianday(expires_at) - julianday('now')) * 86400 FROM oidc_logins").fetch_one(c2.db.pool()).await.unwrap()
    };
    assert!((secs - 600.0).abs() < 10.0, "{secs}");
}

#[tokio::test]
async fn id_tokens_with_a_wrong_nonce_audience_issuer_or_time_are_rejected() {
    let c = ctx().await;
    type Edit = fn(&mut Value);
    let cases: Vec<(&str, Edit)> = vec![
        ("nonce", |cl| cl["nonce"] = json!("someone-elses")),
        ("no nonce", |cl| {
            cl.as_object_mut().unwrap().remove("nonce");
        }),
        ("audience", |cl| cl["aud"] = json!("another-client")),
        ("audience list without us", |cl| cl["aud"] = json!(["a", "b"])),
        ("multiple audiences, azp not us", |cl| {
            cl["aud"] = json!([CLIENT_ID, "other"]);
            cl["azp"] = json!("other");
        }),
        ("issuer", |cl| cl["iss"] = json!("https://evil.example/")),
        ("expired", |cl| cl["exp"] = json!(now() - 3600)),
        ("no exp", |cl| {
            cl.as_object_mut().unwrap().remove("exp");
        }),
        ("not yet valid", |cl| cl["nbf"] = json!(now() + 3600)),
        ("no subject", |cl| {
            cl.as_object_mut().unwrap().remove("sub");
        }),
        ("empty subject", |cl| cl["sub"] = json!("")),
    ];
    for (name, edit) in cases {
        let r = login(&c, edit).await;
        assert!(r.0 == StatusCode::BAD_REQUEST || r.0 == StatusCode::FORBIDDEN, "{name}: {}", r.0);
        assert!(session_of(&r.1).is_none(), "{name}");
    }
    assert_eq!((count(&c.db, "users").await, count(&c.db, "sessions").await), (0, 0));
}

#[tokio::test]
async fn a_list_of_audiences_naming_us_with_azp_is_accepted() {
    let c = ctx().await;
    let (status, ..) = login(&c, |cl| {
        cl["aud"] = json!([CLIENT_ID, "other"]);
        cl["azp"] = json!(CLIENT_ID);
    })
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn forged_tokens_are_rejected_whatever_they_claim() {
    let c = ctx().await;
    for (name, signer) in [("unpublished key under a published kid", Signer::RsaOther), ("HS256 (algorithm confusion)", Signer::Hmac), ("alg none", Signer::NoSig)] {
        let r = login_with(&c, signer, |cl| cl["groups"] = json!(["mealier-users", "mealier-admins"])).await;
        assert!(r.0 == StatusCode::BAD_REQUEST || r.0 == StatusCode::FORBIDDEN, "{name}: {}", r.0);
        assert!(session_of(&r.1).is_none(), "{name}");
    }
    assert_eq!((count(&c.db, "users").await, count(&c.db, "sessions").await), (0, 0));
}

#[tokio::test]
async fn pkce_the_token_endpoint_sees_our_verifier_and_a_wrong_challenge_fails() {
    let c = ctx().await;
    let f = start(&c).await;
    let code = c.idp.issue("a-challenge-for-some-other-verifier", claims(&c, &f), Signer::Rsa);
    let r = callback(&c, &code, &f.state, Some(&f.cookie)).await;
    assert_rejected(&c, &r, 0).await;
    assert_eq!(c.idp.inner.lock().unwrap().token_calls, 1);
}

#[tokio::test]
async fn provider_errors_and_a_mismatched_discovery_issuer_end_the_login_cleanly() {
    let c = ctx().await;
    c.idp.inner.lock().unwrap().token_status = Some(500);
    let r = login(&c, |_| {}).await;
    assert_rejected(&c, &r, 0).await;

    let c = ctx().await;
    let f = start(&c).await;
    let (status, _, _) = get_(&c.app, &format!("/auth/callback?error=access_denied&state={}", f.state), Some(&f.cookie)).await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::FORBIDDEN);

    let c = ctx().await;
    c.idp.inner.lock().unwrap().issuer_override = Some("https://evil.example/o/mealier/".into());
    let (status, ..) = get_(&c.app, "/login/oidc", None).await;
    assert_ne!(status, StatusCode::SEE_OTHER, "a discovery document for another issuer is not followed");
}

#[tokio::test]
async fn oidc_routes_are_404_when_oidc_is_not_configured() {
    let db = Db::open_memory().await.unwrap();
    let app = api::router(api::AppState { db, images_dir: tempfile::tempdir().unwrap().keep() });
    assert_eq!(get_(&app, "/login/oidc", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(get_(&app, "/auth/callback?code=x&state=y", None).await.0, StatusCode::NOT_FOUND);
    let (_, _, page) = get_(&app, "/login", None).await;
    assert!(!page.contains("Sign in with Authentik"));
}

#[tokio::test]
async fn the_login_page_offers_the_authentik_button_and_never_auto_redirects() {
    let c = ctx().await;
    let (status, _, page) = get_(&c.app, "/login", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Sign in with Authentik") && page.contains("/login/oidc"));
}

#[tokio::test]
async fn starting_logins_is_capped_so_anonymous_visitors_cannot_fill_the_table() {
    let c = ctx().await;
    sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) INSERT INTO oidc_logins SELECT 'h' || i, 'n', 'v', '2999-01-01T00:00:00.000Z' FROM n")
        .execute(c.db.pool())
        .await
        .unwrap();
    let (status, ..) = get_(&c.app, "/login/oidc", None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(count(&c.db, "oidc_logins").await, 1000);
}
