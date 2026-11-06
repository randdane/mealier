//! Step 14, part B: recipe actions. Household-scoped. A `link` action opens a URL built from the
//! recipe; a `post` action makes the server send the recipe as JSON to a URL, so only an admin may
//! write one (webhook delivery rules: internal hosts are allowed).
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use sqlx::Row;

use crate::api::{AppState, ErrorBody};
use crate::{Db, Error, Result, Scope};

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct RecipeAction {
    pub id: i64,
    pub title: String,
    /// `link` or `post`.
    pub kind: String,
    /// For a link, a template: `${id}`, `${slug}`, `${url}` (Mealie's merge fields) are replaced, URL-encoded.
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ActionInput {
    pub title: String,
    pub kind: String,
    pub url: String,
}

/// A household's action for one recipe, ready to show: a link carries its finished address.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ActionLink {
    pub id: i64,
    pub title: String,
    pub kind: String,
    /// Links only, and only when the result is an http(s) address.
    pub href: Option<String>,
}

/// Everything but letters, digits and `-._~` as %XX.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `template` with Mealie's merge fields replaced by the recipe's values, percent-encoded. `None`
/// unless the result is an http or https URL.
pub fn render_action_url(template: &str, recipe_id: i64, recipe_name: &str, recipe_url: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        match &after[..end] {
            "id" => out.push_str(&recipe_id.to_string()),
            "slug" => out.push_str(&encode(&crate::tenancy::slugify(recipe_name, "recipe"))),
            "url" => out.push_str(&encode(recipe_url)),
            // Mealie's, but a mealier recipe has no yield
            "servings" | "yieldQuantity" | "yieldText" => {}
            other => out.push_str(&format!("${{{other}}}")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    url::Url::parse(&out).ok().filter(|u| matches!(u.scheme(), "http" | "https") && u.has_host()).map(String::from)
}

fn http_url(u: &str) -> bool {
    url::Url::parse(u).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
}

/// Trimmed title, a known kind, an http(s) address; a post action only from an admin.
fn check(sc: &Scope, i: &ActionInput) -> Result<ActionInput> {
    let title = i.title.trim();
    if title.is_empty() {
        return Err(Error::Validation("an action needs a title".into()));
    }
    let url = i.url.trim();
    match i.kind.as_str() {
        "link" if render_action_url(url, 1, "x", "https://example.test/recipes/1").is_none() => return Err(Error::Validation("a link must come out as an http or https URL".into())),
        "post" if !http_url(url) => return Err(Error::Validation("a post action needs an http or https URL".into())),
        "link" | "post" => {}
        _ => return Err(Error::Validation("kind must be link or post".into())),
    }
    if i.kind == "post" && !sc.admin {
        return Err(Error::Forbidden("only an admin can set up an action that posts".into()));
    }
    Ok(ActionInput { title: title.into(), kind: i.kind.clone(), url: url.into() })
}

fn row(r: &sqlx::sqlite::SqliteRow) -> RecipeAction {
    RecipeAction { id: r.get(0), title: r.get(1), kind: r.get(2), url: r.get(3) }
}

impl Db {
    pub async fn create_action(&self, sc: &Scope, input: ActionInput) -> Result<RecipeAction> {
        let i = check(sc, &input)?;
        let id = sqlx::query("INSERT INTO recipe_actions (household_id, title, kind, url) VALUES (?, ?, ?, ?)").bind(sc.household_id).bind(&i.title).bind(&i.kind).bind(&i.url).execute(&self.pool).await?.last_insert_rowid();
        self.action(sc, id).await
    }

    pub async fn actions(&self, sc: &Scope) -> Result<Vec<RecipeAction>> {
        let rows = sqlx::query("SELECT id, title, kind, url FROM recipe_actions WHERE household_id = ? ORDER BY title, id").bind(sc.household_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row).collect())
    }

    async fn action(&self, sc: &Scope, id: i64) -> Result<RecipeAction> {
        let r = sqlx::query("SELECT id, title, kind, url FROM recipe_actions WHERE id = ? AND household_id = ?").bind(id).bind(sc.household_id).fetch_optional(&self.pool).await?;
        r.as_ref().map(row).ok_or(Error::NotFound("action"))
    }

    /// A member may change a link action; changing a post action, or making one, takes an admin.
    pub async fn update_action(&self, sc: &Scope, id: i64, input: ActionInput) -> Result<RecipeAction> {
        let old = self.action(sc, id).await?;
        if old.kind == "post" && !sc.admin {
            return Err(Error::Forbidden("only an admin can change an action that posts".into()));
        }
        let i = check(sc, &input)?;
        sqlx::query("UPDATE recipe_actions SET title = ?, kind = ?, url = ? WHERE id = ? AND household_id = ?").bind(&i.title).bind(&i.kind).bind(&i.url).bind(id).bind(sc.household_id).execute(&self.pool).await?;
        self.action(sc, id).await
    }

    /// Removing one sends nothing, so any member of the household may.
    pub async fn delete_action(&self, sc: &Scope, id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM recipe_actions WHERE id = ? AND household_id = ?").bind(id).bind(sc.household_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("action"));
        }
        Ok(())
    }

    pub async fn recipe_action_links(&self, sc: &Scope, recipe_id: i64) -> Result<Vec<ActionLink>> {
        let r = self.get_recipe(sc, recipe_id).await?.ok_or(Error::NotFound("recipe"))?;
        let url = self.notifier.link(&format!("/recipes/{recipe_id}"));
        Ok(self
            .actions(sc)
            .await?
            .into_iter()
            .map(|a| {
                let href = if a.kind == "link" { render_action_url(&a.url, recipe_id, &r.content.name, &url) } else { None };
                ActionLink { id: a.id, title: a.title, kind: a.kind, href }
            })
            .collect())
    }

    /// Sends the recipe as JSON to a post action's URL; the receiver's status.
    pub async fn trigger_action(&self, sc: &Scope, action_id: i64, recipe_id: i64) -> Result<u16> {
        let a = self.action(sc, action_id).await?;
        let r = self.get_recipe(sc, recipe_id).await?.ok_or(Error::NotFound("recipe"))?;
        if a.kind != "post" {
            return Err(Error::Validation("only a post action can be triggered".into()));
        }
        match self.notifier.post_json(&a.url, serde_json::to_string(&r)?).await {
            Ok(s) if (200..300).contains(&s) => Ok(s),
            Ok(s) => Err(Error::Validation(format!("{}: the receiver answered {s}", a.title))),
            Err(e) => Err(Error::Validation(format!("{}: delivery failed: {e}", a.title))),
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/recipe-actions", get(list_actions).post(create_action_route))
        .route("/api/recipe-actions/{id}", axum::routing::put(put_action).delete(delete_action_route))
        .route("/api/recipe-actions/{id}/trigger", post(trigger))
        .route("/api/recipes/{id}/actions", get(recipe_actions))
}

#[utoipa::path(get, path = "/api/recipe-actions", responses((status = 200, description = "The caller's household's actions, by title", body = [RecipeAction])))]
pub(crate) async fn list_actions(sc: Scope, State(db): State<Db>) -> Result<Json<Vec<RecipeAction>>> {
    Ok(Json(db.actions(&sc).await?))
}

#[utoipa::path(
    post, path = "/api/recipe-actions", request_body = ActionInput,
    responses(
        (status = 201, description = "Created for the caller's household", body = RecipeAction),
        (status = 403, description = "A `post` action, and the caller is not an admin", body = ErrorBody),
        (status = 422, description = "No title, an unknown kind, or a URL that is not http(s)", body = ErrorBody),
    )
)]
pub(crate) async fn create_action_route(sc: Scope, State(db): State<Db>, Json(i): Json<ActionInput>) -> Result<(StatusCode, Json<RecipeAction>)> {
    Ok((StatusCode::CREATED, Json(db.create_action(&sc, i).await?)))
}

