//! Meal-plan rules: group-scoped filters that say which recipes may be suggested for a day and slot.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sqlx::Row;

use crate::api::{AppState, ErrorBody};
use crate::cookbooks::{Named, read_filter};
use crate::filter::Sel;
use crate::{Db, Error, RecipeSummary, Result, SLOTS, Scope};

const COLS: &str = "id, day, slot, query_filter, filter_json";

pub const DAYS: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct MealPlanRule {
    pub id: i64,
    /// monday to sunday, or `unset` for any day.
    pub day: String,
    /// A meal slot, or `unset` for any.
    pub slot: String,
    /// Mealie's filter string, as written. Empty means every recipe.
    pub query_filter: String,
    /// False when `query_filter` is set but unreadable (an imported one): the rule then selects nothing.
    pub filter_supported: bool,
    /// The households the rule is limited to; empty means every household of the group.
    pub household_ids: Vec<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(default)]
pub struct RuleInput {
    /// monday to sunday; `unset` or empty for any.
    pub day: String,
    /// A meal slot; `unset` or empty for any.
    pub slot: String,
    pub query_filter: String,
    pub household_ids: Vec<i64>,
}

/// A small seedable generator (splitmix64), so a suggestion can be reproduced in a test.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn from_entropy() -> Rng {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        Rng(u64::from_le_bytes(b))
    }

    /// 0 to `n` - 1; `n` must not be 0.
    pub fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        // ponytail: modulo bias, immaterial for a handful of recipes
        ((z ^ (z >> 31)) % n as u64) as usize
    }
}

/// `unset` or empty for any; otherwise one of `allowed`.
fn choice(v: &str, allowed: &[&str], what: &str) -> Result<String> {
    let v = v.trim().to_lowercase();
    if v.is_empty() || v == "unset" {
        return Ok("unset".into());
    }
    if !allowed.contains(&v.as_str()) {
        return Err(Error::Validation(format!("{what} must be unset or one of {}", allowed.join(", "))));
    }
    Ok(v)
}

async fn households_of(db: &Db, rule: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar("SELECT household_id FROM meal_plan_rule_households WHERE rule_id = ? ORDER BY household_id").bind(rule).fetch_all(&db.pool).await?)
}

fn rule_of(r: &sqlx::sqlite::SqliteRow, household_ids: Vec<i64>) -> MealPlanRule {
    let (query_filter, json): (String, Option<String>) = (r.get(3), r.get(4));
    MealPlanRule { id: r.get(0), day: r.get(1), slot: r.get(2), filter_supported: query_filter.trim().is_empty() || json.is_some(), query_filter, household_ids }
}

/// Checks the input and returns day, slot, filter string, parsed filter and the distinct household ids,
/// every one of which must be a household of the caller's group.
async fn checked(db: &Db, sc: &Scope, input: &RuleInput) -> Result<(String, String, String, Option<String>, Vec<i64>)> {
    let day = choice(&input.day, &DAYS, "day")?;
    let slot = choice(&input.slot, &SLOTS, "slot")?;
    let (query_filter, json) = read_filter(&input.query_filter)?;
    let mut households = input.household_ids.clone();
    households.sort();
    households.dedup();
    for h in &households {
        let known = sqlx::query("SELECT 1 FROM households WHERE id = ? AND group_id = ?").bind(h).bind(sc.group_id).fetch_optional(&db.pool).await?;
        if known.is_none() {
            return Err(Error::NotFound("household"));
        }
    }
    Ok((day, slot, query_filter, json, households))
}

