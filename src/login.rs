//! Sign-in, sign-out and the account pages.
use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::Redirect;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use crate::api::{AppState, ErrorBody};
use crate::guard::session_cookie_value;
use crate::{Admin, ApiToken, Auth, CurrentUser, Db, Result, SessionUser, User};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/login", get(login_page))
        .route("/login/oidc", get(oidc_start))
        .route("/auth/callback", get(oidc_callback))
        .route("/login/local", post(local_login))
        .route("/logout", post(logout))
        .route("/settings/users", get(users_page))
        .route("/settings/users/{id}/disable", post(user_disable))
        .route("/settings/users/{id}/enable", post(user_enable))
        .route("/settings/tokens", get(tokens_page).post(token_create))
        .route("/settings/tokens/{id}/revoke", post(token_revoke))
}

#[derive(Template, WebTemplate)]
#[template(path = "login.html")]
struct LoginPage {
    oidc: bool,
    local: bool,
    error: String,
}

async fn login_page(Extension(auth): Extension<Arc<Auth>>) -> LoginPage {
    LoginPage { oidc: auth.cfg.oidc.is_some(), local: auth.cfg.admin_hash.is_some(), error: String::new() }
}

#[derive(serde::Deserialize)]
struct LocalForm {
    username: String,
    password: String,
}

fn page(status: StatusCode, auth: &Auth, error: &str) -> Response {
    (status, LoginPage { oidc: auth.cfg.oidc.is_some(), local: auth.cfg.admin_hash.is_some(), error: error.into() }).into_response()
}

async fn local_login(State(db): State<Db>, Extension(auth): Extension<Arc<Auth>>, axum::Form(f): axum::Form<LocalForm>) -> Result<Response> {
    let Some(hash) = auth.cfg.admin_hash.clone() else { return Ok(StatusCode::NOT_FOUND.into_response()) };
    if !auth.local_login_attempt() {
        return Ok(page(StatusCode::TOO_MANY_REQUESTS, &auth, "Too many failed attempts. Try again in 15 minutes."));
    }
    // Always verify, whatever the username, so both mistakes cost the same.
    let ok = tokio::task::spawn_blocking(move || crate::auth::verify_password(&hash, &f.password) && f.username == "admin")
        .await
        .map_err(|e| crate::Error::Internal(format!("password check task: {e}")))?;
    if !ok {
        return Ok(page(StatusCode::UNAUTHORIZED, &auth, "Wrong username or password."));
    }
    auth.local_login_succeeded();
    let admin = db.local_admin().await?;
    if admin.disabled {
        return Ok(page(StatusCode::UNAUTHORIZED, &auth, "Wrong username or password."));
    }
    let cookie = db.create_session(admin.id).await?;
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/".to_string()), (header::SET_COOKIE, auth.session_cookie(Some(&cookie)))]).into_response())
}

const OIDC_COOKIE: &str = "mealier_oidc";

fn redirect_uri(auth: &Auth) -> String {
    format!("{}/auth/callback", auth.cfg.base_url.as_deref().unwrap_or_default().trim_end_matches('/'))
}

/// The cookie that ties a callback to the browser that started the login. Only the callback path ever sees it.
fn oidc_cookie(auth: &Auth, state: Option<&str>) -> String {
    let secure = if auth.cfg.secure_cookies { "; Secure" } else { "" };
    match state {
        Some(s) => format!("{OIDC_COOKIE}={s}; HttpOnly{secure}; SameSite=Lax; Path=/auth; Max-Age=600"),
        None => format!("{OIDC_COOKIE}=; HttpOnly{secure}; SameSite=Lax; Path=/auth; Max-Age=0"),
    }
}

async fn oidc_start(State(db): State<Db>, Extension(auth): Extension<Arc<Auth>>) -> Result<Response> {
    let Some(oidc) = &auth.cfg.oidc else { return Ok(StatusCode::NOT_FOUND.into_response()) };
    match oidc.begin(&db, &redirect_uri(&auth)).await {
        Ok((url, state)) => Ok((StatusCode::SEE_OTHER, [(header::LOCATION, url), (header::SET_COOKIE, oidc_cookie(&auth, Some(&state)))]).into_response()),
        Err(e) => Ok(refusal(&auth, e)),
    }
}

#[derive(serde::Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// A failed sign-in as a page. Policy refusals say why, with the status every other route gives them;
/// protocol failures are logged and shown generically.
fn refusal(auth: &Auth, e: crate::Error) -> Response {
    use crate::Error::*;
    match e {
        Forbidden(_) | TooMany(_) => page(e.status(), auth, &e.to_string()),
        Validation(m) => {
            eprintln!("mealier: {m}");
            page(StatusCode::BAD_REQUEST, auth, "Sign-in failed. Try again, and tell an admin if it keeps happening.")
        }
        other => {
            eprintln!("mealier: {other}");
            page(other.status(), auth, "Sign-in failed. Try again later.")
        }
    }
}

