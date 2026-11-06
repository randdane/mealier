//! Step 14, part B: share links. A link gives read-only access to one recipe at `/share/<token>` with
//! no login. The token is 32 random bytes (base64url), shown once and stored only as its SHA-256.
use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::{AppState, ErrorBody};
use sqlx::Row;

use crate::{Db, Error, Recipe, Result, Scope, hash_token};

pub const DEFAULT_SHARE_DAYS: i64 = 30;
pub const MAX_SHARE_DAYS: i64 = 365;

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ShareLink {
    pub id: i64,
    pub recipe_id: i64,
    pub created_at: String,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub expires_at: String,
}

/// A link just made: the only time its token is known.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct NewShareLink {
    #[serde(flatten)]
    pub link: ShareLink,
    pub token: String,
    /// Where the recipe is shown, relative to the site.
    pub path: String,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ShareInput {
    /// Days until it expires: 1 to 365, default 30.
    pub days: Option<i64>,
}

/// 32 random bytes as unpadded base64url.
fn new_token() -> Result<String> {
    use base64::Engine;
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| Error::Internal(e.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

fn link_from(r: &sqlx::sqlite::SqliteRow) -> ShareLink {
    ShareLink { id: r.get(0), recipe_id: r.get(1), created_at: r.get(2), expires_at: r.get(3) }
}

impl Db {
    /// For a recipe of the caller's group. The token comes back here and nowhere else.
    pub async fn create_share_link(&self, sc: &Scope, recipe_id: i64, days: Option<i64>) -> Result<NewShareLink> {
        let days = days.unwrap_or(DEFAULT_SHARE_DAYS);
        if !(1..=MAX_SHARE_DAYS).contains(&days) {
            return Err(Error::Validation(format!("a share link lasts 1 to {MAX_SHARE_DAYS} days")));
        }
        self.check_recipe(sc, recipe_id).await?;
        let token = new_token()?;
        let row = sqlx::query(&format!(
            "INSERT INTO share_links (group_id, recipe_id, token_hash, created_by, created_at, expires_at) VALUES (?, ?, ?, ?, {NOW}, strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '+' || ? || ' days')) RETURNING id, recipe_id, created_at, expires_at"
        ))
        .bind(sc.group_id)
        .bind(recipe_id)
        .bind(hash_token(&token))
        .bind(Some(sc.user_id).filter(|u| *u > 0))
        .bind(days)
        .fetch_one(&self.pool)
        .await?;
        Ok(NewShareLink { link: link_from(&row), path: format!("/share/{token}"), token })
    }

    /// The recipe's links, newest first (expired ones too, so they can be cleared away).
    pub async fn share_links(&self, sc: &Scope, recipe_id: i64) -> Result<Vec<ShareLink>> {
        self.check_recipe(sc, recipe_id).await?;
        let rows = sqlx::query("SELECT id, recipe_id, created_at, expires_at FROM share_links WHERE recipe_id = ? AND group_id = ? ORDER BY id DESC").bind(recipe_id).bind(sc.group_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(link_from).collect())
    }

    pub async fn revoke_share_link(&self, sc: &Scope, id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM share_links WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("share link"));
        }
        Ok(())
    }

    /// The recipe a token opens; `NotFound` for an unknown, expired or revoked token (and for an archived recipe).
    pub async fn shared_recipe(&self, token: &str) -> Result<Recipe> {
        Ok(self.shared(token).await?.1)
    }

    /// The group's scope the link grants (no user, no household) and its recipe.
    pub(crate) async fn shared(&self, token: &str) -> Result<(Scope, Recipe)> {
        let missing = Error::NotFound("share link");
        if token.is_empty() || token.len() > 128 {
            return Err(missing);
        }
        let row: Option<(i64, i64)> = sqlx::query_as(&format!("SELECT l.group_id, l.recipe_id FROM share_links l JOIN recipes r ON r.id = l.recipe_id WHERE l.token_hash = ? AND l.expires_at > {NOW} AND r.archived = 0"))
            .bind(hash_token(token))
            .fetch_optional(&self.pool)
            .await?;
        let (group_id, recipe_id) = row.ok_or_else(|| Error::NotFound("share link"))?;
        let sc = Scope { user_id: 0, group_id, household_id: 0, admin: false };
        let recipe = self.get_recipe(&sc, recipe_id).await?.ok_or(missing)?;
        Ok((sc, recipe))
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Characters a token can be made of (ours are base64url; Mealie's are UUIDs).
fn plain_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= 128 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The paths served without a user: `/share/<token>`, its `assets/<hash>` and `images/<name>`, and
/// Mealie's `/g/<group>/shared/r/<token>`. Whole segments, none empty.
pub(crate) fn is_public_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else { return false };
    let segs: Vec<&str> = rest.split('/').collect();
    if segs.iter().any(|s| s.is_empty()) {
        return false;
    }
    matches!(segs.as_slice(), ["share", _] | ["share", _, "assets" | "images", _] | ["g", _, "shared", "r", _])
}

pub(crate) fn routes() -> Router<AppState> {
    let public = Router::new()
        .route("/share/{token}", get(page))
        .route("/share/{token}/assets/{hash}", get(asset))
        .route("/share/{token}/images/{name}", get(image))
        .route("/g/{group}/shared/r/{token}", get(mealie_link))
        .layer(axum::middleware::map_response(private));
    Router::new().merge(public).route("/api/recipes/{id}/share-links", get(list_links).post(create_link)).route("/api/share-links/{id}", delete(revoke_link))
}

/// Nothing about a share page may be cached or leak the token through a `Referer`.
async fn private(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    r
}

/// Expired, revoked, unknown and malformed all look like this, byte for byte.
fn not_found() -> Response {
    const PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Not found</title></head><body><main><h1>404 Not Found</h1><p>This link does not exist or has expired.</p></main></body></html>";
    (StatusCode::NOT_FOUND, Html(PAGE)).into_response()
}

/// A missing link is the 404 page; anything else is a plain 500 (the usual error page has the site's navigation).
fn answer(r: Result<Response>) -> Response {
    match r {
        Ok(r) => r,
        Err(Error::NotFound(_)) => not_found(),
        Err(e) => {
            eprintln!("mealier: share: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn page(UrlPath(token): UrlPath<String>, State(db): State<Db>) -> Response {
    answer(async {
        let (sc, recipe) = db.shared(&token).await?;
        Ok(crate::web::share_page(&db, &sc, recipe, &token).await?.into_response())
    }
    .await)
}

async fn asset(UrlPath((token, hash)): UrlPath<(String, String)>, State(s): State<AppState>) -> Response {
    answer(async {
        let (sc, recipe) = s.db.shared(&token).await?;
        if !recipe.content.settings.show_assets || !is_hex64(&hash) {
            return Err(Error::NotFound("asset"));
        }
        let asset = s.db.recipe_asset(&sc, recipe.id, &hash).await?;
        let bytes = tokio::fs::read(s.assets_dir().join(&hash)).await.map_err(|_| Error::NotFound("asset"))?;
        Ok(crate::features::asset_response(&asset.file_name, bytes))
    }
    .await)
}

async fn image(UrlPath((token, name)): UrlPath<(String, String)>, State(s): State<AppState>) -> Response {
    answer(async {
        let (sc, recipe) = s.db.shared(&token).await?;
        let img = s.db.recipe_images(&sc, recipe.id).await?.into_iter().find(|i| i.file_name() == name).ok_or(Error::NotFound("image"))?;
        let bytes = tokio::fs::read(s.images_dir.join(&name)).await.map_err(|_| Error::NotFound("image"))?;
        Ok(([(header::CONTENT_TYPE, img.mime)], bytes).into_response())
    }
    .await)
}

/// Mealie's own address for a shared recipe, so old links keep working.
async fn mealie_link(UrlPath((_group, token)): UrlPath<(String, String)>) -> Response {
    if !plain_token(&token) {
        return not_found();
    }
    Redirect::temporary(&format!("/share/{token}")).into_response()
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/share-links", params(("id" = i64, Path, description = "Recipe id")),
    responses((status = 200, description = "The recipe's share links, newest first. Tokens are not kept, so they are not listed", body = [ShareLink]), (status = 404, description = "No such recipe", body = ErrorBody))
)]
pub(crate) async fn list_links(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>) -> Result<Json<Vec<ShareLink>>> {
    Ok(Json(db.share_links(&sc, id).await?))
}

#[utoipa::path(
    post, path = "/api/recipes/{id}/share-links", params(("id" = i64, Path, description = "Recipe id")), request_body(content = ShareInput, description = "Optional; an empty body means 30 days"),
    responses(
        (status = 201, description = "Made. `token` is shown only here; anyone holding `/share/<token>` can read the recipe until `expires_at` or until the link is revoked", body = NewShareLink),
        (status = 404, description = "No such recipe", body = ErrorBody),
        (status = 422, description = "`days` is not 1 to 365", body = ErrorBody),
    )
)]
pub(crate) async fn create_link(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>, body: Option<Json<ShareInput>>) -> Result<(StatusCode, Json<NewShareLink>)> {
    Ok((StatusCode::CREATED, Json(db.create_share_link(&sc, id, body.and_then(|b| b.0.days)).await?)))
}

#[utoipa::path(
    delete, path = "/api/share-links/{id}", params(("id" = i64, Path, description = "Share link id")),
    responses((status = 204, description = "Revoked: its address answers 404 from now on"), (status = 404, description = "No such link in the caller's group", body = ErrorBody))
)]
pub(crate) async fn revoke_link(sc: Scope, State(db): State<Db>, UrlPath(id): UrlPath<i64>) -> Result<StatusCode> {
    db.revoke_share_link(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
