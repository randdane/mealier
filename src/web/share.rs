//! The read-only recipe page behind a share link: the recipe's own content and nothing else.
use askama::Template;
use askama_web::WebTemplate;

use super::recipes::{ContentView, IngGroup, StepView, content_view};
use crate::{Db, Note, Recipe, Result, Scope};

pub(crate) struct SharedAsset {
    pub name: String,
    pub hash: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "share.html")]
pub(crate) struct SharePage {
    token: String,
    name: String,
    description: String,
    image: String,
    groups: Vec<IngGroup>,
    steps: Vec<StepView>,
    nutrition: Vec<String>,
    notes: Vec<Note>,
    assets: Vec<SharedAsset>,
}

/// `sc` is the group scope the link grants. Assets are listed only when the recipe's settings show them.
pub(crate) async fn share_page(db: &Db, sc: &Scope, recipe: Recipe, token: &str) -> Result<SharePage> {
    let ContentView { groups, steps, nutrition } = content_view(db, sc, &recipe.content).await?;
    let assets = if recipe.content.settings.show_assets { db.recipe_assets(sc, recipe.id).await?.into_iter().map(|a| SharedAsset { name: a.name, hash: a.hash }).collect() } else { Vec::new() };
    Ok(SharePage {
        token: token.into(),
        image: db.recipe_images(sc, recipe.id).await?.first().map(crate::Image::file_name).unwrap_or_default(),
        name: recipe.content.name,
        description: recipe.content.description,
        groups,
        steps,
        nutrition,
        notes: recipe.content.notes,
        assets,
    })
}