impl Db {
    pub async fn rules(&self, sc: &Scope) -> Result<Vec<MealPlanRule>> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM meal_plan_rules WHERE group_id = ? ORDER BY id")).bind(sc.group_id).fetch_all(&self.pool).await?;
        let mut out = Vec::new();
        for r in &rows {
            out.push(rule_of(r, households_of(self, r.get(0)).await?));
        }
        Ok(out)
    }

    pub async fn rule(&self, sc: &Scope, id: i64) -> Result<MealPlanRule> {
        let r = sqlx::query(&format!("SELECT {COLS} FROM meal_plan_rules WHERE id = ? AND group_id = ?")).bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?.ok_or(Error::NotFound("meal plan rule"))?;
        Ok(rule_of(&r, households_of(self, id).await?))
    }

    pub async fn add_rule(&self, sc: &Scope, input: RuleInput) -> Result<MealPlanRule> {
        let (day, slot, query_filter, json, households) = checked(self, sc, &input).await?;
        let mut tx = self.pool.begin().await?;
        let id = sqlx::query("INSERT INTO meal_plan_rules (group_id, day, slot, query_filter, filter_json) VALUES (?, ?, ?, ?, ?)")
            .bind(sc.group_id)
            .bind(&day)
            .bind(&slot)
            .bind(&query_filter)
            .bind(&json)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        for h in &households {
            sqlx::query("INSERT INTO meal_plan_rule_households (rule_id, household_id) VALUES (?, ?)").bind(id).bind(h).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.rule(sc, id).await
    }

    pub async fn update_rule(&self, sc: &Scope, id: i64, input: RuleInput) -> Result<MealPlanRule> {
        self.rule(sc, id).await?;
        let (day, slot, query_filter, json, households) = checked(self, sc, &input).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE meal_plan_rules SET day = ?, slot = ?, query_filter = ?, filter_json = ? WHERE id = ? AND group_id = ?")
            .bind(&day)
            .bind(&slot)
            .bind(&query_filter)
            .bind(&json)
            .bind(id)
            .bind(sc.group_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM meal_plan_rule_households WHERE rule_id = ?").bind(id).execute(&mut *tx).await?;
        for h in &households {
            sqlx::query("INSERT INTO meal_plan_rule_households (rule_id, household_id) VALUES (?, ?)").bind(id).bind(h).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.rule(sc, id).await
    }

    pub async fn delete_rule(&self, sc: &Scope, id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM meal_plan_rules WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("meal plan rule"));
        }
        Ok(())
    }

    /// A random live recipe of the caller's group chosen by the most specific rule that applies to the
    /// caller's household on `date` (YYYY-MM-DD) in `slot`: day and slot, then day, then slot, then neither.
    /// Rules of the same specificity are pooled. `None` when no rule applies or the rule selects nothing.
    pub async fn suggest(&self, sc: &Scope, date: &str, slot: &str, rng: &mut Rng) -> Result<Option<RecipeSummary>> {
        if !SLOTS.contains(&slot) {
            return Err(Error::Validation(format!("slot must be one of {}", SLOTS.join(", "))));
        }
        let weekday: Option<String> = sqlx::query_scalar("SELECT CASE WHEN date(?1) = ?1 THEN strftime('%w', ?1) END").bind(date).fetch_one(&self.pool).await?;
        let Some(w) = weekday.and_then(|w| w.parse::<usize>().ok()) else {
            return Err(Error::Validation("date must be YYYY-MM-DD".into()));
        };
        let day = DAYS[(w + 6) % 7];
        let rows = sqlx::query(
            "SELECT day, slot, query_filter, filter_json FROM meal_plan_rules r
             WHERE r.group_id = ? AND r.day IN (?, 'unset') AND r.slot IN (?, 'unset')
               AND (NOT EXISTS (SELECT 1 FROM meal_plan_rule_households h WHERE h.rule_id = r.id)
                    OR EXISTS (SELECT 1 FROM meal_plan_rule_households h WHERE h.rule_id = r.id AND h.household_id = ?))",
        )
        .bind(sc.group_id)
        .bind(day)
        .bind(slot)
        .bind(sc.household_id)
        .fetch_all(&self.pool)
        .await?;
        let mut best: Option<(u8, Vec<Sel>)> = None;
        for r in &rows {
            let tier = 2 * u8::from(r.get::<String, _>(0) != "unset") + u8::from(r.get::<String, _>(1) != "unset");
            let sel = Sel::stored(r.get(2), r.get::<Option<String>, _>(3).as_deref())?;
            match &mut best {
                Some((t, sels)) if *t == tier => sels.push(sel),
                Some((t, _)) if *t > tier => {}
                _ => best = Some((tier, vec![sel])),
            }
        }
        let Some((_, sels)) = best else { return Ok(None) };
        let mut recipes = self.select_recipes(sc, &sels).await?;
        if recipes.is_empty() {
            return Ok(None);
        }
        let i = rng.below(recipes.len());
        Ok(Some(recipes.swap_remove(i)))
    }
}

