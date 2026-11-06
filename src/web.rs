//! Server-rendered pages (askama + htmx). Hand-checked; the JSON API is the tested surface.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::api::{AppState, PHOTO_BODY_LIMIT, read_photo};
use crate::{BatchInput, BatchLineInput, Db, Error, Result};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/recipes/{id}", get(recipe_page))
        .route("/recipes/{id}/batches", post(start))
        .route("/batches/{id}", get(batch_page).post(save))
        .route("/batches/{id}/lines", post(add_line))
        .route("/batches/{id}/done", post(done))
        .route(
            "/batches/{id}/photo",
            post(photo).layer(axum::extract::DefaultBodyLimit::max(PHOTO_BODY_LIMIT)),
        )
        .route("/images/{name}", get(image))
        .route("/static/htmx.min.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_str!("../static/htmx.min.js")) }))
        .route("/static/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("../static/app.css")) }))
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

#[derive(Template, WebTemplate)]
#[template(path = "index.html")]
struct Index {
    q: String,
    recipes: Vec<crate::RecipeSummary>,
}

#[derive(Deserialize)]
struct Search {
    q: Option<String>,
}

async fn index(State(db): State<Db>, Query(s): Query<Search>) -> Result<Index> {
    let q = s.q.unwrap_or_default();
    let recipes = db.list_recipes(false, Some(q.trim()).filter(|q| !q.is_empty())).await?;
    Ok(Index { q, recipes })
}

struct BatchRow {
    id: i64,
    date: String,
    status: String,
    scale: String,
    rating: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "recipe.html")]
struct RecipePage {
    id: i64,
    name: String,
    description: String,
    ingredients: Vec<String>,
    steps: Vec<String>,
    batches: Vec<BatchRow>,
}

async fn recipe_page(State(db): State<Db>, Path(id): Path<i64>) -> Result<RecipePage> {
    let r = db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?;
    let ingredients = r
        .content
        .ingredients
        .iter()
        .map(|i| {
            let what = [i.food_name.as_deref().unwrap_or_default(), &i.note];
            let what = what.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(", ");
            let line = [amount(i.qty_min, i.qty_max, i.unit_name.as_deref()), what].join(" ");
            if line.trim().is_empty() { i.original_text.clone() } else { line.trim().to_string() }
        })
        .collect();
    let batches = db
        .list_batches(id)
        .await?
        .into_iter()
        .map(|b| BatchRow {
            id: b.id,
            date: b.made_on.unwrap_or_default(),
            status: b.status,
            scale: num(b.scale),
            rating: b.rating.map(|r| r.to_string()).unwrap_or_default(),
        })
        .collect();
    Ok(RecipePage { id, name: r.content.name, description: r.content.description, ingredients, steps: r.content.steps, batches })
}

#[derive(Deserialize)]
struct StartForm {
    scale: f64,
}

async fn start(State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<StartForm>) -> Result<Redirect> {
    let b = db.start_batch(id, f.scale).await?;
    Ok(Redirect::to(&format!("/batches/{}", b.batch.id)))
}

/// Ids use 0 for "none" so the template can compare plainly.
struct LineRow {
    id: i64,
    added: bool,
    ingredient: String,
    recipe: String,
    qty_min: String,
    qty_max: String,
    unit_id: i64,
    food_id: i64,
    substitution: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "batch.html")]
struct BatchPage {
    id: i64,
    recipe_id: i64,
    recipe_name: String,
    status: String,
    planned: bool,
    made_on: String,
    scale: String,
    rating: i64,
    notes: String,
    lines: Vec<LineRow>,
    units: Vec<crate::Unit>,
    foods: Vec<crate::Food>,
    photos: Vec<String>,
}

