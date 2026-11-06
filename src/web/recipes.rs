//! Recipe pages: the list, one recipe, cooking it, the form, and reviewing parsed ingredients.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::Redirect;
use axum::routing::{get, post};
use serde::Deserialize;

use super::{Svc, amount, id_of, num, number, opt};
use crate::api::{AppState, PHOTO_BODY_LIMIT};
use crate::parse::parsed_input;
use crate::{Db, Dimension, Error, IngredientInput, LineDetail, Note, RecipeInput, Result, Scope, Settings, StepDetail, Substitution};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        // The form carries an image, so it gets the photo-sized body limit.
        .route("/recipes", post(create_recipe).layer(DefaultBodyLimit::max(PHOTO_BODY_LIMIT)))
        .route("/recipes/import", post(import_form))
        .route("/recipes/new", get(new_recipe_page))
        .route("/recipes/{id}", get(recipe_page))
        .route("/recipes/{id}/edit", get(edit_recipe_page).post(edit_recipe).layer(DefaultBodyLimit::max(PHOTO_BODY_LIMIT)))
        .route("/recipes/{id}/nutrition/estimate", post(nutrition_fragment))
        .route("/recipes/{id}/ingredients/review", get(review_page).post(review_save))
        .route("/recipes/{id}/cook", get(cook_page))
        .route("/recipes/{id}/link-ingredients", get(link_page).post(link_save))
        .route("/recipes/{id}/assets", post(add_asset).layer(DefaultBodyLimit::max(crate::MAX_ASSET_BYTES + 1024 * 1024)))
        .route("/recipes/{id}/assets/{hash}/delete", post(delete_asset))
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

/// A run of ingredient lines under one section title (the first run may have none).
struct IngGroup {
    title: String,
    lines: Vec<IngLine>,
}

struct IngLine {
    text: String,
    /// "or ..." texts: the line's own substitutions, then its food's.
    alternatives: Vec<String>,
}

struct StepView {
    title: String,
    text: String,
}

struct AssetView {
    name: String,
    file_name: String,
    hash: String,
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
    groups: Vec<IngGroup>,
    steps: Vec<StepView>,
    batches: Vec<BatchRow>,
    /// File name of the recipe's image, empty when it has none.
    image: String,
    locked: bool,
    nutrition: Vec<String>,
    notes: Vec<Note>,
    extras: Vec<(String, String)>,
    assets: Vec<AssetView>,
}

