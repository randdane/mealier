//! What the router needs to know about authentication: config, middleware, extractors.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, ORIGIN};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::{Db, Error, OidcConfig, SESSION_COOKIE, Scope, User};

pub struct AuthConfig {
    /// `MEALIER_BASE_URL`: the public URL; its origin is what a form POST must come from.
    pub base_url: Option<String>,
    /// False only for local http development (`MEALIER_INSECURE_COOKIES=1`).
    pub secure_cookies: bool,
    /// Argon2id PHC string of the break-glass admin password; `None` disables local login.
    pub admin_hash: Option<String>,
    pub oidc: Option<OidcConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig { base_url: None, secure_cookies: true, admin_hash: None, oidc: None }
    }
}

/// The config plus the one piece of mutable auth state: failed local logins.
pub(crate) struct Auth {
    pub cfg: AuthConfig,
    failures: Mutex<VecDeque<Instant>>,
}

const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

impl Auth {
    pub fn new(cfg: AuthConfig) -> Auth {
        Auth { cfg, failures: Mutex::new(VecDeque::new()) }
    }

    /// Books one local login attempt as a failure up front, so a burst of parallel guesses cannot all
    /// slip under the limit while the first is still hashing. False once `MAX_FAILURES` are on the
    /// books within the window; a correct password is refused then too.
    // ponytail: one counter for the whole process, in memory. An attacker can lock the break-glass
    // login out for 15 minutes, and a restart clears it. Per-IP or persistent limits if that ever matters.
    pub fn local_login_attempt(&self) -> bool {
        let mut f = self.failures.lock().unwrap();
        while f.front().is_some_and(|t| t.elapsed() > FAILURE_WINDOW) {
            f.pop_front();
        }
        if f.len() >= MAX_FAILURES {
            return false;
        }
        f.push_back(Instant::now());
        true
    }

    /// The attempt booked by `local_login_attempt` turned out fine: take it back off the books.
    pub fn local_login_succeeded(&self) {
        self.failures.lock().unwrap().pop_back();
    }

    /// `Set-Cookie` value for a session (`None` clears it).
    pub fn session_cookie(&self, value: Option<&str>) -> String {
        let secure = if self.cfg.secure_cookies { "; Secure" } else { "" };
        match value {
            Some(v) => format!("{SESSION_COOKIE}={v}; HttpOnly{secure}; SameSite=Lax; Path=/; Max-Age={}", crate::SESSION_DAYS * 86400),
            None => format!("{SESSION_COOKIE}=; HttpOnly{secure}; SameSite=Lax; Path=/; Max-Age=0"),
        }
    }
}

/// The signed-in user, put in the request by `guard`.
#[derive(Clone)]
pub(crate) struct CurrentUser {
    pub user: User,
    /// Signed in with an API token rather than a browser session.
    pub via_token: bool,
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = Error;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Error> {
        parts.extensions.get::<CurrentUser>().cloned().ok_or(Error::Unauthorized)
    }
}

/// The tenant a handler may touch. Taking this as an argument is how a handler gets at group- or
/// household-scoped data: the store methods all want one.
impl<S: Send + Sync> FromRequestParts<S> for Scope {
    type Rejection = Error;
    async fn from_request_parts(parts: &mut Parts, s: &S) -> Result<Self, Error> {
        Ok(Scope::from(&CurrentUser::from_request_parts(parts, s).await?.user))
    }
}

/// A signed-in admin. Taking this as an argument is what makes a handler admin-only.
pub(crate) struct Admin;

impl<S: Send + Sync> FromRequestParts<S> for Admin {
    type Rejection = Error;
    async fn from_request_parts(parts: &mut Parts, s: &S) -> Result<Self, Error> {
        let CurrentUser { user: u, .. } = CurrentUser::from_request_parts(parts, s).await?;
        if u.role == "admin" { Ok(Admin) } else { Err(Error::Forbidden("admins only".into())) }
    }
}

/// A user signed in through a browser session; API tokens are refused. For managing tokens themselves.
pub(crate) struct SessionUser(pub User);

