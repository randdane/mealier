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
use crate::{BatchInput, BatchLineInput, Db, Dimension, Error, IngredientInput, RecipeInput, Result, parse_line};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/recipes", post(create_recipe))
        .route("/recipes/import", post(import_form))
        .route("/recipes/new", get(new_recipe_page))
        .route("/recipes/{id}", get(recipe_page))
        .route("/recipes/{id}/edit", get(edit_recipe_page).post(edit_recipe))
        .route("/recipes/{id}/ingredients/review", get(review_page).post(review_save))
        .route("/recipes/{id}/cook", get(cook_page))
        .route("/settings/cook-theme", post(set_theme))
        .route("/recipes/{id}/batches", post(start))
        .route("/batches/{id}", get(batch_page).post(save))
        .route("/batches/{id}/lines", post(add_line))
        .route("/batches/{id}/done", post(done))
        .route(
            "/batches/{id}/photo",
            post(photo).layer(axum::extract::DefaultBodyLimit::max(PHOTO_BODY_LIMIT)),
        )
        .route("/plan", get(plan_page))
        .route("/plan/entries", post(plan_add))
        .route("/plan/entries/{id}/delete", post(plan_delete))
        .route("/lists", get(lists_page).post(lists_create))
        .route("/lists/{id}", get(list_page))
        .route("/lists/{id}/items", post(list_add_item))
        .route("/lists/add-recipe", post(list_add_recipe))
        .route("/lists/add-batch", post(list_add_batch))
        .route("/lists/items/{id}/check", post(item_check))
        .route("/lists/items/{id}/delete", post(item_delete))
        // step 8: admin only
        .route("/settings/webhooks", get(webhooks_page).post(webhook_create))
        .route("/settings/webhooks/{id}/delete", post(webhook_delete))
        .route("/settings/webhooks/{id}/toggle", post(webhook_toggle))
        .route("/settings/webhooks/{id}/test", post(webhook_test))
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
    lists: Vec<crate::ShoppingListSummary>,
    slots: [&'static str; 7],
    today: String,
    name: String,
    description: String,
    ingredients: Vec<String>,
    steps: Vec<String>,
    batches: Vec<BatchRow>,
}

async fn recipe_page(State(db): State<Db>, Path(id): Path<i64>) -> Result<RecipePage> {
    let r = db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?;
    let ingredients = r.content.ingredients.iter().map(|i| ingredient_line(i, 1.0)).collect();
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
    Ok(RecipePage { id, lists: db.shopping_lists().await?, slots: crate::SLOTS, today: db.day("now", 0).await?, name: r.content.name, description: r.content.description, ingredients, steps: r.content.steps, batches })
}

/// One ingredient as text, amounts times `scale`; falls back to the original text.
fn ingredient_line(i: &crate::Ingredient, scale: f64) -> String {
    let what = [i.food_name.as_deref().unwrap_or_default(), &i.note];
    let what = what.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(", ");
    let q = |n: Option<f64>| n.map(|n| n * scale);
    let line = [amount(q(i.qty_min), q(i.qty_max), i.unit_name.as_deref()), what].join(" ");
    if line.trim().is_empty() { i.original_text.clone() } else { line.trim().to_string() }
}

#[derive(Template, WebTemplate)]
#[template(path = "cook.html")]
struct CookPage {
    name: String,
    ingredients: Vec<String>,
    steps: Vec<String>,
    theme: String,
    /// Where "back" goes, and where the theme form returns to.
    back: String,
    here: String,
    themes: [&'static str; 3],
}

#[derive(Deserialize)]
struct CookQuery {
    scale: Option<f64>,
    batch: Option<i64>,
}

fn cookie_theme(headers: &axum::http::HeaderMap) -> String {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix("cook_theme="))
        .find(|t| crate::COOK_THEMES.contains(t))
        .unwrap_or("normal")
        .to_string()
}

