use axum::extract::{DefaultBodyLimit, Multipart, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::extract::Path;
use axum::{Extension, Json, Router};
use std::sync::Arc;

use std::path::PathBuf;

use axum::extract::FromRef;

use crate::{Label, LabelInput, Scope, Webhook, WebhookInput, EntryInput, ItemInput, MealPlanEntry, ShoppingList, ShoppingListSummary, ImportService, Parsed, Batch, BatchDetail, BatchInput, BatchLineInput, Db, Error, Recipe, RecipeInput, RecipeSummary, Snapshot, SnapshotSummary};
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
    start_batch, list_batches, get_batch, put_batch, add_line, finish_batch, add_photo, parse, import_url,
    get_plan, add_plan_entry, put_plan_entry, delete_plan_entry,
    get_lists, create_list, get_list, list_labels, create_label, put_label, delete_label, put_food_label, put_section_order, add_list_item, put_item, delete_item, list_add_recipe, list_add_batch,
    list_webhooks, create_webhook, get_webhook, put_webhook, delete_webhook, test_webhook,
    get_llm, test_llm, nutrition_estimate,
    crate::backup::download, crate::backup::restore_api,
    crate::login::me, crate::login::list_users, crate::login::disable_user, crate::login::enable_user,
    crate::households::list_groups, crate::households::create_group, crate::households::list_households, crate::households::create_household,
    crate::households::get_default, crate::households::put_default, crate::households::move_user
), modifiers(&BearerScheme), security(("bearer" = [])))]
struct ApiDoc;

/// Declares the `Authorization: Bearer` scheme every /api route accepts.
struct BearerScheme;