/// "1.5 tbsp butter (note)" for a substitution, as shown after "or".
fn alternative(qty: Option<f64>, unit: Option<&str>, food: Option<&str>, note: &str) -> String {
    let what = [amount(qty, qty, unit), food.unwrap_or_default().to_string()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    match (what.is_empty(), note.is_empty()) {
        (_, true) => what,
        (true, false) => note.to_string(),
        (false, false) => format!("{what} ({note})"),
    }
}

async fn recipe_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<RecipePage> {
    let r = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?;
    let c = r.content;
    let foods: Vec<i64> = c.ingredients.iter().filter_map(|i| i.food_id).collect();
    let by_food = db.substitutes_of(&sc, &foods).await?;
    let mut groups: Vec<IngGroup> = Vec::new();
    for i in &c.ingredients {
        if groups.is_empty() || !i.section.is_empty() {
            groups.push(IngGroup { title: i.section.clone(), lines: Vec::new() });
        }
        let own = i.substitutions.iter().map(|s| alternative(s.qty, s.unit.as_deref(), s.food.as_deref(), &s.note));
        let of_food = i.food_id.and_then(|f| by_food.get(&f)).into_iter().flatten().map(|s| alternative(None, None, Some(&s.name), &s.note));
        let alternatives = own.chain(of_food).filter(|a| !a.is_empty()).map(|a| format!("or {a}")).collect();
        groups.last_mut().expect("a group was just pushed").lines.push(IngLine { text: ingredient_line(i, 1.0), alternatives });
    }
    let steps = c.steps.iter().enumerate().map(|(n, t)| StepView { title: c.step_details.get(n).map(|d| d.title.clone()).unwrap_or_default(), text: t.clone() }).collect();
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
    let nutrition = match (&c.nutrition, c.settings.hide_nutrition) {
        (Some(n), false) => [("Calories", n.calories), ("Protein (g)", n.protein_g), ("Fat (g)", n.fat_g), ("Carbohydrate (g)", n.carbs_g)].into_iter().filter_map(|(k, v)| v.map(|v| format!("{k}: {}", num(v)))).collect(),
        _ => Vec::new(),
    };
    let assets = db.recipe_assets(&sc, id).await?.into_iter().map(|a| AssetView { name: a.name, file_name: a.file_name, hash: a.hash }).collect();
    Ok(RecipePage {
        id,
        lists: db.shopping_lists(&sc).await?,
        slots: crate::SLOTS,
        today: db.day("now", 0).await?,
        name: c.name,
        description: c.description,
        groups,
        steps,
        batches,
        image: db.recipe_images(&sc, id).await?.first().map(crate::Image::file_name).unwrap_or_default(),
        locked: c.settings.locked,
        nutrition,
        notes: c.notes,
        extras: c.extras.into_iter().collect(),
        assets,
    })
}

/// One ingredient as text, amounts times `scale`; falls back to the original text.
fn ingredient_line(i: &crate::Ingredient, scale: f64) -> String {
    let what = [i.food_name.as_deref().unwrap_or_default(), &i.note];
    let what = what.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(", ");
    let q = |n: Option<f64>| n.map(|n| n * scale);
    let line = [amount(q(i.qty_min), q(i.qty_max), i.unit_name.as_deref()), what].join(" ");
    if line.trim().is_empty() { i.original_text.clone() } else { line.trim().to_string() }
}

struct CookStep {
    title: String,
    text: String,
    /// The ingredient lines this step uses, as shown in the ingredients list.
    uses: Vec<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "cook.html")]
struct CookPage {
    name: String,
    /// Set when the page shows the live recipe because the batch has no snapshot (imported history).
    historical_unavailable: bool,
    /// Names this cook in the browser's storage: `recipe:<id>` or `batch:<id>`.
    key: String,
    ingredients: Vec<String>,
    steps: Vec<CookStep>,
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
    // A batch is cooked from its own snapshot, never the live recipe; imported history has none.
    // `by_line` is the text of each of the content's ingredient lines, in order, for the "uses" under a step.
    let (content, ingredients, by_line, back, historical_unavailable, key) = match q.batch {
        Some(bid) => {
            let b = db.get_batch(&sc, bid).await?;
            if b.batch.recipe_id != id {
                return Err(Error::NotFound("batch"));
            }
            let text = |l: &crate::BatchLine| {
                let sub = if l.substitution.is_empty() { String::new() } else { format!(" (instead: {})", l.substitution) };
                let line = [amount(l.qty_min, l.qty_max, l.unit_name.as_deref()), l.ingredient.clone()];
                format!("{}{sub}", line.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" "))
            };
            let lines: Vec<String> = b.lines.iter().map(text).collect();
            let (content, missing) = match b.batch.snapshot_id {
                Some(s) => (db.snapshot(&sc, s).await?.content, false),
                None => (r.content, true),
            };
            // The batch's lines that came from the snapshot are in the snapshot's order; added ones follow their own.
            let by_line = if missing {
                content.ingredients.iter().map(|i| ingredient_line(i, b.batch.scale)).collect()
            } else {
                b.lines.iter().filter(|l| !l.added).map(text).collect()
            };
            (content, lines, by_line, format!("/batches/{bid}"), missing, format!("batch:{bid}"))
        }
        None => {
            let lines: Vec<String> = r.content.ingredients.iter().map(|i| ingredient_line(i, scale)).collect();
            (r.content, lines.clone(), lines, format!("/recipes/{id}"), false, format!("recipe:{id}"))
        }
    };
    let steps = content
        .steps
        .iter()
        .enumerate()
        .map(|(n, text)| {
            let d = content.step_details.get(n);
            let uses = d.into_iter().flat_map(|d| &d.ingredient_uids).filter_map(|u| content.ingredients.iter().position(|i| &i.uid == u)).filter_map(|p| by_line.get(p).cloned()).collect();
            CookStep { title: d.map(|d| d.title.clone()).unwrap_or_default(), text: text.clone(), uses }
        })
        .collect();
    Ok(CookPage {
        name: content.name,
        historical_unavailable,
        key,
        ingredients,
        steps,
        theme: user.cook_theme,
        back,
        here: uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default(),
        themes: crate::COOK_THEMES,
    })
}

