//! Cookbooks: group-scoped saved filters over the group's recipes. Membership is computed, never stored.
//! `query_filter` is Mealie's filter string, kept as written; `filter_json` is its parsed form.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sqlx::Row;

use crate::filter::{self, By, Filter, Sel, SetField, SetOp};
use crate::tenancy::{free_slug, slugify};
use crate::api::{AppState, ErrorBody};
use crate::{Db, Error, RecipeSummary, Result, Scope};

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Cookbook {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub slug: String,
    pub position: i64,
    pub public: bool,
    /// Mealie's filter string, as written. Empty means every recipe.
    pub query_filter: String,
    /// False when `query_filter` is set but mealier cannot read it (an imported one): the cookbook then lists nothing.
    pub filter_supported: bool,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(default)]
pub struct CookbookInput {
    pub name: String,
    pub description: String,
    /// Left out: last on create, unchanged on update.
    pub position: Option<i64>,
    pub public: bool,
    pub query_filter: String,
}

const COLS: &str = "id, name, description, slug, position, public, query_filter, filter_json";

fn cookbook_row(r: &sqlx::sqlite::SqliteRow) -> Cookbook {
    let (query_filter, json): (String, Option<String>) = (r.get(6), r.get(7));
    Cookbook {
        id: r.get(0),
        name: r.get(1),
        description: r.get(2),
        slug: r.get(3),
        position: r.get(4),
        public: r.get(5),
        filter_supported: query_filter.trim().is_empty() || json.is_some(),
        query_filter,
    }
}

/// A filter a user wrote: trimmed, parsed, and made of ids mealier knows. Returns the string to keep and its parsed form.
pub(crate) fn read_filter(src: &str) -> Result<(String, Option<String>)> {
    let src = src.trim();
    let parsed = filter::parse(src)?;
    if let Some(f) = &parsed {
        f.check_native()?;
    }
    Ok((src.to_string(), parsed.as_ref().map(serde_json::to_string).transpose()?))
}

/// Adds the name to the group's cookbooks if it is new (a recipe may name one). Filterless, so it lists every recipe.
pub(crate) async fn ensure_cookbook(tx: &mut sqlx::SqliteConnection, group_id: i64, name: &str) -> Result<()> {
    let known = sqlx::query("SELECT 1 FROM cookbooks WHERE group_id = ? AND name = ?").bind(group_id).bind(name).fetch_optional(&mut *tx).await?;
    if known.is_none() {
        let slug = free_slug(tx, "cookbooks", Some(group_id), &slugify(name, "cookbook")).await?;
        let position: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(position), 0) + 1 FROM cookbooks WHERE group_id = ?").bind(group_id).fetch_one(&mut *tx).await?;
        sqlx::query("INSERT INTO cookbooks (group_id, name, slug, position) VALUES (?, ?, ?, ?)").bind(group_id).bind(name).bind(slug).bind(position).execute(&mut *tx).await?;
    }
    Ok(())
}

