//! The meal plan page.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use axum::routing::{get, post};
use serde::Deserialize;

use super::{id_of, number, pairs};
use crate::api::AppState;
use crate::{Db, Error, Result, Scope};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/plan", get(plan_page))
        .route("/plan/entries", post(plan_add))
        .route("/plan/entries/{id}/delete", post(plan_delete))
        .route("/plan/suggest", get(plan_suggest))
        .route("/settings/rules", get(rules_page).post(rule_create))
        .route("/settings/rules/{id}", post(rule_update))
        .route("/settings/rules/{id}/delete", post(rule_delete))
}

impl Db {
    /// `date(base, '+n days')`; `base` may be "now". Errors when `base` is not a date.
    pub(super) async fn day(&self, base: &str, n: i64) -> Result<String> {
        let d: Option<String> = sqlx::query_scalar("SELECT date(?, ? || ' days')").bind(base).bind(n).fetch_one(&self.pool).await?;
        d.ok_or_else(|| Error::Validation("date must be YYYY-MM-DD".into()))
    }

    /// The Monday of the week holding `d`.
    pub(super) async fn monday(&self, d: &str) -> Result<String> {
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

struct SlotOpt {
    name: &'static str,
    selected: bool,
}

struct DayRow {
    date: String,
    name: String,
    entries: Vec<EntryRow>,
    /// Meals with nothing planned: where "Suggest" is offered.
    free_slots: Vec<&'static str>,
    /// The add form starts open, with a suggestion in it.
    open: bool,
    slot_opts: Vec<SlotOpt>,
    /// The suggested recipe, 0 for none.
    suggested: i64,
}

#[derive(Template, WebTemplate)]
#[template(path = "plan.html")]
struct PlanPage {
    week: String,
    prev: String,
    next: String,
    days: Vec<DayRow>,
    recipes: Vec<crate::RecipeSummary>,
    /// "No rule has a suggestion for ..." after a suggestion came back empty.
    note: String,
}

#[derive(Deserialize)]
struct WeekQuery {
    week: Option<String>,
    /// Set by `/plan/suggest`: the day and meal asked about, the recipe it found, or `none`.
    date: Option<String>,
    slot: Option<String>,
    recipe: Option<i64>,
    none: Option<String>,
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
        let entries: Vec<EntryRow> = entries
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
        let asked = q.date.as_deref() == Some(date.as_str());
        let slot = q.slot.as_deref().filter(|_| asked).unwrap_or("dinner");
        let free_slots = crate::SLOTS.into_iter().filter(|s| !entries.iter().any(|e| e.slot == *s)).collect();
        days.push(DayRow {
            open: asked,
            slot_opts: crate::SLOTS.into_iter().map(|name| SlotOpt { name, selected: name == slot }).collect(),
            suggested: if asked { q.recipe.unwrap_or(0) } else { 0 },
            free_slots,
            date,
            name,
            entries,
        });
    }
    let note = match (&q.none, &q.date, &q.slot) {
        (Some(_), Some(d), Some(s)) => format!("No rule has a suggestion for {s} on {d}. Add one under Rules."),
        _ => String::new(),
    };
    Ok(PlanPage { prev: db.day(&week, -7).await?, next: db.day(&week, 7).await?, week, days, recipes: recipes.into_iter().filter(|r| !r.archived).collect(), note })
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

#[derive(Deserialize)]
struct SuggestQuery {
    date: String,
    slot: String,
}

/// Asks the rules for a recipe and sends the browser back to the plan with the add form filled in.
async fn plan_suggest(sc: Scope, State(db): State<Db>, Query(q): Query<SuggestQuery>) -> Result<Redirect> {
    let found = db.suggest(&sc, &q.date, &q.slot, &mut crate::Rng::from_entropy()).await?;
    // date and slot are valid here: `suggest` refused anything but YYYY-MM-DD and a known meal
    let base = format!("/plan?week={}&date={}&slot={}", db.monday(&q.date).await?, q.date, q.slot);
    Ok(Redirect::to(&match found {
        Some(r) => format!("{base}&recipe={}", r.id),
        None => format!("{base}&none=1"),
    }))
}

// Meal-plan rules (group-wide; any signed-in user).

struct Opt {
    value: &'static str,
    selected: bool,
}

struct Tick {
    id: i64,
    name: String,
    checked: bool,
}

struct RuleRow {
    id: i64,
    query_filter: String,
    supported: bool,
    reason: String,
    days: Vec<Opt>,
    slots: Vec<Opt>,
    households: Vec<Tick>,
}

#[derive(Template, WebTemplate)]
#[template(path = "rules.html")]
struct RulesPage {
    rules: Vec<RuleRow>,
    days: Vec<&'static str>,
    slots: Vec<&'static str>,
    households: Vec<crate::Named>,
}

fn opts(all: impl IntoIterator<Item = &'static str>, current: &str) -> Vec<Opt> {
    ["unset"].into_iter().chain(all).map(|value| Opt { value, selected: value == current }).collect()
}

async fn rules_page(sc: Scope, State(db): State<Db>) -> Result<RulesPage> {
    let households = db.group_households(&sc).await?;
    let rules = db
        .rules(&sc)
        .await?
        .into_iter()
        .map(|r| RuleRow {
            reason: match crate::filter::parse(&r.query_filter) {
                Err(e) => e.to_string(),
                Ok(_) => "filter not supported".into(),
            },
            days: opts(crate::DAYS, &r.day),
            slots: opts(crate::SLOTS, &r.slot),
            households: households.iter().map(|h| Tick { id: h.id, name: h.name.clone(), checked: r.household_ids.contains(&h.id) }).collect(),
            id: r.id,
            query_filter: r.query_filter,
            supported: r.filter_supported,
        })
        .collect();
    Ok(RulesPage { rules, days: ["unset"].into_iter().chain(crate::DAYS).collect(), slots: ["unset"].into_iter().chain(crate::SLOTS).collect(), households })
}

/// day, slot, filter and the ticked households of a rule form.
fn rule_form(body: &Bytes) -> Result<crate::RuleInput> {
    let form = pairs(body)?;
    let one = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default();
    let mut household_ids = Vec::new();
    for (_, v) in form.iter().filter(|(k, _)| k == "households") {
        household_ids.extend(id_of(v)?);
    }
    Ok(crate::RuleInput { day: one("day"), slot: one("slot"), query_filter: one("query_filter"), household_ids })
}

async fn rule_create(sc: Scope, State(db): State<Db>, body: Bytes) -> Result<Redirect> {
    db.add_rule(&sc, rule_form(&body)?).await?;
    Ok(Redirect::to("/settings/rules"))
}

async fn rule_update(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, body: Bytes) -> Result<Redirect> {
    db.update_rule(&sc, id, rule_form(&body)?).await?;
    Ok(Redirect::to("/settings/rules"))
}

async fn rule_delete(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_rule(&sc, id).await?;
    Ok(Redirect::to("/settings/rules"))
}
