use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::extract::Path;
use axum::{Json, Router};

use crate::{Db, Error, Recipe, RecipeInput, RecipeSummary, Snapshot, SnapshotSummary};
use serde::Deserialize;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            Error::Validation(_) => (StatusCode::UNPROCESSABLE_ENTITY, self.to_string()),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            other => {
                eprintln!("mealier: {other}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

async fn create(State(db): State<Db>, Json(input): Json<RecipeInput>) -> Result<(StatusCode, Json<Recipe>), Error> {
    Ok((StatusCode::CREATED, Json(db.create_recipe(input).await?)))
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    include_archived: bool,
}

async fn list(State(db): State<Db>, Query(p): Query<ListParams>) -> Result<Json<Vec<RecipeSummary>>, Error> {
    Ok(Json(db.list_recipes(p.include_archived).await?))
}

async fn get_one(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Recipe>, Error> {
    db.get_recipe(id).await?.map(Json).ok_or(Error::NotFound("recipe"))
}

async fn update(State(db): State<Db>, Path(id): Path<i64>, Json(input): Json<RecipeInput>) -> Result<Json<Recipe>, Error> {
    Ok(Json(db.update_recipe(id, input).await?))
}

async fn archive(State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    db.archive_recipe(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn snapshots(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Vec<SnapshotSummary>>, Error> {
    Ok(Json(db.snapshots(id).await?))
}

async fn get_snapshot(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Snapshot>, Error> {
    Ok(Json(db.snapshot(id).await?))
}

pub fn router(db: Db) -> Router {
    Router::new()
        .route("/api/recipes", post(create).get(list))
        .route("/api/recipes/{id}", get(get_one).put(update))
        .route("/api/recipes/{id}/archive", post(archive))
        .route("/api/recipes/{id}/snapshots", get(snapshots))
        .route("/api/snapshots/{id}", get(get_snapshot))
        .with_state(db)
}
