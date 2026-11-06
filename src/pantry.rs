//! Step 14, part B: foods on hand and tools owned. The foods and tools are the group's; which of them a
//! household has is the household's own.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::{AppState, ErrorBody};
use sqlx::Row;

use crate::{Db, Error, Result, Scope};

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct PantryItem {
    pub id: i64,
    pub name: String,
    /// The caller's household has it.
    pub have: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Pantry {
    pub foods: Vec<PantryItem>,
    pub tools: Vec<PantryItem>,
}

impl Db {
    pub async fn set_food_on_hand(&self, sc: &Scope, food_id: i64, on_hand: bool) -> Result<()> {
        self.have(sc, "foods", "household_foods", "food_id", "food", food_id, on_hand).await
    }

    pub async fn set_tool_owned(&self, sc: &Scope, tool_id: i64, owned: bool) -> Result<()> {
        self.have(sc, "tools", "household_tools", "tool_id", "tool", tool_id, owned).await
    }

    /// Adds or removes the caller's household's row for a food or tool of the caller's group.
    #[allow(clippy::too_many_arguments)]
    async fn have(&self, sc: &Scope, table: &str, link: &str, col: &str, what: &'static str, id: i64, on: bool) -> Result<()> {
        let found = sqlx::query(&format!("SELECT 1 FROM {table} WHERE id = ? AND group_id = ?")).bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?;
        if found.is_none() {
            return Err(Error::NotFound(what));
        }
        let sql = if on { format!("INSERT OR IGNORE INTO {link} (household_id, {col}) VALUES (?, ?)") } else { format!("DELETE FROM {link} WHERE household_id = ? AND {col} = ?") };
        sqlx::query(&sql).bind(sc.household_id).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// The group's foods and tools by name, each marked for the caller's household.
    pub async fn pantry(&self, sc: &Scope) -> Result<Pantry> {
        let items = |table: &'static str, link: &'static str, col: &'static str| async move {
            let rows = sqlx::query(&format!("SELECT t.id, t.name, EXISTS (SELECT 1 FROM {link} h WHERE h.{col} = t.id AND h.household_id = ?) FROM {table} t WHERE t.group_id = ? ORDER BY t.name, t.id"))
                .bind(sc.household_id)
                .bind(sc.group_id)
                .fetch_all(&self.pool)
                .await?;
            Ok::<_, crate::Error>(rows.iter().map(|r| PantryItem { id: r.get(0), name: r.get(1), have: r.get(2) }).collect::<Vec<_>>())
        };
        Ok(Pantry { foods: items("foods", "household_foods", "food_id").await?, tools: items("tools", "household_tools", "tool_id").await? })
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct OnHandInput {
    pub on_hand: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct OwnedInput {
    pub owned: bool,
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/pantry", get(get_pantry)).route("/api/foods/{id}/on-hand", put(put_on_hand)).route("/api/tools/{id}/owned", put(put_owned))
}

#[utoipa::path(get, path = "/api/pantry", responses((status = 200, description = "The group's foods and tools, each marked for the caller's household", body = Pantry)))]
pub(crate) async fn get_pantry(sc: Scope, State(db): State<Db>) -> Result<Json<Pantry>> {
    Ok(Json(db.pantry(&sc).await?))
}

#[utoipa::path(
    put, path = "/api/foods/{id}/on-hand", params(("id" = i64, Path, description = "Food id")), request_body = OnHandInput,
    responses((status = 204, description = "Set for the caller's household"), (status = 404, description = "No such food in the caller's group", body = ErrorBody))
)]
pub(crate) async fn put_on_hand(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<OnHandInput>) -> Result<StatusCode> {
    db.set_food_on_hand(&sc, id, i.on_hand).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    put, path = "/api/tools/{id}/owned", params(("id" = i64, Path, description = "Tool id")), request_body = OwnedInput,
    responses((status = 204, description = "Set for the caller's household"), (status = 404, description = "No such tool in the caller's group", body = ErrorBody))
)]
pub(crate) async fn put_owned(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<OwnedInput>) -> Result<StatusCode> {
    db.set_tool_owned(&sc, id, i.owned).await?;
    Ok(StatusCode::NO_CONTENT)
}