impl Db {
    /// By position, then name.
    pub async fn cookbooks(&self, sc: &Scope) -> Result<Vec<Cookbook>> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM cookbooks WHERE group_id = ? ORDER BY position, name")).bind(sc.group_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(cookbook_row).collect())
    }

    pub async fn cookbook(&self, sc: &Scope, id: i64) -> Result<Cookbook> {
        let r = sqlx::query(&format!("SELECT {COLS} FROM cookbooks WHERE id = ? AND group_id = ?")).bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?;
        r.as_ref().map(cookbook_row).ok_or(Error::NotFound("cookbook"))
    }

    pub async fn add_cookbook(&self, sc: &Scope, input: CookbookInput) -> Result<Cookbook> {
        let name = clean_name(&input.name)?;
        let (query_filter, json) = read_filter(&input.query_filter)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        name_free(&mut tx, sc, &name, 0).await?;
        let slug = free_slug(&mut tx, "cookbooks", Some(sc.group_id), &slugify(&name, "cookbook")).await?;
        let position = match input.position {
            Some(p) => p,
            None => sqlx::query_scalar("SELECT COALESCE(MAX(position), 0) + 1 FROM cookbooks WHERE group_id = ?").bind(sc.group_id).fetch_one(&mut *tx).await?,
        };
        let id = sqlx::query("INSERT INTO cookbooks (group_id, name, description, slug, position, public, query_filter, filter_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(sc.group_id)
            .bind(&name)
            .bind(&input.description)
            .bind(&slug)
            .bind(position)
            .bind(input.public)
            .bind(&query_filter)
            .bind(&json)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        tx.commit().await?;
        let book = self.cookbook(sc, id).await?;
        self.changed("cookbook_created", sc, id, book.name.clone(), serde_json::json!({}));
        Ok(book)
    }

    pub async fn update_cookbook(&self, sc: &Scope, id: i64, input: CookbookInput) -> Result<Cookbook> {
        let old = self.cookbook(sc, id).await?;
        let name = clean_name(&input.name)?;
        let (query_filter, json) = read_filter(&input.query_filter)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        name_free(&mut tx, sc, &name, id).await?;
        let slug = if name == old.name { old.slug } else { free_slug(&mut tx, "cookbooks", Some(sc.group_id), &slugify(&name, "cookbook")).await? };
        sqlx::query("UPDATE cookbooks SET name = ?, description = ?, slug = ?, position = ?, public = ?, query_filter = ?, filter_json = ? WHERE id = ? AND group_id = ?")
            .bind(&name)
            .bind(&input.description)
            .bind(&slug)
            .bind(input.position.unwrap_or(old.position))
            .bind(input.public)
            .bind(&query_filter)
            .bind(&json)
            .bind(id)
            .bind(sc.group_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let book = self.cookbook(sc, id).await?;
        self.changed("cookbook_updated", sc, id, book.name.clone(), serde_json::json!({}));
        Ok(book)
    }

    pub async fn delete_cookbook(&self, sc: &Scope, id: i64) -> Result<()> {
        let name = self.cookbook(sc, id).await?.name;
        sqlx::query("DELETE FROM cookbooks WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).execute(&self.pool).await?;
        self.changed("cookbook_deleted", sc, id, name, serde_json::json!({}));
        Ok(())
    }

    /// The group's live recipes the cookbook's filter selects, by name.
    pub async fn cookbook_recipes(&self, sc: &Scope, id: i64) -> Result<Vec<RecipeSummary>> {
        let r = sqlx::query("SELECT query_filter, filter_json FROM cookbooks WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?.ok_or(Error::NotFound("cookbook"))?;
        let sel = Sel::stored(r.get(0), r.get::<Option<String>, _>(1).as_deref())?;
        self.select_recipes(sc, &[sel]).await
    }
}

fn clean_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(Error::Validation("a cookbook needs a name".into()));
    }
    Ok(n.into())
}

async fn name_free(tx: &mut sqlx::SqliteConnection, sc: &Scope, name: &str, except: i64) -> Result<()> {
    if sqlx::query("SELECT 1 FROM cookbooks WHERE group_id = ? AND name = ? AND id != ?").bind(sc.group_id).bind(name).bind(except).fetch_optional(&mut *tx).await?.is_some() {
        return Err(Error::Validation(format!("a cookbook called {name:?} exists")));
    }
    Ok(())
}

/// One row of the cookbook builder: tags, categories or tools, matched by any or all of the ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub field: SetField,
    pub all: bool,
    pub ids: Vec<i64>,
}

/// A vocabulary entry, for the builder's checkboxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub id: i64,
    pub name: String,
}

/// The filter string the builder writes, in Mealie's syntax: picks with no ids are left out.
pub fn builder_string(picks: &[Pick]) -> String {
    let parts: Vec<String> = picks
        .iter()
        .filter(|p| !p.ids.is_empty())
        .map(|p| {
            let field = match p.field {
                SetField::Tags => "tags.id",
                SetField::Categories => "recipe_category.id",
                SetField::Tools => "tools.id",
            };
            let ids: Vec<String> = p.ids.iter().map(|i| format!("\"{i}\"")).collect();
            format!("{field} {} [{}]", if p.all { "CONTAINS ALL" } else { "IN" }, ids.join(", "))
        })
        .collect();
    parts.join(" AND ")
}