async fn batch_page(State(db): State<Db>, Path(id): Path<i64>) -> Result<BatchPage> {
    let b = db.get_batch(id).await?;
    let recipe = db.get_recipe(b.batch.recipe_id).await?.ok_or(Error::NotFound("recipe"))?;
    let foods = if b.lines.iter().any(|l| l.added) { db.foods().await? } else { Vec::new() };
    let lines = b
        .lines
        .iter()
        .map(|l| LineRow {
            id: l.id,
            added: l.added,
            ingredient: l.ingredient.clone(),
            recipe: amount(l.recipe_qty_min, l.recipe_qty_max, l.recipe_unit_name.as_deref()),
            qty_min: opt(l.qty_min),
            qty_max: opt(l.qty_max),
            unit_id: l.unit_id.unwrap_or(0),
            food_id: l.food_id.unwrap_or(0),
            substitution: l.substitution.clone(),
        })
        .collect();
    Ok(BatchPage {
        id,
        recipe_id: b.batch.recipe_id,
        recipe_name: recipe.content.name,
        planned: b.batch.status == "planned",
        status: b.batch.status,
        made_on: b.batch.made_on.unwrap_or_default(),
        scale: num(b.batch.scale),
        rating: b.batch.rating.unwrap_or(0),
        notes: b.batch.notes,
        lines,
        units: db.units().await?,
        foods,
        photos: b.photos.iter().map(|p| p.file_name()).collect(),
    })
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

/// The batch form is a flat list of pairs: header fields, then one run of
/// fields per row, each run starting at `line_id`. Rows ticked `remove` are left out.
fn parse_form(body: &[u8]) -> Result<BatchInput> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body).map_err(|e| Error::Validation(e.to_string()))?;
    let mut input = BatchInput::default();
    let mut removed = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "made_on" => input.made_on = Some(v).filter(|v| !v.is_empty()),
            "scale" => input.scale = number(&v)?.unwrap_or(0.0),
            "rating" => input.rating = id_of(&v)?,
            "notes" => input.notes = v,
            "line_id" => {
                input.lines.push(BatchLineInput { id: id_of(&v)?, ..Default::default() });
                removed.push(false);
            }
            _ => {
                let (Some(line), Some(rm)) = (input.lines.last_mut(), removed.last_mut()) else { continue };
                match k.as_str() {
                    "qty_min" => line.qty_min = number(&v)?,
                    "qty_max" => line.qty_max = number(&v)?,
                    "unit_id" => line.unit_id = id_of(&v)?,
                    "food_id" => line.food_id = id_of(&v)?,
                    "substitution" => line.substitution = v,
                    "remove" => *rm = true,
                    _ => {}
                }
            }
        }
    }
    let mut keep = removed.into_iter().map(|r| !r);
    input.lines.retain(|_| keep.next().unwrap_or(true));
    Ok(input)
}

async fn save_form(db: &Db, id: i64, body: &[u8]) -> Result<()> {
    db.update_batch(id, parse_form(body)?).await?;
    Ok(())
}

async fn save(State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    save_form(&db, id, &body).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}

async fn add_line(State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    save_form(&db, id, &body).await?;
    db.add_batch_line(id, BatchLineInput::default()).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}

async fn done(State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    save_form(&db, id, &body).await?;
    db.finish_batch(id).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}

async fn photo(State(s): State<AppState>, Path(id): Path<i64>, form: Multipart) -> Result<Redirect> {
    let bytes = read_photo(form).await?;
    s.db.add_batch_photo(id, &s.images_dir, &bytes).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}

/// Serves `<64 hex>.<png|jpg|webp|gif>` from the images dir; anything else is 404.
async fn image(State(s): State<AppState>, Path(name): Path<String>) -> impl IntoResponse {
    let ok = name.split_once('.').and_then(|(h, ext)| {
        let hex = h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let mime = match ext {
            "png" => "image/png",
            "jpg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => return None,
        };
        hex.then_some(mime)
    });
    let Some(mime) = ok else { return StatusCode::NOT_FOUND.into_response() };
    match tokio::fs::read(s.images_dir.join(&name)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