#[utoipa::path(
    put, path = "/api/recipe-actions/{id}", params(("id" = i64, Path, description = "Action id")), request_body = ActionInput,
    responses(
        (status = 200, description = "Replaced", body = RecipeAction),
        (status = 403, description = "The action posts, or would, and the caller is not an admin", body = ErrorBody),
        (status = 404, description = "Not one of the caller's household's actions", body = ErrorBody),
        (status = 422, description = "No title, an unknown kind, or a URL that is not http(s)", body = ErrorBody),
    )
)]
pub(crate) async fn put_action(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<ActionInput>) -> Result<Json<RecipeAction>> {
    Ok(Json(db.update_action(&sc, id, i).await?))
}

#[utoipa::path(
    delete, path = "/api/recipe-actions/{id}", params(("id" = i64, Path, description = "Action id")),
    responses((status = 204, description = "Removed"), (status = 404, description = "Not one of the caller's household's actions", body = ErrorBody))
)]
pub(crate) async fn delete_action_route(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode> {
    db.delete_action(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/recipes/{id}/actions", params(("id" = i64, Path, description = "Recipe id")),
    responses((status = 200, description = "The caller's household's actions for this recipe; a link carries its finished `href`", body = [ActionLink]), (status = 404, description = "No such recipe", body = ErrorBody))
)]
pub(crate) async fn recipe_actions(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Vec<ActionLink>>> {
    Ok(Json(db.recipe_action_links(&sc, id).await?))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct TriggerInput {
    recipe_id: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct Triggered {
    /// The receiver's HTTP status.
    status: u16,
}

#[utoipa::path(
    post, path = "/api/recipe-actions/{id}/trigger", params(("id" = i64, Path, description = "Action id")), request_body = TriggerInput,
    responses(
        (status = 200, description = "The recipe was sent as JSON to the action's URL and the receiver answered 2xx", body = Triggered),
        (status = 404, description = "No such action in the caller's household, or no such recipe", body = ErrorBody),
        (status = 422, description = "Not a post action, or the delivery failed or was not answered 2xx", body = ErrorBody),
    )
)]
pub(crate) async fn trigger(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<TriggerInput>) -> Result<Json<Triggered>> {
    Ok(Json(Triggered { status: db.trigger_action(&sc, id, i.recipe_id).await? }))
}
