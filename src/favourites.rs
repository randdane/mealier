//! Step 14, part B: favourites and ratings. User-scoped: each user has their own, per recipe, and the
//! recipe itself must be in the user's group.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use sqlx::Row;

use crate::api::{AppState, ErrorBody};
use crate::{Db, Error, RecipeSummary, Result, Scope};

/// One user's opinion of one recipe.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct Opinion {
    pub favourite: bool,
    /// 0.5 to 5 in half steps; none when unrated.
    pub rating: Option<f64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FavouriteInput {
    pub favourite: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RatingInput {
    /// 0.5 to 5 in half steps; null clears it.
    pub rating: Option<f64>,
}

/// 0.5 to 5 in half steps, or none.
pub(crate) fn check_rating(r: Option<f64>) -> Result<Option<f64>> {
    match r {
        Some(v) if !(v.is_finite() && (0.5..=5.0).contains(&v) && (v * 2.0).fract() == 0.0) => Err(Error::Validation("a rating is 0.5 to 5 in half steps".into())),
        r => Ok(r),
    }
}

impl Db {
    pub async fn opinion(&self, sc: &Scope, recipe_id: i64) -> Result<Opinion> {
        self.check_recipe(sc, recipe_id).await?;
        let row: Option<(bool, Option<f64>)> = sqlx::query_as("SELECT is_favorite, rating FROM user_recipes WHERE user_id = ? AND recipe_id = ?").bind(sc.user_id).bind(recipe_id).fetch_optional(&self.pool).await?;
        Ok(row.map_or(Opinion { favourite: false, rating: None }, |(favourite, rating)| Opinion { favourite, rating }))
    }

    pub async fn set_favourite(&self, sc: &Scope, recipe_id: i64, favourite: bool) -> Result<()> {
        self.check_recipe(sc, recipe_id).await?;
        sqlx::query("INSERT INTO user_recipes (user_id, recipe_id, is_favorite) VALUES (?, ?, ?) ON CONFLICT (user_id, recipe_id) DO UPDATE SET is_favorite = excluded.is_favorite")
            .bind(sc.user_id)
            .bind(recipe_id)
            .bind(favourite)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_rating(&self, sc: &Scope, recipe_id: i64, rating: Option<f64>) -> Result<()> {
        let rating = check_rating(rating)?;
        self.check_recipe(sc, recipe_id).await?;
        sqlx::query("INSERT INTO user_recipes (user_id, recipe_id, rating) VALUES (?, ?, ?) ON CONFLICT (user_id, recipe_id) DO UPDATE SET rating = excluded.rating")
            .bind(sc.user_id)
            .bind(recipe_id)
            .bind(rating)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The caller's favourite live recipes of their group, by name.
    pub async fn favourite_recipes(&self, sc: &Scope) -> Result<Vec<RecipeSummary>> {
        let rows = sqlx::query("SELECT r.id, r.name, r.archived FROM recipes r JOIN user_recipes u ON u.recipe_id = r.id WHERE u.user_id = ? AND u.is_favorite = 1 AND r.group_id = ? AND r.archived = 0 ORDER BY r.name, r.id")
            .bind(sc.user_id)
            .bind(sc.group_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| RecipeSummary { id: r.get(0), name: r.get(1), archived: r.get(2) }).collect())
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/recipes/{id}/mine", get(get_mine)).route("/api/recipes/{id}/favourite", put(put_favourite)).route("/api/recipes/{id}/rating", put(put_rating))
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/mine", params(("id" = i64, Path, description = "Recipe id")),
    responses((status = 200, description = "The caller's own favourite flag and rating of the recipe", body = Opinion), (status = 404, description = "No such recipe", body = ErrorBody))
)]
pub(crate) async fn get_mine(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Opinion>> {
    Ok(Json(db.opinion(&sc, id).await?))
}

#[utoipa::path(
    put, path = "/api/recipes/{id}/favourite", params(("id" = i64, Path, description = "Recipe id")), request_body = FavouriteInput,
    responses((status = 204, description = "Set, for the caller only"), (status = 404, description = "No such recipe", body = ErrorBody))
)]
pub(crate) async fn put_favourite(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<FavouriteInput>) -> Result<StatusCode> {
    db.set_favourite(&sc, id, i.favourite).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    put, path = "/api/recipes/{id}/rating", params(("id" = i64, Path, description = "Recipe id")), request_body = RatingInput,
    responses((status = 204, description = "Set, for the caller only; null clears it"), (status = 404, description = "No such recipe", body = ErrorBody), (status = 422, description = "Not 0.5 to 5 in half steps", body = ErrorBody))
)]
pub(crate) async fn put_rating(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<RatingInput>) -> Result<StatusCode> {
    db.set_rating(&sc, id, i.rating).await?;
    Ok(StatusCode::NO_CONTENT)
}