// ---------- API ----------

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/meal-plan-rules", get(list_rules).post(create_rule))
        .route("/api/meal-plan-rules/{id}", get(get_rule).put(put_rule).delete(delete_rule_route))
        .route("/api/meal-plan/suggest", post(suggest_route))
}

#[utoipa::path(get, path = "/api/meal-plan-rules", responses((status = 200, description = "The group's meal-plan rules", body = [MealPlanRule])))]
pub(crate) async fn list_rules(sc: Scope, State(db): State<Db>) -> Result<Json<Vec<MealPlanRule>>> {
    Ok(Json(db.rules(&sc).await?))
}

#[utoipa::path(
    post, path = "/api/meal-plan-rules", request_body = RuleInput,
    responses(
        (status = 201, description = "Rule created", body = MealPlanRule),
        (status = 404, description = "A household that is not in the caller's group", body = ErrorBody),
        (status = 422, description = "Bad day or slot, or a filter mealier cannot read (the error names the token)", body = ErrorBody),
    )
)]
pub(crate) async fn create_rule(sc: Scope, State(db): State<Db>, Json(i): Json<RuleInput>) -> Result<(StatusCode, Json<MealPlanRule>)> {
    Ok((StatusCode::CREATED, Json(db.add_rule(&sc, i).await?)))
}

#[utoipa::path(
    get, path = "/api/meal-plan-rules/{id}", params(("id" = i64, Path, description = "Rule id")),
    responses((status = 200, description = "The rule", body = MealPlanRule), (status = 404, description = "No such rule", body = ErrorBody))
)]
pub(crate) async fn get_rule(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<MealPlanRule>> {
    Ok(Json(db.rule(&sc, id).await?))
}

#[utoipa::path(
    put, path = "/api/meal-plan-rules/{id}", params(("id" = i64, Path, description = "Rule id")), request_body = RuleInput,
    responses((status = 200, description = "Rule replaced", body = MealPlanRule), (status = 404, description = "No such rule or household", body = ErrorBody), (status = 422, description = "Invalid rule", body = ErrorBody))
)]
pub(crate) async fn put_rule(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<RuleInput>) -> Result<Json<MealPlanRule>> {
    Ok(Json(db.update_rule(&sc, id, i).await?))
}

#[utoipa::path(
    delete, path = "/api/meal-plan-rules/{id}", params(("id" = i64, Path, description = "Rule id")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "No such rule", body = ErrorBody))
)]
pub(crate) async fn delete_rule_route(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode> {
    db.delete_rule(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Default, Deserialize, ToSchema)]
#[serde(default)]
pub(crate) struct SuggestInput {
    /// YYYY-MM-DD
    date: String,
    /// breakfast, lunch, dinner, side, snack, dessert or drink
    slot: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct Suggestion {
    /// A random recipe the most specific applicable rule allows; null when no rule applies or it selects nothing.
    recipe: Option<RecipeSummary>,
}

#[utoipa::path(
    post, path = "/api/meal-plan/suggest", request_body = SuggestInput,
    responses((status = 200, description = "A suggestion, or null", body = Suggestion), (status = 422, description = "Bad date or slot", body = ErrorBody))
)]
pub(crate) async fn suggest_route(sc: Scope, State(db): State<Db>, Json(i): Json<SuggestInput>) -> Result<Json<Suggestion>> {
    Ok(Json(Suggestion { recipe: db.suggest(&sc, &i.date, &i.slot, &mut Rng::from_entropy()).await? }))
}

impl Db {
    /// The households of the caller's group, by name.
    pub async fn group_households(&self, sc: &Scope) -> Result<Vec<Named>> {
        let rows = sqlx::query("SELECT id, name FROM households WHERE group_id = ? ORDER BY name, id").bind(sc.group_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Named { id: r.get(0), name: r.get(1) }).collect())
    }
}