impl utoipa::Modify for BearerScheme {
    fn modify(&self, api: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        let scheme = HttpBuilder::new().scheme(HttpAuthScheme::Bearer).description(Some("A per-user API token, created at /settings/tokens")).build();
        api.components.get_or_insert_with(Default::default).add_security_scheme("bearer", SecurityScheme::Http(scheme));
    }
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[utoipa::path(
    post, path = "/api/recipes", request_body = RecipeInput,
    responses(
        (status = 201, description = "Recipe created", body = Recipe),
        (status = 404, description = "A unit or food id does not exist", body = ErrorBody),
        (status = 422, description = "Invalid recipe", body = ErrorBody),
    )
)]
async fn create(sc: Scope, State(db): State<Db>, Json(input): Json<RecipeInput>) -> Result<(StatusCode, Json<Recipe>), Error> {
    Ok((StatusCode::CREATED, Json(db.create_recipe(&sc, input).await?)))
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
async fn list(sc: Scope, State(db): State<Db>, Query(p): Query<ListParams>) -> Result<Json<Vec<RecipeSummary>>, Error> {
    Ok(Json(db.list_recipes(&sc, p.include_archived, p.q.as_deref()).await?))
}

#[utoipa::path(
    get, path = "/api/recipes/{id}",
    params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 200, description = "The current content of the recipe", body = Recipe),
        (status = 404, description = "No such recipe", body = ErrorBody),
    )
)]
async fn get_one(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Recipe>, Error> {
    db.get_recipe(&sc, id).await?.map(Json).ok_or(Error::NotFound("recipe"))
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
async fn update(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(input): Json<RecipeInput>) -> Result<Json<Recipe>, Error> {
    Ok(Json(db.update_recipe(&sc, id, input).await?))
}

#[utoipa::path(
    post, path = "/api/recipes/{id}/archive",
    params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 204, description = "Recipe archived"),
        (status = 404, description = "No such recipe", body = ErrorBody),
    )
)]
async fn archive(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    db.archive_recipe(&sc, id).await?;
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
async fn snapshots(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Vec<SnapshotSummary>>, Error> {
    Ok(Json(db.snapshots(&sc, id).await?))
}

#[utoipa::path(
    get, path = "/api/snapshots/{id}",
    params(("id" = i64, Path, description = "Snapshot id")),
    responses(
        (status = 200, description = "The frozen content of the snapshot", body = Snapshot),
        (status = 404, description = "No such snapshot", body = ErrorBody),
    )
)]
async fn get_snapshot(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Snapshot>, Error> {
    Ok(Json(db.snapshot(&sc, id).await?))
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
async fn start_batch(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(b): Json<StartBatch>) -> Result<(StatusCode, Json<BatchDetail>), Error> {
    Ok((StatusCode::CREATED, Json(db.start_batch(&sc, id, b.scale).await?)))
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/batches",
    params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 200, description = "The recipe's batches, newest first (undated planned ones lead)", body = [Batch]),
        (status = 404, description = "No such recipe", body = ErrorBody),
    )
)]
async fn list_batches(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Vec<Batch>>, Error> {
    db.get_recipe(&sc, id).await?.ok_or(Error::NotFound("recipe"))?;
    Ok(Json(db.list_batches(&sc, id).await?))
}

#[utoipa::path(
    get, path = "/api/batches/{id}",
    params(("id" = i64, Path, description = "Batch id")),
    responses(
        (status = 200, description = "The batch with its lines and photos", body = BatchDetail),
        (status = 404, description = "No such batch", body = ErrorBody),
    )
)]
async fn get_batch(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<BatchDetail>, Error> {
    Ok(Json(db.get_batch(&sc, id).await?))
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
async fn put_batch(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(input): Json<BatchInput>) -> Result<Json<BatchDetail>, Error> {
    Ok(Json(db.update_batch(&sc, id, input).await?))
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
async fn add_line(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(line): Json<BatchLineInput>) -> Result<(StatusCode, Json<BatchDetail>), Error> {
    Ok((StatusCode::CREATED, Json(db.add_batch_line(&sc, id, line).await?)))
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
async fn finish_batch(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<BatchDetail>, Error> {
    Ok(Json(db.finish_batch(&sc, id).await?))
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
async fn add_photo(sc: Scope, State(s): State<AppState>, Path(id): Path<i64>, form: Multipart) -> Result<(StatusCode, Json<BatchDetail>), Error> {
    let bytes = crate::images::read_photo(form).await?;
    s.db.add_batch_photo(&sc, id, &s.images_dir, &bytes).await?;
    Ok((StatusCode::CREATED, Json(s.db.get_batch(&sc, id).await?)))
}

#[derive(Deserialize, ToSchema)]
struct ParseRequest {
    /// Ingredient lines as written, one string each.
    lines: Vec<String>,
}

#[utoipa::path(
    post, path = "/api/parse", request_body = ParseRequest,
    responses((status = 200, description = "One parse result per line, in order, matched against the current units and foods", body = [Parsed]))
)]
async fn parse(sc: Scope, State(db): State<Db>, Json(req): Json<ParseRequest>) -> Result<Json<Vec<Parsed>>, Error> {
    let (units, foods) = (db.units(&sc).await?, db.foods(&sc).await?);
    Ok(Json(req.lines.iter().map(|l| crate::parse_line(l, &units, &foods)).collect()))
}

#[derive(Deserialize, ToSchema)]
struct ImportRequest {
    /// http or https page to import a recipe from.
    url: String,
}

#[utoipa::path(
    post, path = "/api/recipes/import", request_body = ImportRequest,
    responses(
        (status = 201, description = "Recipe created from the page; its ingredient lines still need review", body = Recipe),
        (status = 422, description = "The URL was refused, could not be fetched, or the page has no recipe", body = ErrorBody),
    )
)]
async fn import_url(sc: Scope, State(s): State<AppState>, Extension(svc): Extension<Arc<ImportService>>, Json(req): Json<ImportRequest>) -> Result<(StatusCode, Json<Recipe>), Error> {
    Ok((StatusCode::CREATED, Json(svc.import(&s.db, &sc, &s.images_dir, &req.url).await?)))
}

#[derive(Deserialize, IntoParams)]
struct PlanRange {
    /// First day, YYYY-MM-DD (inclusive).
    from: Option<String>,
    /// Last day, YYYY-MM-DD (inclusive).
    to: Option<String>,
}

#[utoipa::path(
    get, path = "/api/meal-plan", params(PlanRange),
    responses(
        (status = 200, description = "Entries from `from` to `to`, by date then slot", body = [MealPlanEntry]),
        (status = 422, description = "`from` or `to` missing", body = ErrorBody),
    )
)]
async fn get_plan(sc: Scope, State(db): State<Db>, Query(r): Query<PlanRange>) -> Result<Json<Vec<MealPlanEntry>>, Error> {
    let (Some(from), Some(to)) = (r.from, r.to) else { return Err(Error::Validation("from and to are required".into())) };
    Ok(Json(db.meal_plan(&sc, &from, &to).await?))
}