async fn oidc_callback(State(db): State<Db>, Extension(auth): Extension<Arc<Auth>>, headers: HeaderMap, axum::extract::Query(q): axum::extract::Query<CallbackQuery>) -> Result<Response> {
    let Some(oidc) = &auth.cfg.oidc else { return Ok(StatusCode::NOT_FOUND.into_response()) };
    let bad = |m: &str| refusal(&auth, crate::Error::Validation(m.into()));
    let cookie_state = crate::guard::cookie_value(&headers, OIDC_COOKIE);
    // The state in the URL must be the one this browser was given, or someone else's login is being finished here.
    let (Some(state), true) = (q.state.as_deref(), q.state.is_some() && q.state.as_deref() == cookie_state) else {
        return Ok(bad("callback state does not match the login cookie"));
    };
    if let Some(e) = &q.error {
        db.oidc_login_take(state).await?;
        // The value came from the query string: keep it short and free of control characters before it is logged.
        let e: String = e.chars().filter(|c| !c.is_control()).take(80).collect();
        return Ok(bad(&format!("the provider returned an error: {e}")));
    }
    let Some(code) = q.code.as_deref() else { return Ok(bad("callback has no code")) };
    let identity = match oidc.finish(&db, &redirect_uri(&auth), code, state).await {
        Ok(i) => i,
        Err(e) => return Ok(refusal(&auth, e)),
    };
    let user = match db.oidc_login(&identity).await {
        Ok(u) => u,
        Err(e) => return Ok(refusal(&auth, e)),
    };
    let session = db.create_session(user.id).await?;
    Ok((
        StatusCode::SEE_OTHER,
        AppendHeaders([(header::LOCATION, "/".to_string()), (header::SET_COOKIE, auth.session_cookie(Some(&session))), (header::SET_COOKIE, oidc_cookie(&auth, None))]),
    )
        .into_response())
}

async fn logout(State(db): State<Db>, Extension(auth): Extension<Arc<Auth>>, headers: HeaderMap) -> Result<Response> {
    if let Some(c) = session_cookie_value(&headers) {
        db.delete_session(c).await?;
    }
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/login".to_string()), (header::SET_COOKIE, auth.session_cookie(None))]).into_response())
}

/// The signed-in user.
#[utoipa::path(get, path = "/api/me", responses((status = 200, description = "The user the request is signed in as", body = User), (status = 401, description = "Not signed in", body = ErrorBody)))]
pub(crate) async fn me(CurrentUser { user, .. }: CurrentUser) -> Json<User> {
    Json(user)
}

#[utoipa::path(
    get, path = "/api/users",
    responses((status = 200, description = "Every user, by id", body = [User]), (status = 403, description = "Not an admin", body = ErrorBody))
)]
pub(crate) async fn list_users(_: Admin, State(db): State<Db>) -> Result<Json<Vec<User>>> {
    Ok(Json(db.users().await?))
}

/// Disabling yourself would lock you out of the page that re-enables you.
async fn set_disabled(db: &Db, me: &User, id: i64, disabled: bool) -> Result<User> {
    if disabled && me.id == id {
        return Err(crate::Error::Validation("you cannot disable your own account".into()));
    }
    db.set_user_disabled(id, disabled).await?;
    db.user(id).await
}

#[utoipa::path(
    post, path = "/api/users/{id}/disable", params(("id" = i64, Path, description = "User id")),
    responses(
        (status = 200, description = "Disabled; their sessions and API tokens are deleted", body = User),
        (status = 403, description = "Not an admin", body = ErrorBody),
        (status = 404, description = "No such user", body = ErrorBody),
        (status = 422, description = "Your own account", body = ErrorBody),
    )
)]
pub(crate) async fn disable_user(_: Admin, CurrentUser { user: me, .. }: CurrentUser, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<User>> {
    Ok(Json(set_disabled(&db, &me, id, true).await?))
}

#[utoipa::path(
    post, path = "/api/users/{id}/enable", params(("id" = i64, Path, description = "User id")),
    responses(
        (status = 200, description = "Enabled; they must sign in again", body = User),
        (status = 403, description = "Not an admin", body = ErrorBody),
        (status = 404, description = "No such user", body = ErrorBody),
    )
)]
pub(crate) async fn enable_user(_: Admin, CurrentUser { user: me, .. }: CurrentUser, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<User>> {
    Ok(Json(set_disabled(&db, &me, id, false).await?))
}

#[derive(Template, WebTemplate)]
#[template(path = "users.html")]
struct UsersPage {
    users: Vec<User>,
    me: i64,
}

async fn users_page(_: Admin, CurrentUser { user, .. }: CurrentUser, State(db): State<Db>) -> Result<UsersPage> {
    Ok(UsersPage { users: db.users().await?, me: user.id })
}

async fn user_disable(_: Admin, CurrentUser { user: me, .. }: CurrentUser, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    set_disabled(&db, &me, id, true).await?;
    Ok(Redirect::to("/settings/users"))
}

async fn user_enable(_: Admin, CurrentUser { user: me, .. }: CurrentUser, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    set_disabled(&db, &me, id, false).await?;
    Ok(Redirect::to("/settings/users"))
}

struct Created {
    name: String,
    secret: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "tokens.html")]
struct TokensPage {
    tokens: Vec<ApiToken>,
    created: Option<Created>,
}

async fn tokens_page(SessionUser(user): SessionUser, State(db): State<Db>) -> Result<TokensPage> {
    Ok(TokensPage { tokens: db.api_tokens(user.id).await?, created: None })
}

#[derive(serde::Deserialize)]
struct NewToken {
    name: String,
}

/// The only response that ever carries a token's secret; it must not be cached.
async fn token_create(SessionUser(user): SessionUser, State(db): State<Db>, axum::Form(f): axum::Form<NewToken>) -> Result<Response> {
    let (row, secret) = db.create_api_token(user.id, &f.name).await?;
    let page = TokensPage { tokens: db.api_tokens(user.id).await?, created: Some(Created { name: row.name, secret }) };
    Ok(([(header::CACHE_CONTROL, "no-store")], page).into_response())
}

async fn token_revoke(SessionUser(user): SessionUser, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.revoke_api_token(user.id, id).await?;
    Ok(Redirect::to("/settings/tokens"))
}