struct NoteRow {
    title: String,
    text: String,
}

struct ExtraRow {
    key: String,
    value: String,
}

struct SubRow {
    /// 1-based number of the ingredient line (section titles not counted); empty on a blank row.
    line: String,
    qty: String,
    unit: String,
    food: String,
    note: String,
}

/// The settings checkboxes: (field name, label, ticked).
type Flag = (&'static str, &'static str, bool);

#[derive(Template, WebTemplate)]
#[template(path = "recipe_form.html")]
struct RecipeForm {
    heading: &'static str,
    action: String,
    /// 0 on the new-recipe form.
    id: i64,
    name: String,
    description: String,
    ingredients: String,
    /// One per line; a `## ` line titles the step after it.
    steps: String,
    /// Comma separated.
    tags: String,
    categories: String,
    tools: String,
    cookbooks: String,
    /// File name of the recipe's current image, empty when it has none.
    image: String,
    /// The nutrition fieldset, already rendered.
    nutrition: String,
    notes: Vec<NoteRow>,
    extras: Vec<ExtraRow>,
    subs: Vec<SubRow>,
    flags: Vec<Flag>,
    assets: Vec<AssetView>,
}

fn flags(s: &Settings) -> Vec<Flag> {
    vec![
        ("locked", "Locked (edits are refused until this is unticked)", s.locked),
        ("hide_nutrition", "Hide nutrition", s.hide_nutrition),
        ("disable_comments", "Disable comments", s.disable_comments),
        ("public", "Public (kept from Mealie, not used)", s.public),
        ("show_assets", "Show assets (kept from Mealie, not used)", s.show_assets),
        ("landscape_view", "Landscape view (kept from Mealie, not used)", s.landscape_view),
        ("disable_amount", "Disable amounts (kept from Mealie, not used)", s.disable_amount),
    ]
}

/// Two empty rows after the filled ones, so there is always somewhere to type; saving shows two more.
const BLANK_ROWS: usize = 2;

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
        self.render().map_err(|e| Error::Internal(format!("template: {e}")))
    }
}

async fn new_recipe_page() -> Result<RecipeForm> {
    Ok(form_for(0, "New recipe", "/recipes".into(), &crate::Content::default(), Vec::new(), String::new(), NutritionFields::new(0, false, None).html()?))
}