#[utoipa::path(
    post, path = "/api/meal-plan", request_body = EntryInput,
    responses(
        (status = 201, description = "Entry added", body = MealPlanEntry),
        (status = 404, description = "No such recipe or batch", body = ErrorBody),
        (status = 422, description = "Bad date or slot, no recipe or title, or the batch is not a planned batch of the recipe", body = ErrorBody),
    )
)]
async fn add_plan_entry(sc: Scope, State(db): State<Db>, Json(i): Json<EntryInput>) -> Result<(StatusCode, Json<MealPlanEntry>), Error> {
    Ok((StatusCode::CREATED, Json(db.add_entry(&sc, i).await?)))
}

#[utoipa::path(
    put, path = "/api/meal-plan/{id}", params(("id" = i64, Path, description = "Entry id")), request_body = EntryInput,
    responses(
        (status = 200, description = "Entry replaced", body = MealPlanEntry),
        (status = 404, description = "No such entry, recipe or batch", body = ErrorBody),
        (status = 422, description = "Invalid entry", body = ErrorBody),
    )
)]
async fn put_plan_entry(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<EntryInput>) -> Result<Json<MealPlanEntry>, Error> {
    Ok(Json(db.update_entry(&sc, id, i).await?))
}

#[utoipa::path(
    delete, path = "/api/meal-plan/{id}", params(("id" = i64, Path, description = "Entry id")),
    responses((status = 204, description = "Entry deleted"), (status = 404, description = "No such entry", body = ErrorBody))
)]
async fn delete_plan_entry(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    db.delete_entry(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/api/shopping-lists", responses((status = 200, description = "All lists", body = [ShoppingListSummary])))]
async fn get_lists(sc: Scope, State(db): State<Db>) -> Result<Json<Vec<ShoppingListSummary>>, Error> {
    Ok(Json(db.shopping_lists(&sc).await?))
}

#[derive(Deserialize, ToSchema)]
struct NewList {
    name: String,
}

#[utoipa::path(
    post, path = "/api/shopping-lists", request_body = NewList,
    responses((status = 201, description = "List created", body = ShoppingListSummary), (status = 422, description = "Empty name", body = ErrorBody))
)]
async fn create_list(sc: Scope, State(db): State<Db>, Json(l): Json<NewList>) -> Result<(StatusCode, Json<ShoppingListSummary>), Error> {
    Ok((StatusCode::CREATED, Json(db.create_list(&sc, &l.name).await?)))
}

#[utoipa::path(
    get, path = "/api/shopping-lists/{id}", params(("id" = i64, Path, description = "List id")),
    responses((status = 200, description = "The list and its items", body = ShoppingList), (status = 404, description = "No such list", body = ErrorBody))
)]
async fn get_list(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<ShoppingList>, Error> {
    Ok(Json(db.shopping_list(&sc, id).await?))
}

#[utoipa::path(get, path = "/api/labels", responses((status = 200, description = "The group's labels, by name", body = [Label])))]
async fn list_labels(sc: Scope, State(db): State<Db>) -> Result<Json<Vec<Label>>, Error> {
    Ok(Json(db.labels(&sc).await?))
}

#[utoipa::path(
    post, path = "/api/labels", request_body = LabelInput,
    responses((status = 201, description = "Label created", body = Label), (status = 422, description = "Empty or duplicate name, or colour not #rrggbb", body = ErrorBody))
)]
async fn create_label(sc: Scope, State(db): State<Db>, Json(i): Json<LabelInput>) -> Result<(StatusCode, Json<Label>), Error> {
    Ok((StatusCode::CREATED, Json(db.add_label(&sc, i).await?)))
}

#[utoipa::path(
    put, path = "/api/labels/{id}", params(("id" = i64, Path, description = "Label id")), request_body = LabelInput,
    responses((status = 200, description = "Label updated", body = Label), (status = 404, description = "No such label", body = ErrorBody), (status = 422, description = "Invalid", body = ErrorBody))
)]
async fn put_label(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<LabelInput>) -> Result<Json<Label>, Error> {
    Ok(Json(db.update_label(&sc, id, i).await?))
}