async fn cook_page(
    State(db): State<Db>,
    Path(id): Path<i64>,
    Query(q): Query<CookQuery>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Result<CookPage> {
    let r = db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?;
    let scale = q.scale.unwrap_or(1.0);
    if !(scale.is_finite() && scale > 0.0) {
        return Err(Error::Validation("scale must be a positive number".into()));
    }
    let (ingredients, back) = match q.batch {
        Some(bid) => {
            let b = db.get_batch(bid).await?;
            if b.batch.recipe_id != id {
                return Err(Error::NotFound("batch"));
            }
            let lines = b
                .lines
                .iter()
                .map(|l| {
                    let sub = if l.substitution.is_empty() { String::new() } else { format!(" (instead: {})", l.substitution) };
                    let line = [amount(l.qty_min, l.qty_max, l.unit_name.as_deref()), l.ingredient.clone()];
                    format!("{}{sub}", line.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" "))
                })
                .collect();
            (lines, format!("/batches/{bid}"))
        }
        None => (r.content.ingredients.iter().map(|i| ingredient_line(i, scale)).collect(), format!("/recipes/{id}")),
    };
    Ok(CookPage {
        name: r.content.name,
        ingredients,
        steps: r.content.steps,
        // step 8: read from the signed-in user's cook_theme
        theme: cookie_theme(&headers),
        back,
        here: uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default(),
        themes: crate::COOK_THEMES,
    })
}

#[derive(Deserialize)]
struct ThemeForm {
    theme: String,
    back: String,
}

async fn set_theme(axum::Form(f): axum::Form<ThemeForm>) -> Result<impl IntoResponse> {
    if !crate::COOK_THEMES.contains(&f.theme.as_str()) {
        return Err(Error::Validation(format!("cook theme must be one of {}", crate::COOK_THEMES.join(", "))));
    }
    // Same-site paths only, so this cannot bounce a browser elsewhere.
    let back = if f.back.starts_with('/') && !f.back.starts_with("//") && !f.back.contains('\\') { f.back } else { "/".into() };
    let cookie = format!("cook_theme={}; Path=/; Max-Age=31536000; SameSite=Lax", f.theme);
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to(&back)))
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
    lists: Vec<crate::ShoppingListSummary>,
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
        lists: db.shopping_lists().await?,
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

#[derive(Template, WebTemplate)]
#[template(path = "recipe_form.html")]
struct RecipeForm {
    heading: &'static str,
    action: String,
    name: String,
    description: String,
    ingredients: String,
}

async fn new_recipe_page() -> RecipeForm {
    RecipeForm { heading: "New recipe", action: "/recipes".into(), name: String::new(), description: String::new(), ingredients: String::new() }
}

async fn edit_recipe_page(State(db): State<Db>, Path(id): Path<i64>) -> Result<RecipeForm> {
    let r = db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?;
    let lines: Vec<String> = r
        .content
        .ingredients
        .iter()
        .map(|i| if i.original_text.is_empty() { ingredient_line(i, 1.0) } else { i.original_text.clone() })
        .collect();
    Ok(RecipeForm {
        heading: "Edit recipe",
        action: format!("/recipes/{id}/edit"),
        name: r.content.name,
        description: r.content.description,
        ingredients: lines.join("\n"),
    })
}

#[derive(Deserialize)]
struct RecipeFormData {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    ingredients: String,
}

/// Parsed form of a line: what `parse_line` found, or just the text when it found nothing.
pub(crate) fn parsed_input(text: &str, units: &[crate::Unit], foods: &[crate::Food]) -> IngredientInput {
    // step 9: LLM fallback for unparsed lines
    let p = parse_line(text, units, foods);
    let keep = p.confidence != crate::Confidence::None;
    IngredientInput {
        qty_min: p.qty_min.filter(|_| keep),
        qty_max: p.qty_max.filter(|_| keep),
        unit_id: p.unit_id,
        food_id: p.food_id,
        note: p.note,
        original_text: p.original_text,
    }
}

#[derive(Deserialize)]
struct ImportForm {
    url: String,
}

async fn import_form(State(s): State<AppState>, axum::Extension(svc): axum::Extension<std::sync::Arc<crate::ImportService>>, axum::Form(f): axum::Form<ImportForm>) -> Result<Redirect> {
    let r = svc.import(&s.db, &s.images_dir, &f.url).await?;
    Ok(Redirect::to(&format!("/recipes/{}/ingredients/review", r.id)))
}

async fn create_recipe(State(db): State<Db>, axum::Form(f): axum::Form<RecipeFormData>) -> Result<Redirect> {
    let (units, foods) = (db.units().await?, db.foods().await?);
    let ingredients = f.ingredients.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| parsed_input(l, &units, &foods)).collect();
    let r = db.create_recipe(RecipeInput { name: f.name, description: f.description, ingredients, ..Default::default() }).await?;
    Ok(Redirect::to(&format!("/recipes/{}/ingredients/review", r.id)))
}

