use axum::extract::{DefaultBodyLimit, Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::extract::Path;
use axum::{Json, Router};

use std::path::PathBuf;

use axum::extract::FromRef;

use crate::{Batch, BatchDetail, BatchInput, BatchLineInput, Db, Error, Recipe, RecipeInput, RecipeSummary, Snapshot, SnapshotSummary};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, OpenApi, ToSchema};

/// The `{"error": "..."}` body every failing response carries.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    error: String,
}

#[derive(OpenApi)]
#[openapi(paths(
    create, list, get_one, update, archive, snapshots, get_snapshot,
    start_batch, list_batches, get_batch, put_batch, add_line, finish_batch, add_photo
))]
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

#[derive(Deserialize, ToSchema)]
struct StartBatch {
    /// Multiplier on the recipe's amounts; greater than 0.
    scale: f64,
}

#[utoipa::path(
    post, path = "/api/recipes/{id}/batches",
    params(("id" = i64, Path, description = "Recipe id")),
    request_body = StartBatch,
    responses(
        (status = 201, description = "A planned batch on the recipe's current snapshot, actuals pre-filled with the scaled amounts", body = BatchDetail),
        (status = 404, description = "No such recipe", body = ErrorBody),
        (status = 422, description = "Invalid scale", body = ErrorBody),
    )
)]
async fn start_batch(State(db): State<Db>, Path(id): Path<i64>, Json(b): Json<StartBatch>) -> Result<(StatusCode, Json<BatchDetail>), Error> {
    Ok((StatusCode::CREATED, Json(db.start_batch(id, b.scale).await?)))
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/batches",
    params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 200, description = "The recipe's batches, newest first (undated planned ones lead)", body = [Batch]),
        (status = 404, description = "No such recipe", body = ErrorBody),
    )
)]
async fn list_batches(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Vec<Batch>>, Error> {
    db.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))?;
    Ok(Json(db.list_batches(id).await?))
}

#[utoipa::path(
    get, path = "/api/batches/{id}",
    params(("id" = i64, Path, description = "Batch id")),
    responses(
        (status = 200, description = "The batch with its lines and photos", body = BatchDetail),
        (status = 404, description = "No such batch", body = ErrorBody),
    )
)]
async fn get_batch(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<BatchDetail>, Error> {
    Ok(Json(db.get_batch(id).await?))
}

#[utoipa::path(
    put, path = "/api/batches/{id}",
    params(("id" = i64, Path, description = "Batch id")),
    request_body = BatchInput,
    responses(
        (status = 200, description = "Saved. Added lines left out of `lines` are removed; every recipe line must be present", body = BatchDetail),
        (status = 404, description = "No such batch, line, unit or food", body = ErrorBody),
        (status = 422, description = "Invalid rating, scale, date, quantity, or a recipe line was left out", body = ErrorBody),
    )
)]
async fn put_batch(State(db): State<Db>, Path(id): Path<i64>, Json(input): Json<BatchInput>) -> Result<Json<BatchDetail>, Error> {
    Ok(Json(db.update_batch(id, input).await?))
}

#[utoipa::path(
    post, path = "/api/batches/{id}/lines",
    params(("id" = i64, Path, description = "Batch id")),
    request_body = BatchLineInput,
    responses(
        (status = 201, description = "The batch with the new `added` line appended", body = BatchDetail),
        (status = 404, description = "No such batch, unit or food", body = ErrorBody),
        (status = 422, description = "Invalid quantity", body = ErrorBody),
    )
)]
async fn add_line(State(db): State<Db>, Path(id): Path<i64>, Json(line): Json<BatchLineInput>) -> Result<(StatusCode, Json<BatchDetail>), Error> {
    Ok((StatusCode::CREATED, Json(db.add_batch_line(id, line).await?)))
}

#[utoipa::path(
    post, path = "/api/batches/{id}/finish",
    params(("id" = i64, Path, description = "Batch id")),
    responses(
        (status = 200, description = "Marked done; `made_on` defaults to today", body = BatchDetail),
        (status = 404, description = "No such batch", body = ErrorBody),
        (status = 422, description = "Already done", body = ErrorBody),
    )
)]
async fn finish_batch(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<BatchDetail>, Error> {
    Ok(Json(db.finish_batch(id).await?))
}

#[utoipa::path(
    post, path = "/api/batches/{id}/photos",
    params(("id" = i64, Path, description = "Batch id")),
    request_body(content_type = "multipart/form-data", content = String, description = "One image in the form field `photo`: png, jpeg or webp, at most 10 MB"),
    responses(
        (status = 201, description = "The batch with the photo attached", body = BatchDetail),
        (status = 404, description = "No such batch", body = ErrorBody),
        (status = 422, description = "Not an accepted image, over 10 MB, field missing, or the batch already has 5 photos", body = ErrorBody),
    )
)]
async fn add_photo(State(s): State<AppState>, Path(id): Path<i64>, form: Multipart) -> Result<(StatusCode, Json<BatchDetail>), Error> {
    let bytes = read_photo(form).await?;
    s.db.add_batch_photo(id, &s.images_dir, &bytes).await?;
    Ok((StatusCode::CREATED, Json(s.db.get_batch(id).await?)))
}

/// The bytes of the multipart field `photo`. Shared with the HTML upload form.
pub(crate) async fn read_photo(mut form: Multipart) -> Result<axum::body::Bytes, Error> {
    let bad = |e: axum::extract::multipart::MultipartError| Error::Validation(format!("bad upload: {e}"));
    while let Some(field) = form.next_field().await.map_err(bad)? {
        if field.name() == Some("photo") {
            return field.bytes().await.map_err(bad);
        }
    }
    Err(Error::Validation("form field `photo` is missing".into()))
}

/// What every handler can reach: the database and the content-addressed images dir.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub images_dir: PathBuf,
}

impl FromRef<AppState> for Db {
    fn from_ref(s: &AppState) -> Db {
        s.db.clone()
    }
}

pub(crate) const PHOTO_BODY_LIMIT: usize = 11 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/recipes", post(create).get(list))
        .route("/api/recipes/{id}", get(get_one).put(update))
        .route("/api/recipes/{id}/archive", post(archive))
        .route("/api/recipes/{id}/snapshots", get(snapshots))
        .route("/api/snapshots/{id}", get(get_snapshot))
        .route("/api/recipes/{id}/batches", post(start_batch).get(list_batches))
        .route("/api/batches/{id}", get(get_batch).put(put_batch))
        .route("/api/batches/{id}/lines", post(add_line))
        .route("/api/batches/{id}/finish", post(finish_batch))
        // Axum's default body limit is 2 MB; the store enforces the real 10 MB (+ multipart framing).
        .route("/api/batches/{id}/photos", post(add_photo).layer(DefaultBodyLimit::max(PHOTO_BODY_LIMIT)))
        .route("/api/openapi.json", get(openapi))
        .merge(crate::web::routes())
        .with_state(state)
}