#[utoipa::path(
    delete, path = "/api/labels/{id}", params(("id" = i64, Path, description = "Label id")),
    responses((status = 204, description = "Deleted; foods and items lose it"), (status = 404, description = "No such label", body = ErrorBody))
)]
async fn delete_label(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    db.delete_label(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
struct FoodLabel {
    /// `null` clears the food's label.
    label_id: Option<i64>,
}

#[utoipa::path(
    put, path = "/api/foods/{id}/label", params(("id" = i64, Path, description = "Food id")), request_body = FoodLabel,
    responses((status = 204, description = "Label set"), (status = 404, description = "No such food or label", body = ErrorBody))
)]
async fn put_food_label(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(b): Json<FoodLabel>) -> Result<StatusCode, Error> {
    db.set_food_label(&sc, id, b.label_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
struct SectionOrder {
    /// Labels in the order their sections appear; others follow by name.
    label_ids: Vec<i64>,
}

#[utoipa::path(
    put, path = "/api/shopping-lists/{id}/section-order", params(("id" = i64, Path, description = "List id")), request_body = SectionOrder,
    responses((status = 200, description = "The list with its sections reordered", body = ShoppingList), (status = 404, description = "No such list or label", body = ErrorBody), (status = 422, description = "A label listed twice", body = ErrorBody))
)]
async fn put_section_order(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(b): Json<SectionOrder>) -> Result<Json<ShoppingList>, Error> {
    Ok(Json(db.set_section_order(&sc, id, &b.label_ids).await?))
}

#[utoipa::path(
    post, path = "/api/shopping-lists/{id}/items", params(("id" = i64, Path, description = "List id")), request_body = ItemInput,
    responses(
        (status = 201, description = "Item added as given (hand items are not merged)", body = crate::ShoppingItem),
        (status = 404, description = "No such list, unit or food", body = ErrorBody),
        (status = 422, description = "No food or note, or an invalid amount", body = ErrorBody),
    )
)]
async fn add_list_item(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<ItemInput>) -> Result<(StatusCode, Json<crate::ShoppingItem>), Error> {
    Ok((StatusCode::CREATED, Json(db.add_item(&sc, id, i).await?)))
}

#[utoipa::path(
    put, path = "/api/shopping-list-items/{id}", params(("id" = i64, Path, description = "Item id")), request_body = ItemInput,
    responses(
        (status = 200, description = "Item replaced; `checked` ticks it off", body = crate::ShoppingItem),
        (status = 404, description = "No such item, unit or food", body = ErrorBody),
        (status = 422, description = "No food or note, or an invalid amount", body = ErrorBody),
    )
)]
async fn put_item(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<ItemInput>) -> Result<Json<crate::ShoppingItem>, Error> {
    Ok(Json(db.update_item(&sc, id, i).await?))
}

#[utoipa::path(
    delete, path = "/api/shopping-list-items/{id}", params(("id" = i64, Path, description = "Item id")),
    responses((status = 204, description = "Item deleted"), (status = 404, description = "No such item", body = ErrorBody))
)]
async fn delete_item(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    db.delete_item(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
struct ListRecipe {
    recipe_id: i64,
    /// Multiplier on the recipe's amounts; greater than 0.
    scale: f64,
}

#[utoipa::path(
    post, path = "/api/shopping-lists/{id}/recipes", params(("id" = i64, Path, description = "List id")), request_body = ListRecipe,
    responses(
        (status = 200, description = "The list after the recipe's ingredients were added or merged into unchecked items of the same food", body = ShoppingList),
        (status = 404, description = "No such list or recipe", body = ErrorBody),
        (status = 422, description = "Invalid scale", body = ErrorBody),
    )
)]
async fn list_add_recipe(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(r): Json<ListRecipe>) -> Result<Json<ShoppingList>, Error> {
    Ok(Json(db.add_recipe_to_list(&sc, id, r.recipe_id, r.scale).await?))
}

#[derive(Deserialize, ToSchema)]
struct ListBatch {
    batch_id: i64,
}

#[utoipa::path(
    post, path = "/api/shopping-lists/{id}/batches", params(("id" = i64, Path, description = "List id")), request_body = ListBatch,
    responses(
        (status = 200, description = "The list after the batch's actual amounts (substitute foods, added lines) were added or merged", body = ShoppingList),
        (status = 404, description = "No such list or batch", body = ErrorBody),
    )
)]
async fn list_add_batch(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(b): Json<ListBatch>) -> Result<Json<ShoppingList>, Error> {
    Ok(Json(db.add_batch_to_list(&sc, id, b.batch_id).await?))
}