fn to_input(i: &crate::Ingredient) -> IngredientInput {
    IngredientInput {
        qty_min: i.qty_min,
        qty_max: i.qty_max,
        unit_id: i.unit_id,
        food_id: i.food_id,
        note: i.note.clone(),
        original_text: i.original_text.clone(),
    }
}

async fn edit_recipe(State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<RecipeFormData>) -> Result<Redirect> {
    let old = db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?.content;
    let (units, foods) = (db.units().await?, db.foods().await?);
    // A line left as it was keeps what the user already reviewed; only new or changed text is parsed.
    let ingredients = f
        .ingredients
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match old.ingredients.iter().find(|i| i.original_text == l) {
            Some(i) => to_input(i),
            None => parsed_input(l, &units, &foods),
        })
        .collect();
    db.update_recipe(
        id,
        RecipeInput {
            name: f.name,
            description: f.description,
            source_url: old.source_url,
            nutrition: old.nutrition,
            tags: old.tags,
            categories: old.categories,
            cookbooks: old.cookbooks,
            tools: old.tools,
            ingredients,
            steps: old.steps,
        },
    )
    .await?;
    Ok(Redirect::to(&format!("/recipes/{id}/ingredients/review")))
}

struct ReviewRow {
    original: String,
    confidence: &'static str,
    qty_min: String,
    qty_max: String,
    unit_id: i64,
    food_id: i64,
    note: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "review.html")]
struct ReviewPage {
    id: i64,
    name: String,
    rows: Vec<ReviewRow>,
    units: Vec<crate::Unit>,
    foods: Vec<crate::Food>,
}

async fn review_page(State(db): State<Db>, Path(id): Path<i64>) -> Result<ReviewPage> {
    let r = db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?;
    let rows = r
        .content
        .ingredients
        .iter()
        .map(|i| ReviewRow {
            original: i.original_text.clone(),
            confidence: match (i.food_id, i.qty_min) {
                (Some(_), Some(_)) => "parsed",
                (None, None) if i.unit_id.is_none() => "not parsed",
                _ => "partly parsed",
            },
            qty_min: opt(i.qty_min),
            qty_max: opt(i.qty_max),
            unit_id: i.unit_id.unwrap_or(0),
            food_id: i.food_id.unwrap_or(0),
            note: i.note.clone(),
        })
        .collect();
    Ok(ReviewPage { id, name: r.content.name, rows, units: db.units().await?, foods: db.foods().await? })
}

#[derive(Default)]
struct ReviewLine {
    input: IngredientInput,
    new_unit: String,
    new_unit_dimension: String,
    new_unit_factor: Option<f64>,
    new_food: String,
}

/// Flat pairs, one run of fields per row, each run starting at `original_text`.
fn parse_review(body: &[u8]) -> Result<Vec<ReviewLine>> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body).map_err(|e| Error::Validation(e.to_string()))?;
    let mut rows: Vec<ReviewLine> = Vec::new();
    for (k, v) in pairs {
        if k == "original_text" {
            rows.push(ReviewLine { input: IngredientInput { original_text: v, ..Default::default() }, ..Default::default() });
            continue;
        }
        let Some(row) = rows.last_mut() else { continue };
        match k.as_str() {
            "qty_min" => row.input.qty_min = number(&v)?,
            "qty_max" => row.input.qty_max = number(&v)?,
            "unit_id" => row.input.unit_id = id_of(&v)?,
            "food_id" => row.input.food_id = id_of(&v)?,
            "note" => row.input.note = v,
            "new_unit" => row.new_unit = v.trim().to_string(),
            "new_unit_dimension" => row.new_unit_dimension = v,
            "new_unit_factor" => row.new_unit_factor = number(&v)?,
            "new_food" => row.new_food = v.trim().to_string(),
            _ => {}
        }
    }
    Ok(rows)
}