impl<S: Send + Sync> FromRequestParts<S> for SessionUser {
    type Rejection = Error;
    async fn from_request_parts(parts: &mut Parts, s: &S) -> Result<Self, Error> {
        match CurrentUser::from_request_parts(parts, s).await? {
            CurrentUser { via_token: true, .. } => Err(Error::Forbidden("sign in with the browser to manage tokens".into())),
            CurrentUser { user, .. } => Ok(SessionUser(user)),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Guard {
    pub db: Db,
    pub auth: Arc<Auth>,
}

/// Reachable without a user. Whole path segments only: `/loginx` is not `/login`.
fn is_public(path: &str) -> bool {
    let under = |p: &str| path == p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/'));
    path == "/healthz" || path == "/auth/callback" || under("/login") || path.starts_with("/static/")
}

/// Belt over the router: the admin-only areas are refused here even if a handler forgets `Admin`.
fn is_admin_area(path: &str) -> bool {
    ["/api/webhooks", "/settings/webhooks", "/api/users", "/settings/users", "/api/llm", "/settings/llm", "/api/groups", "/api/households", "/api/default-household", "/settings/households"].iter().any(|p| path == *p || path.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
}

pub(crate) fn session_cookie_value(headers: &HeaderMap) -> Option<&str> {
    cookie_value(headers, SESSION_COOKIE)
}

/// The value of cookie `name`, from any `Cookie` header.
pub(crate) fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(name).and_then(|r| r.strip_prefix('=')))
}

fn bearer(value: &axum::http::HeaderValue) -> Option<&str> {
    let v = value.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

/// Cross-site request forgery check for requests that are not bearer-authenticated.
/// `Origin`, when present, must be ours; without it `Sec-Fetch-Site` must say same-origin or none.
fn csrf_refused(cfg: &AuthConfig, h: &HeaderMap) -> bool {
    if let Some(origin) = h.get(ORIGIN) {
        let Some(origin) = origin.to_str().ok().and_then(|o| url::Url::parse(o).ok()).filter(|u| u.has_host()) else { return true };
        let theirs = origin.origin().ascii_serialization();
        return match cfg.base_url.as_deref().and_then(|b| url::Url::parse(b).ok()) {
            Some(base) => theirs != base.origin().ascii_serialization(),
            // No base URL configured: the browser's Origin must at least match the Host it addressed.
            None => {
                let authority = origin.port().map_or_else(|| origin.host_str().unwrap_or_default().to_string(), |p| format!("{}:{p}", origin.host_str().unwrap_or_default()));
                h.get(HOST).and_then(|v| v.to_str().ok()).is_none_or(|host| !host.eq_ignore_ascii_case(&authority))
            }
        };
    }
    match h.get("sec-fetch-site") {
        None => false,
        Some(v) => !matches!(v.to_str(), Ok("same-origin" | "none")),
    }
}

fn refuse(e: Error, html_redirect: bool) -> Response {
    if html_redirect {
        (StatusCode::SEE_OTHER, [(header::LOCATION, "/login")]).into_response()
    } else {
        e.into_response()
    }
}

/// The authentication middleware; wraps every route.
pub(crate) async fn guard(State(g): State<Guard>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let api = path.starts_with("/api/");
    let state_changing = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let authz = req.headers().get(AUTHORIZATION).cloned();

    // A request that carries an Authorization header cannot be forged cross-site (it would need a CORS preflight).
    if state_changing && authz.is_none() && csrf_refused(&g.auth.cfg, req.headers()) {
        return Error::Forbidden("cross-site request refused".into()).into_response();
    }
    if is_public(&path) {
        return next.run(req).await;
    }

    // An Authorization header decides alone: a bad token never falls back to the cookie.
    let found = match &authz {
        Some(v) => match bearer(v) {
            Some(t) => g.db.token_user(t).await,
            None => Ok(None),
        },
        None => match session_cookie_value(req.headers()) {
            Some(c) => g.db.session_user(c).await,
            None => Ok(None),
        },
    };
    let user = match found {
        Ok(Some(u)) => u,
        Ok(None) => return refuse(Error::Unauthorized, !api),
        Err(e) => return e.into_response(),
    };
    if user.role != "admin" && is_admin_area(&path) {
        return Error::Forbidden("admins only".into()).into_response();
    }
    req.extensions_mut().insert(CurrentUser { user, via_token: authz.is_some() });
    next.run(req).await
}

impl AuthConfig {
    /// Reads the process environment; see `from_vars`.
    pub fn from_env() -> crate::Result<AuthConfig> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// Reads the settings through `get`, and the secret files they name. Fails at startup, never at login.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> crate::Result<AuthConfig> {
        use crate::Error::Validation;
        let var = |k: &str| get(k).filter(|v| !v.is_empty());
        let http_url = |name: &str, v: &str| match url::Url::parse(v) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.has_host() => Ok(()),
            _ => Err(Validation(format!("{name} must be an http(s) URL"))),
        };
        // Secrets come from files so they never sit in the environment; the error names the file, not its content.
        let read = |name: &str, path: String| match std::fs::read_to_string(&path) {
            Ok(c) if !c.trim().is_empty() => Ok(c.trim().to_string()),
            Ok(_) => Err(Validation(format!("{name} ({path}) is empty"))),
            Err(e) => Err(Validation(format!("cannot read {name} ({path}): {}", e.kind()))),
        };

        let base_url = var("MEALIER_BASE_URL");
        if let Some(b) = &base_url {
            http_url("MEALIER_BASE_URL", b)?;
        }
        let admin_hash = match var("MEALIER_ADMIN_PASSWORD_FILE") {
            Some(path) => {
                let h = read("MEALIER_ADMIN_PASSWORD_FILE", path)?;
                if !(h.starts_with("$argon2id$") && argon2::password_hash::phc::PasswordHash::new(&h).is_ok()) {
                    return Err(Validation("MEALIER_ADMIN_PASSWORD_FILE must hold an argon2id hash (make one with `mealier hash-password`)".into()));
                }
                Some(h)
            }
            None => None,
        };
        let oidc = match var("OIDC_CONFIGURATION_URL") {
            Some(config_url) => {
                http_url("OIDC_CONFIGURATION_URL", &config_url)?;
                let need = |k: &str| var(k).ok_or_else(|| Validation(format!("{k} is required when OIDC_CONFIGURATION_URL is set")));
                if base_url.is_none() {
                    need("MEALIER_BASE_URL")?;
                }
                Some(OidcConfig {
                    config_url,
                    client_id: need("OIDC_CLIENT_ID")?,
                    client_secret: read("OIDC_CLIENT_SECRET_FILE", need("OIDC_CLIENT_SECRET_FILE")?)?,
                    user_group: var("OIDC_USER_GROUP").unwrap_or_else(|| "mealier-users".into()),
                    admin_group: var("OIDC_ADMIN_GROUP").unwrap_or_else(|| "mealier-admins".into()),
                })
            }
            None => None,
        };
        Ok(AuthConfig { base_url, secure_cookies: var("MEALIER_INSECURE_COOKIES").as_deref() != Some("1"), admin_hash, oidc })
    }
}