/// The form for `c`. `ingredients` and `steps` are the textareas' text.
#[allow(clippy::too_many_arguments)]
fn form_for(id: i64, heading: &'static str, action: String, c: &crate::Content, assets: Vec<AssetView>, image: String, nutrition: String) -> RecipeForm {
    let mut ingredients = Vec::new();
    for i in &c.ingredients {
        if !i.section.is_empty() {
            ingredients.push(format!("## {}", i.section));
        }
        ingredients.push(if i.original_text.is_empty() { ingredient_line(i, 1.0) } else { i.original_text.clone() });
    }
    let mut steps = Vec::new();
    for (n, t) in c.steps.iter().enumerate() {
        if let Some(d) = c.step_details.get(n).filter(|d| !d.title.is_empty()) {
            steps.push(format!("## {}", d.title));
        }
        steps.push(t.clone());
    }
    let blank_note = || NoteRow { title: String::new(), text: String::new() };
    let mut notes: Vec<NoteRow> = c.notes.iter().map(|n| NoteRow { title: n.title.clone(), text: n.text.clone() }).collect();
    notes.extend((0..BLANK_ROWS).map(|_| blank_note()));
    let mut extras: Vec<ExtraRow> = c.extras.iter().map(|(k, v)| ExtraRow { key: k.clone(), value: v.clone() }).collect();
    extras.extend((0..BLANK_ROWS).map(|_| ExtraRow { key: String::new(), value: String::new() }));
    let mut subs = Vec::new();
    for (n, i) in c.ingredients.iter().enumerate() {
        for s in &i.substitutions {
            subs.push(SubRow { line: (n + 1).to_string(), qty: opt(s.qty), unit: s.unit.clone().unwrap_or_default(), food: s.food.clone().unwrap_or_default(), note: s.note.clone() });
        }
    }
    subs.extend((0..BLANK_ROWS).map(|_| SubRow { line: String::new(), qty: String::new(), unit: String::new(), food: String::new(), note: String::new() }));
    RecipeForm {
        heading,
        action,
        id,
        name: c.name.clone(),
        description: c.description.clone(),
        ingredients: ingredients.join("\n"),
        steps: steps.join("\n"),
        tags: c.tags.join(", "),
        categories: c.categories.join(", "),
        tools: c.tools.join(", "),
        cookbooks: c.cookbooks.join(", "),
        image,
        nutrition,
        notes,
        extras,
        subs,
        flags: flags(&c.settings),
        assets,
    }
}

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
    let assets = db.recipe_assets(&sc, id).await?.into_iter().map(|a| AssetView { name: a.name, file_name: a.file_name, hash: a.hash }).collect();
    let image = db.recipe_images(&sc, id).await?.first().map(crate::Image::file_name).unwrap_or_default();
    let nutrition = NutritionFields::new(id, svc.llm().is_some(), r.content.nutrition.as_ref()).html()?;
    Ok(form_for(id, "Edit recipe", format!("/recipes/{id}/edit"), &r.content, assets, image, nutrition))
}

#[derive(Deserialize)]
struct RecipeFormData {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    ingredients: String,
    // Absent means "leave it as it is" (an older form, or an API-style post); present, even blank, replaces it.
    steps: Option<String>,
    tags: Option<String>,
    categories: Option<String>,
    tools: Option<String>,
    cookbooks: Option<String>,
    calories: Option<String>,
    protein_g: Option<String>,
    fat_g: Option<String>,
    carbs_g: Option<String>,
}

/// One entry per non-blank line.
fn lines(v: &str) -> Vec<String> {
    v.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Lines where a `## Title` line gives the next line a title: `(title, text)` per real line.
/// A title with nothing after it is refused rather than dropped.
fn titled(v: &str, what: &str) -> Result<Vec<(String, String)>> {
    let (mut out, mut pending) = (Vec::new(), String::new());
    for l in lines(v) {
        match l.strip_prefix("##") {
            Some(t) => pending = t.trim().to_string(),
            None => out.push((std::mem::take(&mut pending), l)),
        }
    }
    if !pending.is_empty() {
        return Err(Error::Validation(format!("a {what} title (## ...) needs a line after it")));
    }
    Ok(out)
}

/// Comma separated names, trimmed, without blanks or repeats.
fn names(v: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in v.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !out.iter().any(|o| o == n) {
            out.push(n.to_string());
        }
    }
    out
}

/// The recipe form: urlencoded, or multipart when it carries an image. Both end up as the same fields.
struct Submission {
    form: RecipeFormData,
    /// Every field in order, repeats included (the rows of notes, extras and substitutions).
    pairs: Vec<(String, String)>,
    /// The file chosen in the `image` field; none when the field is empty.
    image: Option<axum::body::Bytes>,
}

