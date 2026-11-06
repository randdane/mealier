//! Shopping list pages.
use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use serde::Deserialize;

use super::{amount, id_of, number};
use crate::api::AppState;
use crate::{Db, Error, Result, Scope};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/lists", get(lists_page).post(lists_create))
        .route("/lists/{id}", get(list_page))
        .route("/lists/{id}/items", post(list_add_item))
        .route("/lists/{id}/sections/{label}/move", post(section_move))
        .route("/lists/add-recipe", post(list_add_recipe))
        .route("/lists/add-batch", post(list_add_batch))
        .route("/lists/items/{id}/check", post(item_check))
        .route("/lists/items/{id}/delete", post(item_delete))
}

#[derive(Template, WebTemplate)]
#[template(path = "lists.html")]
struct ListsPage {
    lists: Vec<crate::ShoppingListSummary>,
}

async fn lists_page(sc: Scope, State(db): State<Db>) -> Result<ListsPage> {
    Ok(ListsPage { lists: db.shopping_lists(&sc).await? })
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn lists_create(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<NameForm>) -> Result<Redirect> {
    Ok(Redirect::to(&format!("/lists/{}", db.create_list(&sc, &f.name).await?.id)))
}

struct ItemRow {
    id: i64,
    checked: bool,
    text: String,
}

struct SectionRow {
    /// `None` for the unlabelled items, which cannot be moved.
    label_id: Option<i64>,
    title: String,
    color: String,
    items: Vec<ItemRow>,
}

#[derive(Template, WebTemplate)]
#[template(path = "list.html")]
struct ListPage {
    id: i64,
    name: String,
    sections: Vec<SectionRow>,
    units: Vec<crate::Unit>,
}

async fn list_page(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<ListPage> {
    let list = db.shopping_list(&sc, id).await?;
    let (units, foods) = (db.units(&sc).await?, db.foods(&sc).await?);
    let row = |i: &crate::ShoppingItem| {
        let unit = i.unit_id.and_then(|u| units.iter().find(|x| x.id == u)).map(|u| u.name.as_str());
        let food = i.food_id.and_then(|f| foods.iter().find(|x| x.id == f)).map(|f| f.name.as_str());
        let text = [amount(i.qty, i.qty_max, unit), food.unwrap_or_default().to_string(), i.note.clone()];
        ItemRow { id: i.id, checked: i.checked, text: text.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ") }
    };
    let sections = list
        .sections
        .iter()
        .map(|s| SectionRow {
            label_id: s.label.as_ref().map(|l| l.id),
            title: s.label.as_ref().map_or("No label".into(), |l| l.name.clone()),
            color: s.label.as_ref().map_or("var(--line)".into(), |l| l.color.clone()),
            items: s.items.iter().map(row).collect(),
        })
        .collect();
    Ok(ListPage { id, name: list.name, sections, units })
}

#[derive(Deserialize)]
struct MoveForm {
    /// `up` or `down`.
    dir: String,
}

async fn section_move(sc: Scope, State(db): State<Db>, Path((id, label)): Path<(i64, i64)>, axum::Form(f): axum::Form<MoveForm>) -> Result<Redirect> {
    db.move_section(&sc, id, label, f.dir == "up").await?;
    Ok(Redirect::to(&format!("/lists/{id}")))
}

#[derive(Deserialize)]
struct ItemForm {
    #[serde(default)]
    qty: String,
    #[serde(default)]
    unit_id: String,
    /// A food's name or alias; anything else is kept as a note.
    #[serde(default)]
    item: String,
}

async fn list_add_item(sc: Scope, State(db): State<Db>, Path(id): Path<i64>, axum::Form(f): axum::Form<ItemForm>) -> Result<Redirect> {
    let wanted = f.item.trim().to_lowercase();
    let food = db.foods(&sc).await?.into_iter().find(|x| std::iter::once(&x.name).chain(&x.aliases).any(|n| n.to_lowercase() == wanted));
    let input = crate::ItemInput {
        qty: number(&f.qty)?,
        unit_id: id_of(&f.unit_id)?,
        note: if food.is_some() { String::new() } else { f.item.trim().to_string() },
        food_id: food.map(|x| x.id),
        ..Default::default()
    };
    db.add_item(&sc, id, input).await?;
    Ok(Redirect::to(&format!("/lists/{id}")))
}

#[derive(Deserialize)]
struct AddToList {
    /// Existing list; ignored when `new_list` is filled in.
    #[serde(default)]
    list_id: String,
    #[serde(default)]
    new_list: String,
    recipe_id: Option<i64>,
    batch_id: Option<i64>,
    #[serde(default)]
    scale: String,
}

async fn target_list(db: &Db, sc: &Scope, f: &AddToList) -> Result<i64> {
    if !f.new_list.trim().is_empty() {
        return Ok(db.create_list(sc, &f.new_list).await?.id);
    }
    id_of(&f.list_id)?.ok_or_else(|| Error::Validation("choose a list or name a new one".into()))
}

async fn list_add_recipe(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<AddToList>) -> Result<Redirect> {
    let list = target_list(&db, &sc, &f).await?;
    let recipe = f.recipe_id.ok_or(Error::Validation("recipe_id is required".into()))?;
    db.add_recipe_to_list(&sc, list, recipe, number(&f.scale)?.unwrap_or(1.0)).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

async fn list_add_batch(sc: Scope, State(db): State<Db>, axum::Form(f): axum::Form<AddToList>) -> Result<Redirect> {
    let list = target_list(&db, &sc, &f).await?;
    db.add_batch_to_list(&sc, list, f.batch_id.ok_or(Error::Validation("batch_id is required".into()))?).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

#[derive(Deserialize)]
struct CheckForm {
    /// A checkbox: present when ticked.
    checked: Option<String>,
}

/// htmx gets 204 (the box already shows the state); a plain form post gets a redirect.
async fn item_check(
    sc: Scope,
    State(db): State<Db>,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    axum::Form(f): axum::Form<CheckForm>,
) -> Result<axum::response::Response> {
    db.set_item_checked(&sc, id, f.checked.is_some()).await?;
    if headers.contains_key("hx-request") {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(back_to_list(&db, &sc, id).await.into_response())
}

async fn item_delete(sc: Scope, State(db): State<Db>, Path(id): Path<i64>) -> Result<Redirect> {
    let list = db.item_list_id(&sc, id).await?;
    db.delete_item(&sc, id).await?;
    Ok(Redirect::to(&format!("/lists/{list}")))
}

async fn back_to_list(db: &Db, sc: &Scope, item: i64) -> Redirect {
    Redirect::to(&format!("/lists/{}", db.item_list_id(sc, item).await.unwrap_or(0)))
}
