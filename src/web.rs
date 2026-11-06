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
use crate::{Scope, BatchInput, BatchLineInput, Db, Dimension, Error, IngredientInput, RecipeInput, Result, parse_line};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/recipes", post(create_recipe))
        .route("/recipes/import", post(import_form))
        .route("/recipes/new", get(new_recipe_page))
        .route("/recipes/{id}", get(recipe_page))
        .route("/recipes/{id}/edit", get(edit_recipe_page).post(edit_recipe))
        .route("/recipes/{id}/nutrition/estimate", post(nutrition_fragment))
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
        // Admin only: each handler takes `Admin`, and the guard middleware refuses non-admins under /settings/webhooks.
        .route("/settings/webhooks", get(webhooks_page).post(webhook_create))
        .route("/settings/webhooks/{id}/delete", post(webhook_delete))
        .route("/settings/webhooks/{id}/toggle", post(webhook_toggle))
        .route("/settings/webhooks/{id}/test", post(webhook_test))
        // Admin only, like the webhooks above.
        .route("/settings/llm", get(llm_page))
        .route("/settings/llm/test", post(llm_test))
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

async fn index(sc: Scope, State(db): State<Db>, Query(s): Query<Search>) -> Result<Index> {
    let q = s.q.unwrap_or_default();
    let recipes = db.list_recipes(&sc, false, Some(q.trim()).filter(|q| !q.is_empty())).await?;
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

async fn recipe_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<RecipePage> {
    let r = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?;
    let ingredients = r.content.ingredients.iter().map(|i| ingredient_line(i, 1.0)).collect();
    let batches = db
        .list_batches(&sc, id)
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
    Ok(RecipePage { id, lists: db.shopping_lists(&sc).await?, slots: crate::SLOTS, today: db.day("now", 0).await?, name: r.content.name, description: r.content.description, ingredients, steps: r.content.steps, batches })
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

async fn cook_page(
    crate::CurrentUser { user, .. }: crate::CurrentUser,
    sc: Scope,
    State(db): State<Db>,
    Path(id): Path<i64>,
    Query(q): Query<CookQuery>,
    uri: axum::http::Uri,
) -> Result<CookPage> {
    let r = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?;
    let scale = q.scale.unwrap_or(1.0);
    if !(scale.is_finite() && scale > 0.0) {
        return Err(Error::Validation("scale must be a positive number".into()));
    }
    let (ingredients, back) = match q.batch {
        Some(bid) => {
            let b = db.get_batch(&sc, bid).await?;
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
        theme: user.cook_theme,
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

async fn set_theme(crate::CurrentUser { user, .. }: crate::CurrentUser, State(db): State<Db>, axum::Form(f): axum::Form<ThemeForm>) -> Result<Redirect> {
    db.set_cook_theme(user.id, &f.theme).await?;
    // Same-site paths only, so this cannot bounce a browser elsewhere.
    let back = if f.back.starts_with('/') && !f.back.starts_with("//") && !f.back.contains('\\') { f.back } else { "/".into() };
    Ok(Redirect::to(&back))
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

/// Serves `<64 hex>.<png|jpg|webp|gif>` from the images dir to the tenant that uses the image; anything else is 404.
async fn image(sc: Scope, State(s): State<AppState>, Path(name): Path<String>) -> impl IntoResponse {
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
    // The file name is the hash; whose it is, is in the database.
    if !matches!(s.db.image_visible(&sc, name.split('.').next().unwrap_or_default()).await, Ok(true)) {
        return StatusCode::NOT_FOUND.into_response();
    }
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
    /// The nutrition fieldset, already rendered.
    nutrition: String,
}

/// The nutrition inputs; also what the estimate button swaps in. `id` is 0 on the new-recipe form.
#[derive(Template)]
#[template(path = "nutrition.html")]
struct NutritionFields {
    id: i64,
    can_estimate: bool,
    calories: String,
    protein_g: String,
    fat_g: String,
    carbs_g: String,
}

impl NutritionFields {
    fn new(id: i64, can_estimate: bool, n: Option<&crate::Nutrition>) -> Self {
        let n = n.cloned().unwrap_or_default();
        NutritionFields { id, can_estimate, calories: opt(n.calories), protein_g: opt(n.protein_g), fat_g: opt(n.fat_g), carbs_g: opt(n.carbs_g) }
    }

    fn html(&self) -> Result<String> {
        self.render().map_err(|e| Error::Io(std::io::Error::other(e)))
    }
}

async fn new_recipe_page() -> Result<RecipeForm> {
    Ok(RecipeForm {
        heading: "New recipe",
        action: "/recipes".into(),
        name: String::new(),
        description: String::new(),
        ingredients: String::new(),
        nutrition: NutritionFields::new(0, false, None).html()?,
    })
}

type Svc = axum::Extension<std::sync::Arc<crate::ImportService>>;

/// The LLM's nutrition guess for a recipe. Recipe first, so an unknown id costs no request.
pub(crate) async fn estimate_recipe(db: &Db, sc: &Scope, svc: &crate::ImportService, id: i64) -> Result<crate::NutritionEstimate> {
    let r = db.get_recipe(sc, id).await?.ok_or(Error::NotFound("recipe"))?;
    let llm = svc.llm().ok_or_else(|| Error::Validation("no LLM is configured (set LLM_PROVIDER)".into()))?;
    let lines: Vec<String> = r.content.ingredients.iter().map(|i| if i.original_text.is_empty() { ingredient_line(i, 1.0) } else { i.original_text.clone() }).filter(|l| !l.is_empty()).collect();
    llm.estimate_nutrition(&r.content.name, &lines).await
}

/// htmx target: the nutrition fieldset filled with a guess. Saves nothing.
async fn nutrition_fragment(sc: Scope, State(db): State<Db>, axum::Extension(svc): Svc, Path(id): Path<i64>) -> Result<axum::response::Html<String>> {
    let e = estimate_recipe(&db, &sc, &svc, id).await?;
    let n = crate::Nutrition { calories: e.calories, protein_g: e.protein, fat_g: e.fat, carbs_g: e.carbs };
    Ok(axum::response::Html(NutritionFields::new(id, true, Some(&n)).html()?))
}

async fn edit_recipe_page(sc: Scope, State(db): State<Db>, axum::Extension(svc): Svc, Path(id): Path<i64>) -> Result<RecipeForm> {
    let r = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?;
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
        nutrition: NutritionFields::new(id, svc.llm().is_some(), r.content.nutrition.as_ref()).html()?,
    })
}

#[derive(Deserialize)]
struct RecipeFormData {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    ingredients: String,
    // Absent means "leave nutrition as it is"; present (even blank) replaces it.
    calories: Option<String>,
    protein_g: Option<String>,
    fat_g: Option<String>,
    carbs_g: Option<String>,
}

impl RecipeFormData {
    /// `None` when the form carried no nutrition fields at all.
    fn nutrition(&self) -> Result<Option<Option<crate::Nutrition>>> {
        let f = [&self.calories, &self.protein_g, &self.fat_g, &self.carbs_g];
        if f.iter().all(|v| v.is_none()) {
            return Ok(None);
        }
        let n = |v: &Option<String>| number(v.as_deref().unwrap_or_default());
        let n = crate::Nutrition { calories: n(&self.calories)?, protein_g: n(&self.protein_g)?, fat_g: n(&self.fat_g)?, carbs_g: n(&self.carbs_g)? };
        Ok(Some((n != crate::Nutrition::default()).then_some(n)))
    }
}

/// Where the review screen is; `suggested` lists the lines the LLM filled so it can mark them.
fn review_url(id: i64, suggested: &[usize]) -> String {
    let base = format!("/recipes/{id}/ingredients/review");
    if suggested.is_empty() {
        return base;
    }
    format!("{base}?suggested={}", suggested.iter().map(usize::to_string).collect::<Vec<_>>().join(","))
}

/// Parsed form of a line: what `parse_line` found, or just the text when it found nothing.
pub(crate) fn parsed_input(text: &str, units: &[crate::Unit], foods: &[crate::Food]) -> IngredientInput {
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

async fn import_form(sc: Scope, State(s): State<AppState>, axum::Extension(svc): Svc, axum::Form(f): axum::Form<ImportForm>) -> Result<Redirect> {
    let (r, suggested) = svc.import_suggested(&s.db, &sc, &s.images_dir, &f.url).await?;
    Ok(Redirect::to(&review_url(r.id, &suggested)))
}

async fn create_recipe(sc: Scope, State(db): State<Db>, axum::Extension(svc): Svc, axum::Form(f): axum::Form<RecipeFormData>) -> Result<Redirect> {
    let (units, foods) = (db.units(&sc).await?, db.foods(&sc).await?);
    let nutrition = f.nutrition()?.flatten();
    let mut ingredients: Vec<IngredientInput> = f.ingredients.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| parsed_input(l, &units, &foods)).collect();
    let all = vec![true; ingredients.len()];
    let suggested = svc.fill_lines(&mut ingredients, &all, &units, &foods).await;
    let r = db.create_recipe(&sc, RecipeInput { name: f.name, description: f.description, nutrition, ingredients, ..Default::default() }).await?;
    Ok(Redirect::to(&review_url(r.id, &suggested)))
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

async fn edit_recipe(sc: Scope, State(db): State<Db>, axum::Extension(svc): Svc, Path(id): Path<i64>, axum::Form(f): axum::Form<RecipeFormData>) -> Result<Redirect> {
    let old = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?.content;
    let (units, foods) = (db.units(&sc).await?, db.foods(&sc).await?);
    let nutrition = f.nutrition()?.unwrap_or(old.nutrition);
    // A line left as it was keeps what the user already reviewed (and is not sent to the LLM again); only new or changed text is parsed.
    let (mut ingredients, mut is_new) = (Vec::new(), Vec::new());
    for l in f.ingredients.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let known = old.ingredients.iter().find(|i| i.original_text == l);
        is_new.push(known.is_none());
        ingredients.push(known.map_or_else(|| parsed_input(l, &units, &foods), to_input));
    }
    let suggested = svc.fill_lines(&mut ingredients, &is_new, &units, &foods).await;
    db.update_recipe(
        &sc,
        id,
        RecipeInput {
            name: f.name,
            description: f.description,
            source_url: old.source_url,
            nutrition,
            tags: old.tags,
            categories: old.categories,
            cookbooks: old.cookbooks,
            tools: old.tools,
            ingredients,
            steps: old.steps,
        },
    )
    .await?;
    Ok(Redirect::to(&review_url(id, &suggested)))
}

struct ReviewRow {
    original: String,
    confidence: &'static str,
    suggested: bool,
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

#[derive(Deserialize)]
struct ReviewQuery {
    /// "1,3": line numbers the LLM filled.
    suggested: Option<String>,
}

async fn review_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Query(q): Query<ReviewQuery>) -> Result<ReviewPage> {
    let r = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?;
    let suggested: Vec<usize> = q.suggested.unwrap_or_default().split(',').filter_map(|n| n.trim().parse().ok()).collect();
    let rows = r
        .content
        .ingredients
        .iter()
        .enumerate()
        .map(|(n, i)| ReviewRow {
            original: i.original_text.clone(),
            suggested: suggested.contains(&n),
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
    Ok(ReviewPage { id, name: r.content.name, rows, units: db.units(&sc).await?, foods: db.foods(&sc).await? })
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

async fn review_save(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    let old = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?.content;
    let mut rows = parse_review(&body)?;
    // The same new name typed on several rows is created once; an existing name is reused.
    let mut units = db.units(&sc).await?;
    let mut foods = db.foods(&sc).await?;
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
                    let new_id = db.add_unit(&sc, &row.new_unit, dim, factor).await?;
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
                    let new_id = db.add_food(&sc, &row.new_food).await?;
                    foods.push(crate::Food { id: new_id, name: row.new_food.clone(), aliases: vec![] });
                    new_id
                }
            });
        }
    }
    // ponytail: units/foods created above stay if update_recipe then rejects the rows; harmless extras, one tx if it matters.
    db.update_recipe(
        &sc,
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

async fn plan_page(sc: Scope, State(db): State<Db>, Query(q): Query<WeekQuery>) -> Result<PlanPage> {
    let week = db.monday(&q.week.unwrap_or_else(|| "now".into())).await?;
    let end = db.day(&week, 6).await?;
    let recipes = db.list_recipes(&sc, true, None).await?;
    let entries = db.meal_plan(&sc, &week, &end).await?;
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

async fn plan_add(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<PlanForm>) -> Result<Redirect> {
    let recipe_id = id_of(f.recipe_id.as_deref().unwrap_or_default())?;
    let batch_id = match (recipe_id, number(&f.scale)?) {
        (Some(r), Some(scale)) => Some(db.start_batch(&sc, r, scale).await?.batch.id),
        _ => None,
    };
    db.add_entry(&sc, crate::EntryInput { date: f.date.clone(), slot: f.slot, recipe_id, title: f.title, batch_id, ..Default::default() }).await?;
    Ok(Redirect::to(&format!("/plan?week={}", db.monday(&f.date).await?)))
}

#[derive(Deserialize)]
struct WeekForm {
    week: String,
}

async fn plan_delete(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<WeekForm>) -> Result<Redirect> {
    db.delete_entry(&sc, id).await?;
    Ok(Redirect::to(&format!("/plan?week={}", db.monday(&f.week).await?)))
}

#[derive(Template, WebTemplate)]
#[template(path = "lists.html")]
struct ListsPage {
    lists: Vec<crate::ShoppingListSummary>,
}

async fn lists_page(sc: Scope, State(db): State<Db>) -> Result<ListsPage> {
    Ok(ListsPage { lists: db.shopping_lists(&sc).await? })
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn lists_create(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<NameForm>) -> Result<Redirect> {
    Ok(Redirect::to(&format!("/lists/{}", db.create_list(&sc, &f.name).await?.id)))
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

async fn list_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<ListPage> {
    let list = db.shopping_list(&sc, id).await?;
    let (units, foods) = (db.units(&sc).await?, db.foods(&sc).await?);
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

async fn list_add_item(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<ItemForm>) -> Result<Redirect> {
    let wanted = f.item.trim().to_lowercase();
    let food = db.foods(&sc).await?.into_iter().find(|x| std::iter::once(&x.name).chain(&x.aliases).any(|n| n.to_lowercase() == wanted));
    let input = crate::ItemInput {
        qty: number(&f.qty)?,
        unit_id: id_of(&f.unit_id)?,
        note: if food.is_some() { String::new() } else { f.item.trim().to_string() },
        food_id: food.map(|x| x.id),
        ..Default::default()
    };
    db.add_item(&sc, id, input).await?;
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

async fn target_list(db: &Db, sc: &Scope, f: &AddToList) -> Result<i64> {
    if !f.new_list.trim().is_empty() {
        return Ok(db.create_list(sc, &f.new_list).await?.id);
    }
    id_of(&f.list_id)?.ok_or_else(|| Error::Validation("choose a list or name a new one".into()))
}

async fn list_add_recipe(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<AddToList>) -> Result<Redirect> {
    let list = target_list(&db, &sc, &f).await?;
    let recipe = f.recipe_id.ok_or(Error::Validation("recipe_id is required".into()))?;
    db.add_recipe_to_list(&sc, list, recipe, number(&f.scale)?.unwrap_or(1.0)).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

async fn list_add_batch(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<AddToList>) -> Result<Redirect> {
    let list = target_list(&db, &sc, &f).await?;
    db.add_batch_to_list(&sc, list, f.batch_id.ok_or(Error::Validation("batch_id is required".into()))?).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

#[derive(Deserialize)]
struct CheckForm {
    /// A checkbox: present when ticked.
    checked: Option<String>,
}

/// htmx gets 204 (the box already shows the state); a plain form post gets a redirect.
async fn item_check(
    sc: Scope,
    State(db): State<Db>,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    axum::Form(f): axum::Form<CheckForm>,
) -> Result<axum::response::Response> {
    db.set_item_checked(&sc, id, f.checked.is_some()).await?;
    if headers.contains_key("hx-request") {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(back_to_list(&db, &sc, id).await.into_response())
}

async fn item_delete(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let list = db.item_list_id(&sc, id).await?;
    db.delete_item(&sc, id).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

async fn back_to_list(db: &Db, sc: &Scope, item: i64) -> Redirect {
    Redirect::to(&format!("/lists/{}", db.item_list_id(sc, item).await.unwrap_or(0)))
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

async fn webhooks_page(_: crate::Admin, State(db): State<Db>, Query(q): Query<MsgQuery>) -> Result<WebhooksPage> {
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

async fn webhook_create(_: crate::Admin, State(db): State<Db>, axum::Form(f): axum::Form<WebhookForm>) -> Result<Redirect> {
    let events = [("import_failed", f.ev_import_failed), ("batch_logged", f.ev_batch_logged)].into_iter().filter(|(_, on)| on.is_some()).map(|(e, _)| e.to_string()).collect();
    db.create_webhook(crate::WebhookInput { name: f.name, kind: f.kind, url: f.url, events, ntfy_tags: f.ntfy_tags, enabled: true }).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_delete(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_webhook(id).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_toggle(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let w = db.get_webhook(id).await?;
    db.update_webhook(id, crate::WebhookInput { name: w.name, kind: w.kind, url: w.url, events: w.events, ntfy_tags: w.ntfy_tags, enabled: !w.enabled }).await?;
    Ok(Redirect::to("/settings/webhooks"))
}

async fn webhook_test(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let msg = match db.send_test(id).await {
        Ok(s) => format!("Test sent; the receiver answered {s}."),
        Err(Error::NotFound(n)) => return Err(Error::NotFound(n)),
        Err(e) => e.to_string(),
    };
    Ok(Redirect::to(&format!("/settings/webhooks?{}", serde_urlencoded::to_string([("msg", msg)]).unwrap_or_default())))
}

// LLM settings.

#[derive(Template, WebTemplate)]
#[template(path = "llm.html")]
struct LlmPage {
    enabled: bool,
    provider: String,
    model: String,
    msg: String,
}

fn llm_view(svc: &crate::ImportService, msg: String) -> LlmPage {
    let llm = svc.llm();
    LlmPage {
        enabled: llm.is_some(),
        provider: llm.map(|l| serde_json::to_value(l.provider()).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()).unwrap_or_default(),
        model: llm.map(|l| l.model().to_string()).unwrap_or_default(),
        msg,
    }
}

async fn llm_page(_: crate::Admin, axum::Extension(svc): Svc) -> LlmPage {
    llm_view(&svc, String::new())
}

async fn llm_test(_: crate::Admin, axum::Extension(svc): Svc) -> LlmPage {
    let msg = match svc.llm() {
        None => "No LLM is configured.".to_string(),
        Some(l) => match l.test().await {
            Ok(()) => "Test passed.".to_string(),
            Err(e) => format!("Test failed: {e}"),
        },
    };
    llm_view(&svc, msg)
}
