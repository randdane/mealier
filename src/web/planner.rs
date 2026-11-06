//! The meal plan page.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Redirect;
use axum::routing::{get, post};
use serde::Deserialize;

use super::{id_of, number};
use crate::api::AppState;
use crate::{Db, Error, Result, Scope};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/plan", get(plan_page))
        .route("/plan/entries", post(plan_add))
        .route("/plan/entries/{id}/delete", post(plan_delete))
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