// Admin only (`crate::Admin`); the guard middleware also refuses non-admins under /api/webhooks.
#[utoipa::path(get, path = "/api/webhooks", responses((status = 200, description = "All webhooks", body = [Webhook])))]
async fn list_webhooks(_: crate::Admin, State(db): State<Db>) -> Result<Json<Vec<Webhook>>, Error> {
    Ok(Json(db.list_webhooks().await?))
}

#[utoipa::path(
    post, path = "/api/webhooks", request_body = WebhookInput,
    responses((status = 201, description = "Webhook created", body = Webhook), (status = 422, description = "Invalid webhook", body = ErrorBody))
)]
async fn create_webhook(_: crate::Admin, State(db): State<Db>, Json(i): Json<WebhookInput>) -> Result<(StatusCode, Json<Webhook>), Error> {
    Ok((StatusCode::CREATED, Json(db.create_webhook(i).await?)))
}

#[utoipa::path(
    get, path = "/api/webhooks/{id}", params(("id" = i64, Path, description = "Webhook id")),
    responses((status = 200, description = "The webhook", body = Webhook), (status = 404, description = "No such webhook", body = ErrorBody))
)]
async fn get_webhook(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Webhook>, Error> {
    Ok(Json(db.get_webhook(id).await?))
}

#[utoipa::path(
    put, path = "/api/webhooks/{id}", params(("id" = i64, Path, description = "Webhook id")), request_body = WebhookInput,
    responses(
        (status = 200, description = "Webhook replaced", body = Webhook),
        (status = 404, description = "No such webhook", body = ErrorBody),
        (status = 422, description = "Invalid webhook", body = ErrorBody),
    )
)]
async fn put_webhook(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<WebhookInput>) -> Result<Json<Webhook>, Error> {
    Ok(Json(db.update_webhook(id, i).await?))
}

#[utoipa::path(
    delete, path = "/api/webhooks/{id}", params(("id" = i64, Path, description = "Webhook id")),
    responses((status = 204, description = "Webhook deleted"), (status = 404, description = "No such webhook", body = ErrorBody))
)]
async fn delete_webhook(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode, Error> {
    db.delete_webhook(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What a test delivery got back.
#[derive(Serialize, ToSchema)]
struct TestOutcome {
    /// True when the receiver answered 2xx.
    ok: bool,
    /// The receiver's HTTP status.
    status: u16,
}

#[utoipa::path(
    post, path = "/api/webhooks/{id}/test", params(("id" = i64, Path, description = "Webhook id")),
    responses(
        (status = 200, description = "A test event was sent (to a disabled webhook too); the receiver's status is reported", body = TestOutcome),
        (status = 404, description = "No such webhook", body = ErrorBody),
        (status = 422, description = "The receiver could not be reached", body = ErrorBody),
    )
)]
async fn test_webhook(_: crate::Admin, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<TestOutcome>, Error> {
    let status = db.send_test(id).await?;
    Ok(Json(TestOutcome { ok: (200..300).contains(&status), status }))
}

/// The active LLM, never its key.
#[derive(Serialize, ToSchema)]
struct LlmInfo {
    provider: Option<crate::Provider>,
    model: Option<String>,
    enabled: bool,
}

#[utoipa::path(get, path = "/api/llm", responses((status = 200, description = "The configured provider and model; `enabled` is false when LLM_PROVIDER is unset", body = LlmInfo)))]
async fn get_llm(_: crate::Admin, Extension(svc): Extension<Arc<ImportService>>) -> Json<LlmInfo> {
    let llm = svc.llm();
    Json(LlmInfo { provider: llm.map(|l| l.provider()), model: llm.map(|l| l.model().to_string()), enabled: llm.is_some() })
}

/// What a test prompt got back.
#[derive(Serialize, ToSchema)]
struct LlmOutcome {
    ok: bool,
    /// Why it failed; never contains the key.
    error: Option<String>,
}

#[utoipa::path(
    post, path = "/api/llm/test",
    responses((status = 200, description = "A tiny prompt was sent to the provider; `ok` says whether it answered", body = LlmOutcome))
)]
async fn test_llm(_: crate::Admin, Extension(svc): Extension<Arc<ImportService>>) -> Json<LlmOutcome> {
    let r = match svc.llm() {
        Some(l) => l.test().await,
        None => Err(Error::Validation("no LLM is configured (set LLM_PROVIDER)".into())),
    };
    Json(LlmOutcome { ok: r.is_ok(), error: r.err().map(|e| e.to_string()) })
}