async fn review_save(State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    let old = db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?.content;
    let mut rows = parse_review(&body)?;
    // The same new name typed on several rows is created once; an existing name is reused.
    let mut units = db.units().await?;
    let mut foods = db.foods().await?;
    for row in &mut rows {
        if !row.new_unit.is_empty() {
            let wanted = row.new_unit.to_lowercase();
            let known = units.iter().find(|u| std::iter::once(&u.name).chain(&u.aliases).any(|n| n.to_lowercase() == wanted));
            row.input.unit_id = Some(match known {
                Some(u) => u.id,
                None => {
                    let dim: Dimension = serde_json::from_value(row.new_unit_dimension.clone().into())
                        .map_err(|_| Error::Validation(format!("'{}' is not a unit dimension", row.new_unit_dimension)))?;
                    let factor = row.new_unit_factor.filter(|_| matches!(dim, Dimension::Mass | Dimension::Volume));
                    let new_id = db.add_unit(&row.new_unit, dim, factor).await?;
                    units.push(crate::Unit { id: new_id, name: row.new_unit.clone(), aliases: vec![], dimension: dim, factor });
                    new_id
                }
            });
        }
        if !row.new_food.is_empty() {
            let wanted = row.new_food.to_lowercase();
            let known = foods.iter().find(|f| std::iter::once(&f.name).chain(&f.aliases).any(|n| n.to_lowercase() == wanted));
            row.input.food_id = Some(match known {
                Some(f) => f.id,
                None => {
                    let new_id = db.add_food(&row.new_food).await?;
                    foods.push(crate::Food { id: new_id, name: row.new_food.clone(), aliases: vec![] });
                    new_id
                }
            });
        }
    }
    // ponytail: units/foods created above stay if update_recipe then rejects the rows; harmless extras, one tx if it matters.
    db.update_recipe(
        id,
        RecipeInput {
            name: old.name,
            description: old.description,
            source_url: old.source_url,
            nutrition: old.nutrition,
            tags: old.tags,
            categories: old.categories,
            cookbooks: old.cookbooks,
            tools: old.tools,
            ingredients: rows.into_iter().map(|r| r.input).collect(),
            steps: old.steps,
        },
    )
    .await?;
    Ok(Redirect::to(&format!("/recipes/{id}")))
}

// Meal plan and shopping lists.

impl Db {
    /// `date(base, '+n days')`; `base` may be "now". Errors when `base` is not a date.
    async fn day(&self, base: &str, n: i64) -> Result<String> {
        let d: Option<String> = sqlx::query_scalar("SELECT date(?, ? || ' days')").bind(base).bind(n).fetch_one(&self.pool).await?;
        d.ok_or_else(|| Error::Validation("date must be YYYY-MM-DD".into()))
    }

    /// The Monday of the week holding `d`.
    async fn monday(&self, d: &str) -> Result<String> {
        let m: Option<String> = sqlx::query_scalar("SELECT date(?, '-6 days', 'weekday 1')").bind(d).fetch_one(&self.pool).await?;
        m.ok_or_else(|| Error::Validation("date must be YYYY-MM-DD".into()))
    }
}

struct EntryRow {
    id: i64,
    slot: String,
    label: String,
    recipe_id: i64,
    batch_id: i64,
    text: String,
}

struct DayRow {
    date: String,
    name: String,
    entries: Vec<EntryRow>,
}

