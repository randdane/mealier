//! Server-rendered pages (askama + htmx). Hand-checked; the JSON API is the tested surface.
//! One module per area; this one has the router and what they share.
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;

use crate::api::AppState;
use crate::{Error, Result, Scope};

mod batches;
mod cookbooks;
mod planner;
mod recipes;
mod settings;
mod shopping;
mod share;

pub(crate) use recipes::estimate_recipe;
pub(crate) use share::share_page;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .merge(recipes::routes())
        .merge(batches::routes())
        .merge(cookbooks::routes())
        .merge(planner::routes())
        .merge(shopping::routes())
        .merge(settings::routes())
        .route("/images/{name}", get(image))
        .route("/static/htmx.min.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_str!("../../static/htmx.min.js")) }))
        .route("/static/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("../../static/app.css")) }))
}

/// 250, 1.5, 0.333: no trailing zeros.
fn num(n: f64) -> String {
    let s = format!("{n:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn opt(n: Option<f64>) -> String {
    n.map(num).unwrap_or_default()
}

/// "2-4 g", "1 egg"-style amount text; empty when there is no amount.
fn amount(min: Option<f64>, max: Option<f64>, unit: Option<&str>) -> String {
    let q = match (min, max) {
        (Some(a), Some(b)) if a != b => format!("{}-{}", num(a), num(b)),
        (Some(a), _) => num(a),
        _ => String::new(),
    };
    [q.as_str(), unit.unwrap_or_default()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ")
}

fn number(v: &str) -> Result<Option<f64>> {
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    v.parse().map(Some).map_err(|_| Error::Validation(format!("'{v}' is not a number")))
}

fn id_of(v: &str) -> Result<Option<i64>> {
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    v.parse().map(Some).map_err(|_| Error::Validation(format!("'{v}' is not an id")))
}

type Svc = axum::Extension<std::sync::Arc<crate::ImportService>>;

/// Serves `<64 hex>.<ext>` from the images dir to the tenant that uses the image; anything else is 404.
/// The type comes from `images.mime`, which the extension must agree with.
async fn image(sc: Scope, State(s): State<AppState>, Path(name): Path<String>) -> impl IntoResponse {
    let Some((hash, ext)) = name.split_once('.').filter(|(h, _)| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // The file name is the hash; whose it is, is in the database.
    let Ok(Some(mime)) = s.db.image_mime(&sc, hash).await else { return StatusCode::NOT_FOUND.into_response() };
    if crate::images::ext(&mime) != Some(ext) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(s.images_dir.join(&name)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable".into())], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A urlencoded body as its key/value pairs, repeats included (a checkbox group sends the key once per tick).
fn pairs(body: &[u8]) -> Result<Vec<(String, String)>> {
    serde_urlencoded::from_bytes(body).map_err(|e| Error::Validation(format!("bad form: {e}")))
}
