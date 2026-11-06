//! Batch pages: starting one, editing its lines, finishing it, photos.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Multipart, Path, State};
use axum::response::Redirect;
use axum::routing::{get, post};
use serde::Deserialize;

use super::{amount, id_of, num, number, opt};
use crate::api::{AppState, PHOTO_BODY_LIMIT};
use crate::images::read_photo;
use crate::{BatchInput, BatchLineInput, Db, Error, Result, Scope};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/recipes/{id}/batches", post(start))
        .route("/batches/{id}", get(batch_page).post(save))
        .route("/batches/{id}/lines", post(add_line))
        .route("/batches/{id}/done", post(done))
        .route("/batches/{id}/photo", post(photo).layer(axum::extract::DefaultBodyLimit::max(PHOTO_BODY_LIMIT)))
}

#[derive(Deserialize)]
struct StartForm {
    scale: f64,
}

async fn start(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<StartForm>) -> Result<Redirect> {
    let b = db.start_batch(&sc, id, f.scale).await?;
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
    lists: Vec<crate::ShoppingListSummary>,
}

async fn batch_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<BatchPage> {
    let b = db.get_batch(&sc, id).await?;
    let recipe = db.get_recipe(&sc, b.batch.recipe_id).await?.ok_or(Error::NotFound("recipe"))?;
    let foods = if b.lines.iter().any(|l| l.added) { db.foods(&sc).await? } else { Vec::new() };
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
        units: db.units(&sc).await?,
        foods,
        photos: b.photos.iter().map(|p| p.file_name()).collect(),
        lists: db.shopping_lists(&sc).await?,
    })
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

async fn save_form(db: &Db, sc: &Scope, id: i64, body: &[u8]) -> Result<()> {
    db.update_batch(sc, id, parse_form(body)?).await?;
    Ok(())
}

async fn save(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    save_form(&db, &sc, id, &body).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}

async fn add_line(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    save_form(&db, &sc, id, &body).await?;
    db.add_batch_line(&sc, id, BatchLineInput::default()).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}

async fn done(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    save_form(&db, &sc, id, &body).await?;
    db.finish_batch(&sc, id).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}

async fn photo(sc: Scope, State(s): State<AppState>, Path(id): Path<i64>, form: Multipart) -> Result<Redirect> {
    let bytes = read_photo(form).await?;
    s.db.add_batch_photo(&sc, id, &s.images_dir, &bytes).await?;
    Ok(Redirect::to(&format!("/batches/{id}")))
}