#[derive(Template, WebTemplate)]
#[template(path = "plan.html")]
struct PlanPage {
    week: String,
    prev: String,
    next: String,
    days: Vec<DayRow>,
    recipes: Vec<crate::RecipeSummary>,
    slots: [&'static str; 7],
}

#[derive(Deserialize)]
struct WeekQuery {
    week: Option<String>,
}

async fn plan_page(State(db): State<Db>, Query(q): Query<WeekQuery>) -> Result<PlanPage> {
    let week = db.monday(&q.week.unwrap_or_else(|| "now".into())).await?;
    let end = db.day(&week, 6).await?;
    let recipes = db.list_recipes(true, None).await?;
    let entries = db.meal_plan(&week, &end).await?;
    let mut days = Vec::new();
    for n in 0..7 {
        let date = db.day(&week, n).await?;
        let name: String = sqlx::query_scalar("SELECT strftime('%a %d %b', ?)").bind(&date).fetch_one(&db.pool).await?;
        let entries = entries
            .iter()
            .filter(|e| e.date == date)
            .map(|e| EntryRow {
                id: e.id,
                slot: e.slot.clone(),
                label: match e.recipe_id.and_then(|r| recipes.iter().find(|x| x.id == r)) {
                    Some(r) => r.name.clone(),
                    None => e.title.clone(),
                },
                recipe_id: e.recipe_id.unwrap_or(0),
                batch_id: e.batch_id.unwrap_or(0),
                text: e.text.clone(),
            })
            .collect();
        days.push(DayRow { date, name, entries });
    }
    Ok(PlanPage { prev: db.day(&week, -7).await?, next: db.day(&week, 7).await?, week, days, recipes: recipes.into_iter().filter(|r| !r.archived).collect(), slots: crate::SLOTS })
}

#[derive(Deserialize)]
struct PlanForm {
    date: String,
    slot: String,
    recipe_id: Option<String>,
    #[serde(default)]
    title: String,
    /// Set: start a batch at this scale and attach it.
    #[serde(default)]
    scale: String,
}

async fn plan_add(State(db): State<Db>, axum::Form(f): axum::Form<PlanForm>) -> Result<Redirect> {
    let recipe_id = id_of(f.recipe_id.as_deref().unwrap_or_default())?;
    let batch_id = match (recipe_id, number(&f.scale)?) {
        (Some(r), Some(scale)) => Some(db.start_batch(r, scale).await?.batch.id),
        _ => None,
    };
    db.add_entry(crate::EntryInput { date: f.date.clone(), slot: f.slot, recipe_id, title: f.title, batch_id, ..Default::default() }).await?;
    Ok(Redirect::to(&format!("/plan?week={}", db.monday(&f.date).await?)))
}

#[derive(Deserialize)]
struct WeekForm {
    week: String,
}

async fn plan_delete(State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<WeekForm>) -> Result<Redirect> {
    db.delete_entry(id).await?;
    Ok(Redirect::to(&format!("/plan?week={}", db.monday(&f.week).await?)))
}

#[derive(Template, WebTemplate)]
#[template(path = "lists.html")]
struct ListsPage {
    lists: Vec<crate::ShoppingListSummary>,
}

async fn lists_page(State(db): State<Db>) -> Result<ListsPage> {
    Ok(ListsPage { lists: db.shopping_lists().await? })
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn lists_create(State(db): State<Db>, axum::Form(f): axum::Form<NameForm>) -> Result<Redirect> {
    Ok(Redirect::to(&format!("/lists/{}", db.create_list(&f.name).await?.id)))
}

struct ItemRow {
    id: i64,
    checked: bool,
    text: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "list.html")]
struct ListPage {
    id: i64,
    name: String,
    items: Vec<ItemRow>,
    units: Vec<crate::Unit>,
}

async fn list_page(State(db): State<Db>, Path(id): Path<i64>) -> Result<ListPage> {
    let list = db.shopping_list(id).await?;
    let (units, foods) = (db.units().await?, db.foods().await?);
    let items = list
        .items
        .iter()
        .map(|i| {
            let unit = i.unit_id.and_then(|u| units.iter().find(|x| x.id == u)).map(|u| u.name.as_str());
            let food = i.food_id.and_then(|f| foods.iter().find(|x| x.id == f)).map(|f| f.name.as_str());
            let text = [amount(i.qty, i.qty_max, unit), food.unwrap_or_default().to_string(), i.note.clone()];
            ItemRow { id: i.id, checked: i.checked, text: text.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ") }
        })
        .collect();
    Ok(ListPage { id, name: list.name, items, units })
}

#[derive(Deserialize)]
struct ItemForm {
    #[serde(default)]
    qty: String,
    #[serde(default)]
    unit_id: String,
    /// A food's name or alias; anything else is kept as a note.
    #[serde(default)]
    item: String,
}

async fn list_add_item(State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<ItemForm>) -> Result<Redirect> {
    let wanted = f.item.trim().to_lowercase();
    let food = db.foods().await?.into_iter().find(|x| std::iter::once(&x.name).chain(&x.aliases).any(|n| n.to_lowercase() == wanted));
    let input = crate::ItemInput {
        qty: number(&f.qty)?,
        unit_id: id_of(&f.unit_id)?,
        note: if food.is_some() { String::new() } else { f.item.trim().to_string() },
        food_id: food.map(|x| x.id),
        ..Default::default()
    };
    db.add_item(id, input).await?;
    Ok(Redirect::to(&format!("/lists/{id}")))
}

#[derive(Deserialize)]
struct AddToList {
    /// Existing list; ignored when `new_list` is filled in.
    #[serde(default)]
    list_id: String,
    #[serde(default)]
    new_list: String,
    recipe_id: Option<i64>,
    batch_id: Option<i64>,
    #[serde(default)]
    scale: String,
}

async fn target_list(db: &Db, f: &AddToList) -> Result<i64> {
    if !f.new_list.trim().is_empty() {
        return Ok(db.create_list(&f.new_list).await?.id);
    }
    id_of(&f.list_id)?.ok_or_else(|| Error::Validation("choose a list or name a new one".into()))
}

async fn list_add_recipe(State(db): State<Db>, axum::Form(f): axum::Form<AddToList>) -> Result<Redirect> {
    let list = target_list(&db, &f).await?;
    let recipe = f.recipe_id.ok_or(Error::Validation("recipe_id is required".into()))?;
    db.add_recipe_to_list(list, recipe, number(&f.scale)?.unwrap_or(1.0)).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

async fn list_add_batch(State(db): State<Db>, axum::Form(f): axum::Form<AddToList>) -> Result<Redirect> {
    let list = target_list(&db, &f).await?;
    db.add_batch_to_list(list, f.batch_id.ok_or(Error::Validation("batch_id is required".into()))?).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

#[derive(Deserialize)]
struct CheckForm {
    /// A checkbox: present when ticked.
    checked: Option<String>,
}

/// htmx gets 204 (the box already shows the state); a plain form post gets a redirect.
async fn item_check(
    State(db): State<Db>,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    axum::Form(f): axum::Form<CheckForm>,
) -> Result<axum::response::Response> {
    db.set_item_checked(id, f.checked.is_some()).await?;
    if headers.contains_key("hx-request") {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(back_to_list(&db, id).await.into_response())
}

async fn item_delete(State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let list = list_of(&db, id).await?;
    db.delete_item(id).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

async fn list_of(db: &Db, item: i64) -> Result<i64> {
    sqlx::query_scalar("SELECT list_id FROM shopping_list_items WHERE id = ?").bind(item).fetch_optional(&db.pool).await?.ok_or(Error::NotFound("shopping list item"))
}

async fn back_to_list(db: &Db, item: i64) -> Redirect {
    Redirect::to(&format!("/lists/{}", list_of(db, item).await.unwrap_or(0)))
}

#[derive(Template, WebTemplate)]
#[template(path = "webhooks.html")]
struct WebhooksPage {
    hooks: Vec<crate::Webhook>,
    msg: String,
}

#[derive(Deserialize)]
struct MsgQuery {
    msg: Option<String>,
}

async fn webhooks_page(State(db): State<Db>, Query(q): Query<MsgQuery>) -> Result<WebhooksPage> {
    Ok(WebhooksPage { hooks: db.list_webhooks().await?, msg: q.msg.unwrap_or_default() })
}

#[derive(Deserialize)]
struct WebhookForm {
    name: String,
    kind: String,
    url: String,
    #[serde(default)]
    ntfy_tags: String,
    ev_import_failed: Option<String>,
    ev_batch_logged: Option<String>,
}

async fn webhook_create(State(db): State<Db>, axum::Form(f): axum::Form<WebhookForm>) -> Result<Redirect> {
    let events = [("import_failed", f.ev_import_failed), ("batch_logged", f.ev_batch_logged)].into_iter().filter(|(_, on)| on.is_some()).map(|(e, _)| e.to_string()).collect();
    db.create_webhook(crate::WebhookInput { name: f.name, kind: f.kind, url: f.url, events, ntfy_tags: f.ntfy_tags, enabled: true }).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_delete(State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_webhook(id).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_toggle(State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let w = db.get_webhook(id).await?;
    db.update_webhook(id, crate::WebhookInput { name: w.name, kind: w.kind, url: w.url, events: w.events, ntfy_tags: w.ntfy_tags, enabled: !w.enabled }).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_test(State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let msg = match db.send_test(id).await {
        Ok(s) => format!("Test sent; the receiver answered {s}."),
        Err(Error::NotFound(n)) => return Err(Error::NotFound(n)),
        Err(e) => e.to_string(),
    };
    Ok(Redirect::to(&format!("/settings/webhooks?{}", serde_urlencoded::to_string([("msg", msg)]).unwrap_or_default())))
}