#[utoipa::path(
    post, path = "/api/recipes/{id}/nutrition/estimate", params(("id" = i64, Path, description = "Recipe id")),
    responses(
        (status = 200, description = "The LLM's guess per serving. Nothing is saved; put the numbers in the recipe with PUT", body = crate::NutritionEstimate),
        (status = 404, description = "No such recipe", body = ErrorBody),
        (status = 422, description = "No LLM is configured, or it failed", body = ErrorBody),
    )
)]
async fn nutrition_estimate(sc: Scope, State(db): State<Db>, Extension(svc): Extension<Arc<ImportService>>, Path(id): Path<i64>) -> Result<Json<crate::NutritionEstimate>, Error> {
    Ok(Json(crate::web::estimate_recipe(&db, &sc, &svc, id).await?))
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
    router_with(state, ImportService::default())
}

/// `router` with a chosen import service (config from the environment, or a test fetcher).
pub fn router_with(state: AppState, import: ImportService) -> Router {
    router_with_auth(state, import, crate::AuthConfig::default())
}

/// `router_with` and an explicit auth configuration (`AuthConfig::from_env()` in the binary).
pub fn router_with_auth(state: AppState, import: ImportService, auth: crate::AuthConfig) -> Router {
    router_with_restart(state, import, auth, Arc::new(crate::backup::Restart::new()))
}

/// `router_with_auth`, and the signal a restore uses to ask the process to exit (`main` waits on it).
pub fn router_with_restart(state: AppState, import: ImportService, auth: crate::AuthConfig, restart: Arc<crate::backup::Restart>) -> Router {
    let guard = crate::Guard { db: state.db.clone(), auth: Arc::new(crate::Auth::new(auth)), restart: restart.clone() };
    Router::new()
        .route("/api/recipes/import", post(import_url))
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
        .route("/api/parse", post(parse))
        .route("/api/meal-plan", get(get_plan).post(add_plan_entry))
        .route("/api/meal-plan/{id}", put(put_plan_entry).delete(delete_plan_entry))
        .route("/api/shopping-lists", get(get_lists).post(create_list))
        .route("/api/shopping-lists/{id}", get(get_list))
        .route("/api/shopping-lists/{id}/section-order", put(put_section_order))
        .route("/api/labels", get(list_labels).post(create_label))
        .route("/api/labels/{id}", put(put_label).delete(delete_label))
        .route("/api/foods/{id}/label", put(put_food_label))
        .route("/api/shopping-lists/{id}/items", post(add_list_item))
        .route("/api/shopping-list-items/{id}", put(put_item).delete(delete_item))
        .route("/api/shopping-lists/{id}/recipes", post(list_add_recipe))
        .route("/api/shopping-lists/{id}/batches", post(list_add_batch))
        .route("/api/webhooks", get(list_webhooks).post(create_webhook))
        .route("/api/webhooks/{id}", get(get_webhook).put(put_webhook).delete(delete_webhook))
        .route("/api/webhooks/{id}/test", post(test_webhook))
        .route("/api/llm", get(get_llm))
        .route("/api/llm/test", post(test_llm))
        .route("/api/recipes/{id}/nutrition/estimate", post(nutrition_estimate))
        .route("/api/openapi.json", get(openapi))
        .route("/api/me", get(crate::login::me))
        .route("/api/users", get(crate::login::list_users))
        .route("/api/users/{id}/disable", post(crate::login::disable_user))
        .route("/api/users/{id}/enable", post(crate::login::enable_user))
        .merge(crate::web::routes())
        .merge(crate::login::routes())
        .merge(crate::households::routes())
        .merge(crate::backup::routes())
        .layer(Extension(restart))
        .layer(Extension(Arc::new(import)))
        .layer(Extension(guard.auth.clone()))
        .layer(axum::middleware::from_fn_with_state(guard, crate::guard::guard))
        .layer(axum::middleware::from_fn(crate::errors::pages))
        .with_state(state)
}