/// The picks a filter string is made of, when it is exactly what the builder writes (an AND of id lists); empty otherwise.
pub fn picks_of(query_filter: &str) -> Vec<Pick> {
    let Ok(Some(f)) = filter::parse(query_filter) else { return vec![] };
    let nodes = match f {
        Filter::And(parts) => parts,
        one => vec![one],
    };
    let mut picks = Vec::new();
    for n in nodes {
        let Filter::Set { field, by: By::Id, op, values } = n else { return vec![] };
        let ids: Option<Vec<i64>> = values.iter().map(|v| v.parse().ok()).collect();
        match (op, ids) {
            (SetOp::In | SetOp::ContainsAll, Some(ids)) => picks.push(Pick { field, all: op == SetOp::ContainsAll, ids }),
            _ => return vec![],
        }
    }
    picks
}

impl Db {
    /// The group's tags, categories or tools, by name.
    pub async fn vocabulary(&self, sc: &Scope, field: SetField) -> Result<Vec<Named>> {
        let rows = sqlx::query(&format!("SELECT id, name FROM {} WHERE group_id = ? ORDER BY name", field.table())).bind(sc.group_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Named { id: r.get(0), name: r.get(1) }).collect())
    }
}

// ---------- API ----------

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/cookbooks", get(list_cookbooks).post(create_cookbook)).route("/api/cookbooks/{id}", get(get_cookbook).put(put_cookbook).delete(delete_cookbook_route)).route("/api/cookbooks/{id}/recipes", get(cookbook_recipes_route))
}

#[utoipa::path(get, path = "/api/cookbooks", responses((status = 200, description = "The group's cookbooks, by position then name", body = [Cookbook])))]
pub(crate) async fn list_cookbooks(sc: Scope, State(db): State<Db>) -> Result<Json<Vec<Cookbook>>> {
    Ok(Json(db.cookbooks(&sc).await?))
}

#[utoipa::path(
    post, path = "/api/cookbooks", request_body = CookbookInput,
    responses((status = 201, description = "Cookbook created", body = Cookbook), (status = 422, description = "No name, a name in use, or a filter mealier cannot read (the error names the token)", body = ErrorBody))
)]
pub(crate) async fn create_cookbook(sc: Scope, State(db): State<Db>, Json(i): Json<CookbookInput>) -> Result<(StatusCode, Json<Cookbook>)> {
    Ok((StatusCode::CREATED, Json(db.add_cookbook(&sc, i).await?)))
}

#[utoipa::path(
    get, path = "/api/cookbooks/{id}", params(("id" = i64, Path, description = "Cookbook id")),
    responses((status = 200, description = "The cookbook", body = Cookbook), (status = 404, description = "No such cookbook", body = ErrorBody))
)]
pub(crate) async fn get_cookbook(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Cookbook>> {
    Ok(Json(db.cookbook(&sc, id).await?))
}

#[utoipa::path(
    put, path = "/api/cookbooks/{id}", params(("id" = i64, Path, description = "Cookbook id")), request_body = CookbookInput,
    responses((status = 200, description = "Cookbook replaced; a missing position is kept", body = Cookbook), (status = 404, description = "No such cookbook", body = ErrorBody), (status = 422, description = "Invalid cookbook or filter", body = ErrorBody))
)]
pub(crate) async fn put_cookbook(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, Json(i): Json<CookbookInput>) -> Result<Json<Cookbook>> {
    Ok(Json(db.update_cookbook(&sc, id, i).await?))
}

#[utoipa::path(
    delete, path = "/api/cookbooks/{id}", params(("id" = i64, Path, description = "Cookbook id")),
    responses((status = 204, description = "Deleted; the recipes stay"), (status = 404, description = "No such cookbook", body = ErrorBody))
)]
pub(crate) async fn delete_cookbook_route(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<StatusCode> {
    db.delete_cookbook(&sc, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/cookbooks/{id}/recipes", params(("id" = i64, Path, description = "Cookbook id")),
    responses((status = 200, description = "The group's live recipes the filter selects, by name; none when the filter is not supported", body = [RecipeSummary]), (status = 404, description = "No such cookbook", body = ErrorBody))
)]
pub(crate) async fn cookbook_recipes_route(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Vec<RecipeSummary>>> {
    Ok(Json(db.cookbook_recipes(&sc, id).await?))
}
