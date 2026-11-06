//! Admin management of groups and households (spec, "Tenancy"): the API and the settings page.
use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Redirect;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::api::{AppState, ErrorBody};
use crate::{Admin, Db, Group, Household, Result, User};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/groups", get(list_groups).post(create_group))
        .route("/api/households", get(list_households).post(create_household))
        .route("/api/default-household", get(get_default).put(put_default))
        .route("/api/users/{id}/household", put(move_user))
        // Admin only: each handler takes `Admin`, and the guard refuses non-admins under these prefixes too.
        .route("/settings/households", get(page))
        .route("/settings/households/groups", post(form_group))
        .route("/settings/households/households", post(form_household))
        .route("/settings/households/default", post(form_default))
        .route("/settings/households/move-user", post(form_move))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct NewGroup {
    name: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct NewHousehold {
    group_id: i64,
    name: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct HouseholdChoice {
    household_id: i64,
}

#[utoipa::path(get, path = "/api/groups", responses((status = 200, description = "Every group, by id", body = [Group]), (status = 403, description = "Not an admin", body = ErrorBody)))]
pub(crate) async fn list_groups(_: Admin, State(db): State<Db>) -> Result<Json<Vec<Group>>> {
    Ok(Json(db.groups().await?))
}

#[utoipa::path(
    post, path = "/api/groups", request_body = NewGroup,
    responses((status = 201, description = "Group created", body = Group), (status = 403, description = "Not an admin", body = ErrorBody), (status = 422, description = "Empty name", body = ErrorBody))
)]
pub(crate) async fn create_group(_: Admin, State(db): State<Db>, Json(g): Json<NewGroup>) -> Result<(StatusCode, Json<Group>)> {
    Ok((StatusCode::CREATED, Json(db.create_group(&g.name).await?)))
}

#[utoipa::path(get, path = "/api/households", responses((status = 200, description = "Every household, by id", body = [Household]), (status = 403, description = "Not an admin", body = ErrorBody)))]
pub(crate) async fn list_households(_: Admin, State(db): State<Db>) -> Result<Json<Vec<Household>>> {
    Ok(Json(db.households().await?))
}

#[utoipa::path(
    post, path = "/api/households", request_body = NewHousehold,
    responses(
        (status = 201, description = "Household created in the group", body = Household),
        (status = 403, description = "Not an admin", body = ErrorBody),
        (status = 404, description = "No such group", body = ErrorBody),
        (status = 422, description = "Empty name", body = ErrorBody),
    )
)]
pub(crate) async fn create_household(_: Admin, State(db): State<Db>, Json(h): Json<NewHousehold>) -> Result<(StatusCode, Json<Household>)> {
    Ok((StatusCode::CREATED, Json(db.create_household(h.group_id, &h.name).await?)))
}

#[utoipa::path(get, path = "/api/default-household", responses((status = 200, description = "The household new users join; \"Home\" in \"Home\" is made if none was ever chosen", body = Household), (status = 403, description = "Not an admin", body = ErrorBody)))]
pub(crate) async fn get_default(_: Admin, State(db): State<Db>) -> Result<Json<Household>> {
    Ok(Json(db.default_household().await?))
}

#[utoipa::path(
    put, path = "/api/default-household", request_body = HouseholdChoice,
    responses((status = 200, description = "Chosen; people who already have an account stay where they are", body = Household), (status = 403, description = "Not an admin", body = ErrorBody), (status = 404, description = "No such household", body = ErrorBody))
)]
pub(crate) async fn put_default(_: Admin, State(db): State<Db>, Json(c): Json<HouseholdChoice>) -> Result<Json<Household>> {
    Ok(Json(db.set_default_household(c.household_id).await?))
}

#[utoipa::path(
    put, path = "/api/users/{id}/household", params(("id" = i64, Path, description = "User id")), request_body = HouseholdChoice,
    responses(
        (status = 200, description = "Moved, into any group; the household-scoped data they leave stays with their old household", body = User),
        (status = 403, description = "Not an admin", body = ErrorBody),
        (status = 404, description = "No such user or household", body = ErrorBody),
    )
)]
pub(crate) async fn move_user(_: Admin, State(db): State<Db>, Path(id): Path<i64>, Json(c): Json<HouseholdChoice>) -> Result<Json<User>> {
    Ok(Json(db.move_user(id, c.household_id).await?))
}

struct HouseholdRow {
    name: String,
    is_default: bool,
}

struct GroupRow {
    id: i64,
    name: String,
    slug: String,
    households: Vec<HouseholdRow>,
}

struct HouseholdOption {
    id: i64,
    label: String,
    selected: bool,
}

struct UserRow {
    id: i64,
    username: String,
    household: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "households.html")]
struct Page {
    groups: Vec<GroupRow>,
    households: Vec<HouseholdOption>,
    users: Vec<UserRow>,
}

async fn page(_: Admin, State(db): State<Db>) -> Result<Page> {
    let (groups, households, users) = (db.groups().await?, db.households().await?, db.users().await?);
    let default = db.default_household().await?.id;
    let label = |h: &Household| format!("{} / {}", groups.iter().find(|g| g.id == h.group_id).map_or("?", |g| g.name.as_str()), h.name);
    Ok(Page {
        groups: groups
            .iter()
            .map(|g| GroupRow {
                id: g.id,
                name: g.name.clone(),
                slug: g.slug.clone(),
                households: households.iter().filter(|h| h.group_id == g.id).map(|h| HouseholdRow { name: h.name.clone(), is_default: h.id == default }).collect(),
            })
            .collect(),
        households: households.iter().map(|h| HouseholdOption { id: h.id, label: label(h), selected: h.id == default }).collect(),
        users: users.iter().map(|u| UserRow { id: u.id, username: u.username.clone(), household: households.iter().find(|h| h.id == u.household_id).map(label).unwrap_or_default() }).collect(),
    })
}

async fn form_group(_: Admin, State(db): State<Db>, axum::Form(f): axum::Form<NewGroup>) -> Result<Redirect> {
    db.create_group(&f.name).await?;
    Ok(Redirect::to("/settings/households"))
}

async fn form_household(_: Admin, State(db): State<Db>, axum::Form(f): axum::Form<NewHousehold>) -> Result<Redirect> {
    db.create_household(f.group_id, &f.name).await?;
    Ok(Redirect::to("/settings/households"))
}

async fn form_default(_: Admin, State(db): State<Db>, axum::Form(f): axum::Form<HouseholdChoice>) -> Result<Redirect> {
    db.set_default_household(f.household_id).await?;
    Ok(Redirect::to("/settings/households"))
}

#[derive(Deserialize)]
struct MoveForm {
    user_id: i64,
    household_id: i64,
}

async fn form_move(_: Admin, State(db): State<Db>, axum::Form(f): axum::Form<MoveForm>) -> Result<Redirect> {
    db.move_user(f.user_id, f.household_id).await?;
    Ok(Redirect::to("/settings/households"))
}
