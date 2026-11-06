//! Cookbook pages: the list, one cookbook's recipes, and the edit page with the filter builder.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Redirect;
use axum::routing::{get, post};
use serde::Deserialize;

use super::{id_of, pairs};
use crate::api::AppState;
use crate::filter::SetField;
use crate::{Db, Error, Pick, Result, Scope, builder_string, picks_of};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/cookbooks", get(list_page).post(create))
        .route("/cookbooks/{id}", get(show_page))
        .route("/cookbooks/{id}/edit", get(edit_page).post(edit))
        .route("/cookbooks/{id}/builder", post(builder))
        .route("/cookbooks/{id}/delete", post(delete))
}

struct BookRow {
    id: i64,
    name: String,
    supported: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "cookbooks.html")]
struct ListPage {
    books: Vec<BookRow>,
}

async fn list_page(sc: Scope, State(db): State<Db>) -> Result<ListPage> {
    let books = db.cookbooks(&sc).await?.into_iter().map(|c| BookRow { id: c.id, name: c.name, supported: c.filter_supported }).collect();
    Ok(ListPage { books })
}

#[derive(Deserialize)]
struct NewBook {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    query_filter: String,
}

async fn create(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<NewBook>) -> Result<Redirect> {
    let c = db.add_cookbook(&sc, crate::CookbookInput { name: f.name, description: f.description, query_filter: f.query_filter, ..Default::default() }).await?;
    Ok(Redirect::to(&format!("/cookbooks/{}", c.id)))
}

/// Why a stored filter is not usable, in words.
fn reason(query_filter: &str) -> String {
    match crate::filter::parse(query_filter) {
        Err(e) => e.to_string(),
        Ok(_) => "filter not supported".into(),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "cookbook.html")]
struct ShowPage {
    id: i64,
    name: String,
    description: String,
    query_filter: String,
    supported: bool,
    reason: String,
    recipes: Vec<crate::RecipeSummary>,
}

async fn show_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<ShowPage> {
    let c = db.cookbook(&sc, id).await?;
    let recipes = db.cookbook_recipes(&sc, id).await?;
    Ok(ShowPage { id, reason: reason(&c.query_filter), name: c.name, description: c.description, query_filter: c.query_filter, supported: c.filter_supported, recipes })
}

struct Option_ {
    id: i64,
    name: String,
    checked: bool,
}

struct Group {
    title: &'static str,
    field: &'static str,
    all: bool,
    options: Vec<Option_>,
}

#[derive(Template, WebTemplate)]
#[template(path = "cookbook_edit.html")]
struct EditPage {
    id: i64,
    name: String,
    description: String,
    position: i64,
    public: bool,
    query_filter: String,
    supported: bool,
    reason: String,
    groups: Vec<Group>,
}

const KINDS: [(SetField, &str, &str); 3] = [(SetField::Tags, "tags", "Tags"), (SetField::Categories, "categories", "Categories"), (SetField::Tools, "tools", "Tools")];

async fn edit_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<EditPage> {
    let c = db.cookbook(&sc, id).await?;
    let picks = picks_of(&c.query_filter);
    let mut groups = Vec::new();
    for (field, key, title) in KINDS {
        let pick = picks.iter().find(|p| p.field == field);
        let options = db.vocabulary(&sc, field).await?.into_iter().map(|n| Option_ { checked: pick.is_some_and(|p| p.ids.contains(&n.id)), id: n.id, name: n.name }).collect();
        groups.push(Group { title, field: key, all: pick.is_some_and(|p| p.all), options });
    }
    Ok(EditPage { id, reason: reason(&c.query_filter), name: c.name, description: c.description, position: c.position, public: c.public, query_filter: c.query_filter, supported: c.filter_supported, groups })
}

#[derive(Deserialize)]
struct EditForm {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    position: String,
    public: Option<String>,
    #[serde(default)]
    query_filter: String,
}

async fn edit(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<EditForm>) -> Result<Redirect> {
    let position = id_of(&f.position)?;
    db.update_cookbook(&sc, id, crate::CookbookInput { name: f.name, description: f.description, position, public: f.public.is_some(), query_filter: f.query_filter }).await?;
    Ok(Redirect::to(&format!("/cookbooks/{id}")))
}

/// The ticked tags, categories and tools become the filter string. Every id must be the group's own.
async fn builder(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, body: Bytes) -> Result<Redirect> {
    let old = db.cookbook(&sc, id).await?;
    let form = pairs(&body)?;
    let mut picks = Vec::new();
    for (field, key, _) in KINDS {
        let mode = format!("{key}_mode");
        let all = form.iter().any(|(k, v)| *k == mode && v == "all");
        let mut ids = Vec::new();
        for (_, v) in form.iter().filter(|(k, _)| k == key) {
            ids.push(id_of(v)?.ok_or_else(|| Error::Validation(format!("an empty {key} id")))?);
        }
        let known = db.vocabulary(&sc, field).await?;
        if ids.iter().any(|i| !known.iter().any(|n| n.id == *i)) {
            return Err(Error::NotFound(match field {
                SetField::Tags => "tag",
                SetField::Categories => "category",
                SetField::Tools => "tool",
            }));
        }
        picks.push(Pick { field, all, ids });
    }
    db.update_cookbook(&sc, id, crate::CookbookInput { name: old.name, description: old.description, position: Some(old.position), public: old.public, query_filter: builder_string(&picks) }).await?;
    Ok(Redirect::to(&format!("/cookbooks/{id}/edit")))
}

async fn delete(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    db.delete_cookbook(&sc, id).await?;
    Ok(Redirect::to("/cookbooks"))
}