impl<S: Send + Sync> FromRequest<S> for Submission {
    type Rejection = Error;
    async fn from_request(req: Request, state: &S) -> Result<Self> {
        let bad = |e: &dyn std::fmt::Display| Error::Validation(format!("bad form: {e}"));
        let multipart = req.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("multipart/form-data"));
        let (mut pairs, mut image): (Vec<(String, String)>, _) = (Vec::new(), None);
        if !multipart {
            let body = axum::body::Bytes::from_request(req, state).await.map_err(|e| bad(&e))?;
            pairs = serde_urlencoded::from_bytes(&body).map_err(|e| bad(&e))?;
        } else {
            let mut parts = Multipart::from_request(req, state).await.map_err(|e| bad(&e))?;
            while let Some(field) = parts.next_field().await.map_err(|e| bad(&e))? {
                let name = field.name().unwrap_or_default().to_string();
                if name == "image" {
                    image = Some(field.bytes().await.map_err(|e| bad(&e))?).filter(|b| !b.is_empty());
                } else {
                    pairs.push((name, field.text().await.map_err(|e| bad(&e))?));
                }
            }
        }
        // The struct takes the plain fields; a repeated key (a row) is not one of them.
        let plain: Vec<_> = pairs.iter().filter(|(k, _)| !ROW_FIELDS.contains(&k.as_str())).collect();
        let form = serde_urlencoded::from_str(&serde_urlencoded::to_string(&plain).map_err(|e| bad(&e))?).map_err(|e| bad(&e))?;
        Ok(Submission { form, pairs, image })
    }
}

/// Fields that come in rows, or are checkboxes; read from the pairs, not the struct.
const ROW_FIELDS: [&str; 18] = [
    "features", "note_title", "note_text", "extra_key", "extra_value", "sub_line", "sub_qty", "sub_unit", "sub_food", "sub_note",
    "locked", "hide_nutrition", "disable_comments", "public", "show_assets", "landscape_view", "disable_amount", "link",
];

/// What the rows of the form say about notes, extras, substitutions and settings; `None` when the form carried
/// no `features` marker (an older form, or an API-style post), which leaves all of them as they are.
struct Rows {
    notes: Vec<Note>,
    extras: std::collections::BTreeMap<String, String>,
    /// (1-based ingredient line number, substitution)
    subs: Vec<(usize, Substitution)>,
    settings: Settings,
}

