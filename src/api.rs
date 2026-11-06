use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::extract::Path;
use axum::{Json, Router};

use crate::{Db, Error, Recipe, RecipeInput, RecipeSummary, Snapshot, SnapshotSummary};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, OpenApi, ToSchema};

/// The `{"error": "..."}` body every failing response carries.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    error: String,
}

#[derive(OpenApi)]
#[openapi(paths(create, list, get_one, update, archive, snapshots, get_snapshot))]
struct ApiDoc;

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

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

#[utoipa::path(
    post, path = "/api/recipes", request_body = RecipeInput,
    responses(
        (status = 201, description = "Recipe created", body = Recipe),
        (status = 404, description = "A unit or food id does not exist", body = ErrorBody),
        (status = 422, description = "Invalid recipe", body = ErrorBody),
    )
)]
async fn create(State(db): State<Db>, Json(input): Json<RecipeInput>) -> Result<(StatusCode, Json<Recipe>), Error> {
    Ok((StatusCode::CREATED, Json(db.create_recipe(input).await?)))
}

#[derive(Deserialize, IntoParams)]
struct ListParams {
    /// Include archived recipes. Defaults to false.
    #[serde(default)]
    include_archived: bool,
    /// Case-insensitive substring match on name or description.
    q: Option<String>,
}

#[utoipa::path(
    get, path = "/api/recipes", params(ListParams),
    responses((status = 200, description = "Recipe summaries, sorted by name", body = [RecipeSummary]))
)]
async fn list(State(db): State<Db>, Query(p): Query<ListParams>) -> Result<Json<Vec<RecipeSummary>>, Error> {
    Ok(Json(db.list_recipes(p.include_archived, p.q.as_deref()).await?))
}

#[utoipa::path(
    get, path = "/api/recipes/{id}",
    params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 200, description = "The current content of the recipe", body = Recipe),
        (status = 404, description = "No such recipe", body = ErrorBody),
    )
)]
async fn get_one(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Recipe>, Error> {
    db.get_recipe(id).await?.map(Json).ok_or(Error::NotFound("recipe"))
}

#[utoipa::path(
    put, path = "/api/recipes/{id}",
    params(("id" = i64, Path, description = "Recipe id")),
    request_body = RecipeInput,
    responses(
        (status = 200, description = "Recipe updated; a new snapshot was written", body = Recipe),
        (status = 404, description = "No such recipe, or a unit or food id does not exist", body = ErrorBody),
        (status = 422, description = "Invalid recipe", body = ErrorBody),
    )
)]
async fn update(State(db): State<Db>, Path(id): Path<i64>, Json(input): Json<RecipeInput>) -> Result<Json<Recipe>, Error> {
    Ok(Json(db.update_recipe(id, input).await?))
}

#[utoipa::path(
    post, path = "/api/recipes/{id}/archive",
    params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 204, description = "Recipe archived"),
        (status = 404, description = "No such recipe", body = ErrorBody),
    )
)]
async fn archive(State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    db.archive_recipe(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/snapshots",
    params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 200, description = "Snapshots of the recipe, newest first", body = [SnapshotSummary]),
        (status = 404, description = "No such recipe", body = ErrorBody),
    )
)]
async fn snapshots(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Vec<SnapshotSummary>>, Error> {
    Ok(Json(db.snapshots(id).await?))
}

#[utoipa::path(
    get, path = "/api/snapshots/{id}",
    params(("id" = i64, Path, description = "Snapshot id")),
    responses(
        (status = 200, description = "The frozen content of the snapshot", body = Snapshot),
        (status = 404, description = "No such snapshot", body = ErrorBody),
    )
)]
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
        .route("/api/openapi.json", get(openapi))
        .with_state(db)
}