fn rows(pairs: &[(String, String)]) -> Result<Option<Rows>> {
    if !pairs.iter().any(|(k, _)| k == "features") {
        return Ok(None);
    }
    let has = |k: &str| pairs.iter().any(|(n, _)| n == k);
    let mut notes: Vec<Note> = Vec::new();
    let mut extras = std::collections::BTreeMap::new();
    let mut raw_extras: Vec<(String, String)> = Vec::new();
    // (line, qty, unit, food, note) as typed
    let mut subs: Vec<[String; 5]> = Vec::new();
    for (k, v) in pairs {
        let v = v.clone();
        match k.as_str() {
            "note_title" => notes.push(Note { title: v, text: String::new() }),
            "note_text" => match notes.last_mut() {
                Some(n) if n.text.is_empty() => n.text = v,
                _ => notes.push(Note { title: String::new(), text: v }),
            },
            "extra_key" => raw_extras.push((v, String::new())),
            "extra_value" => {
                if let Some(e) = raw_extras.last_mut() {
                    e.1 = v;
                }
            }
            "sub_line" => subs.push([v, String::new(), String::new(), String::new(), String::new()]),
            "sub_qty" | "sub_unit" | "sub_food" | "sub_note" => {
                let col = ["sub_qty", "sub_unit", "sub_food", "sub_note"].iter().position(|c| c == k).expect("matched above") + 1;
                if let Some(r) = subs.last_mut() {
                    r[col] = v;
                }
            }
            _ => {}
        }
    }
    notes.retain(|n| !(n.title.trim().is_empty() && n.text.trim().is_empty()));
    for (k, v) in raw_extras {
        if !(k.trim().is_empty() && v.trim().is_empty()) {
            extras.insert(k, v);
        }
    }
    let mut out = Vec::new();
    for r in subs {
        if r.iter().all(|x| x.trim().is_empty()) {
            continue;
        }
        let line: usize = r[0].trim().parse().ok().filter(|n| *n >= 1).ok_or_else(|| Error::Validation("a substitution needs the number of its ingredient line".into()))?;
        out.push((line, Substitution { qty: number(&r[1])?, unit: Some(r[2].clone()), food: Some(r[3].clone()), note: r[4].clone() }));
    }
    let settings = Settings {
        locked: has("locked"),
        hide_nutrition: has("hide_nutrition"),
        disable_comments: has("disable_comments"),
        public: has("public"),
        show_assets: has("show_assets"),
        landscape_view: has("landscape_view"),
        disable_amount: has("disable_amount"),
    };
    Ok(Some(Rows { notes, extras, subs: out, settings }))
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

#[derive(Deserialize)]
struct ImportForm {
    url: String,
}

async fn import_form(sc: Scope, State(s): State<AppState>, axum::Extension(svc): Svc, axum::Form(f): axum::Form<ImportForm>) -> Result<Redirect> {
    let (r, suggested) = svc.import_suggested(&s.db, &sc, &s.images_dir, &f.url).await?;
    Ok(Redirect::to(&review_url(r.id, &suggested)))
}

async fn create_recipe(sc: Scope, State(s): State<AppState>, axum::Extension(svc): Svc, Submission { form: f, pairs, image }: Submission) -> Result<Redirect> {
    let db = &s.db;
    let (units, foods) = (db.units(&sc).await?, db.foods(&sc).await?);
    let nutrition = f.nutrition()?.flatten();
    let typed = titled(&f.ingredients, "section")?;
    let mut ingredients: Vec<IngredientInput> = typed.iter().map(|(_, l)| parsed_input(l, &units, &foods)).collect();
    let all = vec![true; ingredients.len()];
    let suggested = svc.fill_lines(&mut ingredients, &all, &units, &foods).await;
    let mut details: Vec<LineDetail> = typed.iter().map(|(section, _)| LineDetail { section: section.clone(), ..Default::default() }).collect();
    let rows = rows(&pairs)?;
    if let Some(r) = &rows {
        add_subs(&mut details, &r.subs)?;
    }
    let steps = f.steps.as_deref().map(|v| titled(v, "step")).transpose()?.unwrap_or_default();
    let photo = match image {
        Some(bytes) => Some(crate::images::prepare(&bytes).await?),
        None => None,
    };
    let input = RecipeInput {
        name: f.name,
        description: f.description,
        nutrition,
        tags: f.tags.as_deref().map(names).unwrap_or_default(),
        categories: f.categories.as_deref().map(names).unwrap_or_default(),
        cookbooks: f.cookbooks.as_deref().map(names).unwrap_or_default(),
        tools: f.tools.as_deref().map(names).unwrap_or_default(),
        ingredients,
        ingredient_details: details,
        step_details: steps.iter().map(|(title, _)| StepDetail { title: title.clone(), ..Default::default() }).collect(),
        steps: steps.into_iter().map(|(_, t)| t).collect(),
        notes: rows.as_ref().map(|r| r.notes.clone()),
        extras: rows.as_ref().map(|r| r.extras.clone()),
        settings: rows.map(|r| r.settings),
        ..Default::default()
    };
    let r = db.create_recipe_with(&sc, input, photo.as_ref().map(|p| (p, s.images_dir.as_path()))).await?;
    Ok(Redirect::to(&review_url(r.id, &suggested)))
}

/// Puts each typed substitution on its ingredient line (numbered from 1).
fn add_subs(details: &mut [LineDetail], subs: &[(usize, Substitution)]) -> Result<()> {
    let count = details.len();
    for (line, s) in subs {
        let d = details.get_mut(line - 1).ok_or_else(|| Error::Validation(format!("substitution for ingredient line {line}, but there are only {count}")))?;
        d.substitutions.push(s.clone());
    }
    Ok(())
}

async fn edit_recipe(sc: Scope, State(s): State<AppState>, axum::Extension(svc): Svc, Path(id): Path<i64>, Submission { form: f, pairs, image }: Submission) -> Result<Redirect> {
    let db = &s.db;
    let old = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?.content;
    let (units, foods) = (db.units(&sc).await?, db.foods(&sc).await?);
    let nutrition = f.nutrition()?.unwrap_or(old.nutrition.clone());
    let rows = rows(&pairs)?;
    // A line left as it was keeps what the user already reviewed (and is not sent to the LLM again), and its id,
    // so steps still link to it; only new or changed text is parsed. Each old line is claimed once, so two equal
    // lines stay two lines.
    let (mut ingredients, mut is_new, mut details) = (Vec::new(), Vec::new(), Vec::new());
    let mut claimed = vec![false; old.ingredients.len()];
    for (section, l) in titled(&f.ingredients, "section")? {
        let known = old.ingredients.iter().enumerate().find(|(n, i)| !claimed[*n] && i.original_text == l);
        if let Some((n, _)) = known {
            claimed[n] = true;
        }
        is_new.push(known.is_none());
        ingredients.push(known.map_or_else(|| parsed_input(&l, &units, &foods), |(_, i)| IngredientInput::from(i)));
        details.push(LineDetail {
            uid: known.map(|(_, i)| i.uid.clone()),
            section,
            // The rows of the form are the whole truth once the form sends them; an older post leaves them be.
            substitutions: known.filter(|_| rows.is_none()).map(|(_, i)| i.substitutions.clone()).unwrap_or_default(),
        });
    }
    if let Some(r) = &rows {
        add_subs(&mut details, &r.subs)?;
    }
    let suggested = svc.fill_lines(&mut ingredients, &is_new, &units, &foods).await;
    // The same for steps: the text decides which old step this is, so its links carry over (to lines that are still there).
    let step_texts = f.steps.as_deref().map(|v| titled(v, "step")).transpose()?;
    let (steps, step_details) = match step_texts {
        None => (old.steps.clone(), old.step_details.clone()),
        Some(typed) => {
            let mut taken = vec![false; old.steps.len()];
            let surviving: Vec<&str> = details.iter().filter_map(|d| d.uid.as_deref()).collect();
            let mut out = Vec::new();
            for (title, text) in &typed {
                let from = old.steps.iter().enumerate().find(|(n, t)| !taken[*n] && *t == text).map(|(n, _)| n);
                if let Some(n) = from {
                    taken[n] = true;
                }
                let links = from.and_then(|n| old.step_details.get(n)).map(|d| d.ingredient_uids.iter().filter(|u| surviving.contains(&u.as_str())).cloned().collect()).unwrap_or_default();
                out.push(StepDetail { title: title.clone(), ingredient_uids: links });
            }
            (typed.into_iter().map(|(_, t)| t).collect(), out)
        }
    };
    let photo = match image {
        Some(bytes) => Some(crate::images::prepare(&bytes).await?),
        None => None,
    };
    db.update_recipe_with(
        &sc,
        id,
        RecipeInput {
            name: f.name,
            description: f.description,
            source_url: old.source_url,
            nutrition,
            tags: f.tags.as_deref().map(names).unwrap_or(old.tags),
            categories: f.categories.as_deref().map(names).unwrap_or(old.categories),
            cookbooks: f.cookbooks.as_deref().map(names).unwrap_or(old.cookbooks),
            tools: f.tools.as_deref().map(names).unwrap_or(old.tools),
            ingredients,
            ingredient_details: details,
            steps,
            step_details,
            notes: rows.as_ref().map(|r| r.notes.clone()),
            extras: rows.as_ref().map(|r| r.extras.clone()),
            settings: rows.map(|r| r.settings),
        },
        photo.as_ref().map(|p| (p, s.images_dir.as_path())),
    )
    .await?;
    Ok(Redirect::to(&review_url(id, &suggested)))
}

// ---------- linking ingredients to steps ----------

struct LinkCell {
    value: String,
    checked: bool,
}

struct LinkRow {
    title: String,
    text: String,
    cells: Vec<LinkCell>,
}

#[derive(Template, WebTemplate)]
#[template(path = "link.html")]
struct LinkPage {
    id: i64,
    name: String,
    /// The ingredient lines, as column headings.
    lines: Vec<String>,
    rows: Vec<LinkRow>,
    /// Lines written before step 14 have no id until the recipe is saved once.
    unlinkable: bool,
}

async fn link_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<LinkPage> {
    let c = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?.content;
    let rows = c
        .steps
        .iter()
        .enumerate()
        .map(|(n, text)| {
            let d = c.step_details.get(n);
            LinkRow {
                title: d.map(|d| d.title.clone()).unwrap_or_default(),
                text: text.clone(),
                cells: c.ingredients.iter().map(|i| LinkCell { value: format!("{n}:{}", i.uid), checked: d.is_some_and(|d| d.ingredient_uids.contains(&i.uid)) }).collect(),
            }
        })
        .collect();
    Ok(LinkPage { id, name: c.name.clone(), lines: c.ingredients.iter().map(|i| ingredient_line(i, 1.0)).collect(), rows, unlinkable: c.ingredients.iter().any(|i| i.uid.is_empty()) })
}

async fn link_save(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, body: axum::body::Bytes) -> Result<Redirect> {
    let c = db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?.content;
    let mut details: Vec<StepDetail> = (0..c.steps.len()).map(|n| StepDetail { title: c.step_details.get(n).map(|d| d.title.clone()).unwrap_or_default(), ingredient_uids: Vec::new() }).collect();
    for (k, v) in super::pairs(&body)? {
        if k != "link" {
            continue;
        }
        let (step, uid) = v.split_once(':').ok_or_else(|| Error::Validation(format!("'{v}' is not a step and an ingredient line")))?;
        let step: usize = step.parse().map_err(|_| Error::Validation(format!("'{step}' is not a step number")))?;
        details.get_mut(step).ok_or_else(|| Error::Validation(format!("the recipe has no step {step}")))?.ingredient_uids.push(uid.to_string());
    }
    let mut input = RecipeInput::from(c);
    input.step_details = details;
    db.update_recipe(&sc, id, input).await?;
    Ok(Redirect::to(&format!("/recipes/{id}")))
}

// ---------- attachments ----------

async fn add_asset(sc: Scope, State(s): State<AppState>, Path(id): Path<i64>, form: Multipart) -> Result<Redirect> {
    let u = crate::features::read_upload(form).await?;
    s.db.add_recipe_asset(&sc, id, &s.assets_dir(), &u.file_name, &u.name, &u.icon, &u.bytes).await?;
    Ok(Redirect::to(&format!("/recipes/{id}")))
}

async fn delete_asset(sc: Scope, State(db): State<Db>, Path((id, hash)): Path<(i64, String)>) -> Result<Redirect> {
    db.remove_recipe_asset(&sc, id, &hash).await?;
    Ok(Redirect::to(&format!("/recipes/{id}")))
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
    // The review edits the same lines in the same order, so what they carry stays with them; rows dropped take their links with them.
    let details: Vec<LineDetail> = old.ingredients.iter().take(rows.len()).map(|i| LineDetail { uid: Some(i.uid.clone()), section: i.section.clone(), substitutions: i.substitutions.clone() }).collect();
    let step_details: Vec<StepDetail> = old.step_details.iter().map(|d| StepDetail { title: d.title.clone(), ingredient_uids: d.ingredient_uids.iter().filter(|u| details.iter().any(|x| x.uid.as_deref() == Some(u.as_str()))).cloned().collect() }).collect();
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
            ingredient_details: details,
            steps: old.steps,
            step_details,
            notes: None,
            extras: None,
            settings: None,
        },
    )
    .await?;
    Ok(Redirect::to(&format!("/recipes/{id}")))
}
