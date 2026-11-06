use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};
use utoipa::ToSchema;

use crate::images::Processed;
use crate::{Db, Error, Result, Scope};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Nutrition {
    pub calories: Option<f64>,
    pub protein_g: Option<f64>,
    pub fat_g: Option<f64>,
    pub carbs_g: Option<f64>,
}

/// Another way to make an ingredient line. Names are copied, so a snapshot never points at the
/// shared tables.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Substitution {
    pub qty: Option<f64>,
    pub unit: Option<String>,
    pub food: Option<String>,
    pub note: String,
}

/// What an ingredient line carries beyond `IngredientInput`, sent in `RecipeInput.ingredient_details`
/// at the same index as the line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct LineDetail {
    /// The line's stable id. Send back the one a read gave to keep it; leave out for a new line.
    pub uid: Option<String>,
    /// A title starting a section at this line.
    pub section: String,
    pub substitutions: Vec<Substitution>,
}

/// A step's title and the ingredient lines it uses (by `uid`), at the same index as the step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct StepDetail {
    pub title: String,
    pub ingredient_uids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Note {
    pub title: String,
    pub text: String,
}

/// Recipe settings. `locked`, `hide_nutrition` and `disable_comments` are enforced; the rest are
/// Mealie's, kept so a migration loses nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Settings {
    pub locked: bool,
    pub hide_nutrition: bool,
    pub disable_comments: bool,
    pub public: bool,
    pub show_assets: bool,
    pub landscape_view: bool,
    pub disable_amount: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct IngredientInput {
    pub qty_min: Option<f64>,
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    pub food_id: Option<i64>,
    pub note: String,
    pub original_text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct RecipeInput {
    pub name: String,
    pub description: String,
    pub source_url: Option<String>,
    pub nutrition: Option<Nutrition>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub cookbooks: Vec<String>,
    pub tools: Vec<String>,
    pub ingredients: Vec<IngredientInput>,
    pub steps: Vec<String>,
    /// Section, id and substitutions of each ingredient line, by index. Replaces what the lines had.
    pub ingredient_details: Vec<LineDetail>,
    /// Title and ingredient links of each step, by index. Replaces what the steps had.
    pub step_details: Vec<StepDetail>,
    /// Left out: the recipe keeps its notes (an empty recipe has none).
    pub notes: Option<Vec<Note>>,
    /// Left out: the recipe keeps its extras.
    pub extras: Option<BTreeMap<String, String>>,
    /// Left out: the recipe keeps its settings. A locked recipe refuses every edit that does not send `locked: false`.
    pub settings: Option<Settings>,
}

/// An ingredient line as stored in a snapshot: unit and food names (and the
/// unit factor) are copied, so later renames do not change history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Ingredient {
    pub qty_min: Option<f64>,
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    pub unit_name: Option<String>,
    pub unit_factor: Option<f64>,
    pub food_id: Option<i64>,
    pub food_name: Option<String>,
    pub note: String,
    pub original_text: String,
    /// Stable across edits of an unchanged line; steps link to it. Empty on lines written before step 14.
    #[serde(default)]
    pub uid: String,
    /// A title starting a section at this line; empty when none does.
    #[serde(default)]
    pub section: String,
    #[serde(default)]
    pub substitutions: Vec<Substitution>,
}

/// Everything a snapshot freezes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Content {
    pub name: String,
    pub description: String,
    pub source_url: Option<String>,
    pub nutrition: Option<Nutrition>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub cookbooks: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    pub ingredients: Vec<Ingredient>,
    pub steps: Vec<String>,
    /// Title and ingredient links of each step, by index (empty on recipes written before step 14).
    #[serde(default)]
    pub step_details: Vec<StepDetail>,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default)]
    pub extras: BTreeMap<String, String>,
    #[serde(default)]
    pub settings: Settings,
    /// Hashes of the recipe's images when this was written, in order. Frozen with the rest, so
    /// replacing an image never rewrites history.
    #[serde(default)]
    pub images: Vec<String>,
    /// Hashes of the recipe's assets (non-image attachments) when this was written, in order.
    #[serde(default)]
    pub assets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Recipe {
    pub id: i64,
    pub archived: bool,
    pub current_snapshot_id: i64,
    #[serde(flatten)]
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct RecipeSummary {
    pub id: i64,
    pub name: String,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct SnapshotSummary {
    pub id: i64,
    pub recipe_id: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Snapshot {
    pub id: i64,
    pub recipe_id: i64,
    pub created_at: String,
    pub content: Content,
}

/// The one rule for a quantity range, wherever one is stored (recipe lines, batch lines, shopping items).
/// A lone minimum is not a range: the maximum is filled from it. A maximum alone, a negative or
/// non-finite number, or a minimum above the maximum is refused.
pub(crate) fn range(min: Option<f64>, max: Option<f64>) -> Result<(Option<f64>, Option<f64>)> {
    if [min, max].into_iter().flatten().any(|q| !q.is_finite()) {
        return Err(Error::Validation("quantities must be finite numbers".into()));
    }
    let max = match (min, max) {
        (Some(min), None) => Some(min),
        (None, Some(_)) => return Err(Error::Validation("a maximum quantity needs a minimum".into())),
        (_, max) => max,
    };
    if let (Some(min), Some(max)) = (min, max) {
        if min < 0.0 || max < 0.0 {
            return Err(Error::Validation("quantities must not be negative".into()));
        }
        if min > max {
            return Err(Error::Validation("the minimum quantity must not exceed the maximum".into()));
        }
    }
    Ok((min, max))
}

/// `uid`s are 1 to 64 characters of letters, digits, `-` and `_`.
fn valid_uid(u: &str) -> bool {
    !u.is_empty() && u.len() <= 64 && u.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn new_uid() -> Result<String> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| Error::Internal(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Extras are string pairs with a non-blank key; keys are trimmed.
pub(crate) fn check_extras(extras: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (k, v) in extras {
        let k = k.trim().to_string();
        if k.is_empty() || k.chars().count() > 100 {
            return Err(Error::Validation("extras need a key of 1 to 100 characters".into()));
        }
        if v.chars().count() > 2000 {
            return Err(Error::Validation("an extras value must be at most 2000 characters".into()));
        }
        out.insert(k, v);
    }
    Ok(out)
}

fn text_or_none(v: Option<String>) -> Option<String> {
    v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn check_substitution(mut s: Substitution) -> Result<Substitution> {
    if s.qty.is_some_and(|q| !q.is_finite() || q < 0.0) {
        return Err(Error::Validation("substitution quantities must be finite, non-negative numbers".into()));
    }
    (s.unit, s.food, s.note) = (text_or_none(s.unit), text_or_none(s.food), s.note.trim().to_string());
    if s.qty.is_none() && s.unit.is_none() && s.food.is_none() && s.note.is_empty() {
        return Err(Error::Validation("a substitution needs a quantity, unit, food or note".into()));
    }
    Ok(s)
}

/// `prev` is the recipe's current content on an edit: what the input leaves out (notes, extras, settings) is kept from it.
async fn build_content(tx: &mut Transaction<'_, Sqlite>, group_id: i64, input: RecipeInput, images: Vec<String>, assets: Vec<String>, prev: Option<&Content>) -> Result<Content> {
    if input.name.trim().is_empty() {
        return Err(Error::Validation("recipe name must not be empty".into()));
    }
    if input.ingredient_details.len() > input.ingredients.len() {
        return Err(Error::Validation("ingredient_details has more entries than ingredients".into()));
    }
    if input.step_details.len() > input.steps.len() {
        return Err(Error::Validation("step_details has more entries than steps".into()));
    }
    let mut details = input.ingredient_details.into_iter();
    let mut uids = std::collections::HashSet::new();
    let mut ingredients = Vec::new();
    for mut i in input.ingredients {
        let d = details.next().unwrap_or_default();
        (i.qty_min, i.qty_max) = range(i.qty_min, i.qty_max)?;
        let unit = match i.unit_id {
            Some(id) => {
                let row = sqlx::query("SELECT name, factor FROM units WHERE id = ? AND group_id = ?")
                    .bind(id)
                    .bind(group_id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(Error::NotFound("unit"))?;
                Some((row.get::<String, _>(0), row.get::<Option<f64>, _>(1)))
            }
            None => None,
        };
        let food_name = match i.food_id {
            Some(id) => Some(
                sqlx::query_scalar::<_, String>("SELECT name FROM foods WHERE id = ? AND group_id = ?")
                    .bind(id)
                    .bind(group_id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(Error::NotFound("food"))?,
            ),
            None => None,
        };
        let uid = match d.uid.filter(|u| !u.is_empty()) {
            Some(u) if !valid_uid(&u) => return Err(Error::Validation("an ingredient line id may only use letters, digits, - and _ (at most 64)".into())),
            Some(u) => u,
            None => loop {
                let u = new_uid()?;
                if !uids.contains(&u) {
                    break u;
                }
            },
        };
        if !uids.insert(uid.clone()) {
            return Err(Error::Validation(format!("ingredient line id {uid:?} is used twice")));
        }
        ingredients.push(Ingredient {
            qty_min: i.qty_min,
            qty_max: i.qty_max,
            unit_id: i.unit_id,
            unit_name: unit.as_ref().map(|u| u.0.clone()),
            unit_factor: unit.and_then(|u| u.1),
            food_id: i.food_id,
            food_name,
            note: i.note,
            original_text: i.original_text,
            uid,
            section: d.section.trim().to_string(),
            substitutions: d.substitutions.into_iter().map(check_substitution).collect::<Result<_>>()?,
        });
    }
    let mut step_details = Vec::new();
    let mut given = input.step_details.into_iter();
    for _ in &input.steps {
        let mut d = given.next().unwrap_or_default();
        d.title = d.title.trim().to_string();
        let mut seen = Vec::new();
        for u in std::mem::take(&mut d.ingredient_uids) {
            if !uids.contains(&u) {
                return Err(Error::Validation("a step links to an unknown ingredient line".into()));
            }
            if !seen.contains(&u) {
                seen.push(u);
            }
        }
        d.ingredient_uids = seen;
        step_details.push(d);
    }
    let notes = match input.notes {
        Some(notes) => notes
            .into_iter()
            .map(|n| match n.text.trim() {
                "" => Err(Error::Validation("a note needs text".into())),
                text => Ok(Note { title: n.title.trim().to_string(), text: text.to_string() }),
            })
            .collect::<Result<_>>()?,
        None => prev.map(|p| p.notes.clone()).unwrap_or_default(),
    };
    let extras = match input.extras {
        Some(e) => check_extras(e)?,
        None => prev.map(|p| p.extras.clone()).unwrap_or_default(),
    };
    let settings = input.settings.or_else(|| prev.map(|p| p.settings.clone())).unwrap_or_default();
    // The group's vocabularies learn every name a recipe uses.
    for (table, names) in [("tags", &input.tags), ("categories", &input.categories), ("tools", &input.tools)] {
        for name in names {
            sqlx::query(&format!("INSERT OR IGNORE INTO {table} (group_id, name) VALUES (?, ?)")).bind(group_id).bind(name).execute(&mut **tx).await?;
        }
    }
    for name in &input.cookbooks {
        crate::cookbooks::ensure_cookbook(tx, group_id, name).await?;
    }
    Ok(Content {
        name: input.name,
        description: input.description,
        source_url: input.source_url,
        nutrition: input.nutrition,
        tags: input.tags,
        categories: input.categories,
        cookbooks: input.cookbooks,
        tools: input.tools,
        ingredients,
        steps: input.steps,
        step_details,
        notes,
        extras,
        settings,
        images,
        assets,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dimension {
    Mass,
    Volume,
    Count,
    Other,
}

impl Dimension {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Dimension::Mass => "mass",
            Dimension::Volume => "volume",
            Dimension::Count => "count",
            Dimension::Other => "other",
        }
    }
}

/// Writes the snapshot, points the recipe at it, returns the snapshot id. The caller has already found the recipe in its group.
async fn write_snapshot(tx: &mut Transaction<'_, Sqlite>, recipe_id: i64, c: &Content) -> Result<i64> {
    let snap = sqlx::query("INSERT INTO recipe_snapshots (recipe_id, content) VALUES (?, ?)")
        .bind(recipe_id)
        .bind(serde_json::to_string(c)?)
        .execute(&mut **tx)
        .await?
        .last_insert_rowid();
    sqlx::query(
        "UPDATE recipes SET name = ?, description = ?, source_url = ?, nutrition = ?, current_snapshot_id = ? WHERE id = ?",
    )
    .bind(&c.name)
    .bind(&c.description)
    .bind(&c.source_url)
    .bind(c.nutrition.as_ref().map(serde_json::to_string).transpose()?)
    .bind(snap)
    .bind(recipe_id)
    .execute(&mut **tx)
    .await?;
    Ok(snap)
}

fn snapshot_from(row: &sqlx::sqlite::SqliteRow) -> Result<Snapshot> {
    Ok(Snapshot {
        id: row.get(0),
        recipe_id: row.get(1),
        created_at: row.get(2),
        content: serde_json::from_str(row.get(3))?,
    })
}

/// Creates a recipe and its first snapshot inside the caller's transaction.
/// `images` and `assets` are the hashes the first snapshot lists; the caller links them to the recipe.
pub(crate) async fn insert_recipe(tx: &mut Transaction<'_, Sqlite>, group_id: i64, input: RecipeInput, images: Vec<String>, assets: Vec<String>) -> Result<Recipe> {
    let content = build_content(tx, group_id, input, images, assets, None).await?;
    let id = sqlx::query("INSERT INTO recipes (group_id, name) VALUES (?, ?)")
        .bind(group_id)
        .bind(&content.name)
        .execute(&mut **tx)
        .await?
        .last_insert_rowid();
    let current_snapshot_id = write_snapshot(tx, id, &content).await?;
    Ok(Recipe { id, archived: false, current_snapshot_id, content })
}

/// Files, records and links an image for a recipe. The file goes down before the commit: it is content-addressed, so one left by a failed commit is harmless.
async fn link_image(tx: &mut Transaction<'_, Sqlite>, recipe_id: i64, photo: &Processed, dir: &std::path::Path) -> Result<()> {
    photo.write(dir)?;
    photo.record(tx).await?;
    sqlx::query("INSERT OR IGNORE INTO recipe_images (recipe_id, hash) VALUES (?, ?)").bind(recipe_id).bind(&photo.hash).execute(&mut **tx).await?;
    Ok(())
}

/// Writes a new snapshot of recipe `id` (in `group_id`) and points the recipe at it: `input`, or, when
/// `None`, what the recipe already says (for edits that only change an attachment). The one place an
/// edit is made, so the lock is checked here for all of them: a locked recipe refuses unless the input
/// says `locked: false`.
pub(crate) async fn rewrite(tx: &mut Transaction<'_, Sqlite>, group_id: i64, id: i64, input: Option<RecipeInput>, image: Option<(&Processed, &std::path::Path)>) -> Result<Recipe> {
    let row = sqlx::query("SELECT r.archived, s.content FROM recipes r JOIN recipe_snapshots s ON s.id = r.current_snapshot_id WHERE r.id = ? AND r.group_id = ?")
        .bind(id)
        .bind(group_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::NotFound("recipe"))?;
    let archived: bool = row.get(0);
    let prev: Content = serde_json::from_str(row.get(1))?;
    let input = input.unwrap_or_else(|| prev.clone().into());
    if prev.settings.locked && input.settings.as_ref().is_none_or(|s| s.locked) {
        return Err(Error::Validation("recipe is locked".into()));
    }
    let images = match image {
        Some((p, _)) => vec![p.hash.clone()],
        None => sqlx::query_scalar("SELECT hash FROM recipe_images WHERE recipe_id = ? ORDER BY position, hash").bind(id).fetch_all(&mut **tx).await?,
    };
    let assets = sqlx::query_scalar("SELECT hash FROM recipe_assets WHERE recipe_id = ? ORDER BY id").bind(id).fetch_all(&mut **tx).await?;
    let content = build_content(tx, group_id, input, images, assets, Some(&prev)).await?;
    let current_snapshot_id = write_snapshot(tx, id, &content).await?;
    if let Some((photo, dir)) = image {
        sqlx::query("DELETE FROM recipe_images WHERE recipe_id = ?").bind(id).execute(&mut **tx).await?;
        link_image(tx, id, photo, dir).await?;
    }
    Ok(Recipe { id, archived, current_snapshot_id, content })
}

impl From<Content> for RecipeInput {
    /// What an edit that changes nothing would send. Unit and food names are looked up again on save.
    fn from(c: Content) -> RecipeInput {
        RecipeInput {
            name: c.name,
            description: c.description,
            source_url: c.source_url,
            nutrition: c.nutrition,
            tags: c.tags,
            categories: c.categories,
            cookbooks: c.cookbooks,
            tools: c.tools,
            ingredients: c.ingredients.iter().map(IngredientInput::from).collect(),
            steps: c.steps,
            ingredient_details: c.ingredients.into_iter().map(|i| LineDetail { uid: Some(i.uid), section: i.section, substitutions: i.substitutions }).collect(),
            step_details: c.step_details,
            notes: Some(c.notes),
            extras: Some(c.extras),
            settings: Some(c.settings),
        }
    }
}

impl From<&Ingredient> for IngredientInput {
    fn from(i: &Ingredient) -> IngredientInput {
        IngredientInput { qty_min: i.qty_min, qty_max: i.qty_max, unit_id: i.unit_id, food_id: i.food_id, note: i.note.clone(), original_text: i.original_text.clone() }
    }
}

impl Db {
    /// `NotFound` unless the recipe is in the caller's group. The first line of every recipe-keyed read.
    pub(crate) async fn check_recipe(&self, sc: &Scope, id: i64) -> Result<()> {
        let found = sqlx::query("SELECT 1 FROM recipes WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?;
        found.map(|_| ()).ok_or(Error::NotFound("recipe"))
    }

    pub async fn add_unit(&self, sc: &Scope, name: &str, dimension: Dimension, factor: Option<f64>) -> Result<i64> {
        let ok = match dimension {
            Dimension::Mass | Dimension::Volume => factor.is_some_and(|f| f > 0.0),
            Dimension::Count | Dimension::Other => factor.is_none(),
        };
        if !ok {
            return Err(Error::Validation(
                "mass and volume units need a factor > 0; count and other units must have none".into(),
            ));
        }
        Ok(sqlx::query("INSERT INTO units (group_id, name, dimension, factor) VALUES (?, ?, ?, ?)")
            .bind(sc.group_id)
            .bind(name)
            .bind(dimension.as_str())
            .bind(factor)
            .execute(&self.pool)
            .await?
            .last_insert_rowid())
    }

    pub async fn rename_unit(&self, sc: &Scope, id: i64, name: &str) -> Result<()> {
        let n = sqlx::query("UPDATE units SET name = ? WHERE id = ? AND group_id = ?").bind(name).bind(id).bind(sc.group_id).execute(&self.pool).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("unit"));
        }
        Ok(())
    }

    pub async fn add_food(&self, sc: &Scope, name: &str) -> Result<i64> {
        Ok(sqlx::query("INSERT INTO foods (group_id, name) VALUES (?, ?)").bind(sc.group_id).bind(name).execute(&self.pool).await?.last_insert_rowid())
    }

    pub async fn rename_food(&self, sc: &Scope, id: i64, name: &str) -> Result<()> {
        let n = sqlx::query("UPDATE foods SET name = ? WHERE id = ? AND group_id = ?").bind(name).bind(id).bind(sc.group_id).execute(&self.pool).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("food"));
        }
        Ok(())
    }

    pub async fn update_recipe(&self, sc: &Scope, id: i64, input: RecipeInput) -> Result<Recipe> {
        self.update_recipe_with(sc, id, input, None).await
    }

    /// `update_recipe`, and optionally a new image (which replaces the recipe's image) in the same snapshot.
    pub(crate) async fn update_recipe_with(&self, sc: &Scope, id: i64, input: RecipeInput, image: Option<(&Processed, &std::path::Path)>) -> Result<Recipe> {
        let mut tx = self.pool.begin().await?;
        let recipe = rewrite(&mut tx, sc.group_id, id, Some(input), image).await?;
        tx.commit().await?;
        self.changed("recipe_updated", sc, recipe.id, recipe.content.name.clone(), serde_json::json!({}));
        Ok(recipe)
    }

    /// Replaces the recipe's image. An edit like any other: it writes a snapshot, so the old one still lists the old image.
    pub async fn set_recipe_image(&self, sc: &Scope, id: i64, images_dir: &std::path::Path, bytes: &[u8]) -> Result<Recipe> {
        let photo = crate::images::prepare(bytes).await?;
        let current = self.get_recipe(sc, id).await?.ok_or(Error::NotFound("recipe"))?;
        self.update_recipe_with(sc, id, current.content.into(), Some((&photo, images_dir))).await
    }

    pub async fn archive_recipe(&self, sc: &Scope, id: i64) -> Result<()> {
        let n = sqlx::query("UPDATE recipes SET archived = 1 WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).execute(&self.pool).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("recipe"));
        }
        let name = self.get_recipe(sc, id).await?.map(|r| r.content.name).unwrap_or_default();
        self.changed("recipe_deleted", sc, id, name, serde_json::json!({}));
        Ok(())
    }

    pub async fn list_recipes(&self, sc: &Scope, include_archived: bool, q: Option<&str>) -> Result<Vec<RecipeSummary>> {
        // Escape LIKE wildcards so the query is matched literally.
        let pattern = q.map(|q| format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let rows = sqlx::query(
            "SELECT id, name, archived FROM recipes
             WHERE group_id = ?3 AND (archived = 0 OR ?1)
               AND (?2 IS NULL OR name LIKE ?2 ESCAPE '\\' OR description LIKE ?2 ESCAPE '\\')
             ORDER BY name, id",
        )
        .bind(include_archived)
        .bind(pattern)
        .bind(sc.group_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| RecipeSummary { id: r.get(0), name: r.get(1), archived: r.get(2) }).collect())
    }

    pub async fn snapshot(&self, sc: &Scope, id: i64) -> Result<Snapshot> {
        let row = sqlx::query(
            "SELECT s.id, s.recipe_id, s.created_at, s.content FROM recipe_snapshots s
             JOIN recipes r ON r.id = s.recipe_id WHERE s.id = ? AND r.group_id = ?",
        )
        .bind(id)
        .bind(sc.group_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("snapshot"))?;
        snapshot_from(&row)
    }

    pub async fn create_recipe(&self, sc: &Scope, input: RecipeInput) -> Result<Recipe> {
        self.create_recipe_with(sc, input, None).await
    }

    /// `create_recipe`, with an image already in the first snapshot.
    pub(crate) async fn create_recipe_with(&self, sc: &Scope, input: RecipeInput, image: Option<(&Processed, &std::path::Path)>) -> Result<Recipe> {
        let mut tx = self.pool.begin().await?;
        let recipe = insert_recipe(&mut tx, sc.group_id, input, image.iter().map(|(p, _)| p.hash.clone()).collect(), Vec::new()).await?;
        if let Some((photo, dir)) = image {
            link_image(&mut tx, recipe.id, photo, dir).await?;
        }
        tx.commit().await?;
        self.changed("recipe_created", sc, recipe.id, recipe.content.name.clone(), serde_json::json!({}));
        Ok(recipe)
    }

    pub async fn get_recipe(&self, sc: &Scope, id: i64) -> Result<Option<Recipe>> {
        let Some(row) = sqlx::query(
            "SELECT r.archived, r.current_snapshot_id, s.content FROM recipes r
             JOIN recipe_snapshots s ON s.id = r.current_snapshot_id WHERE r.id = ? AND r.group_id = ?",
        )
        .bind(id)
        .bind(sc.group_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(Recipe {
            id,
            archived: row.get(0),
            current_snapshot_id: row.get(1),
            content: serde_json::from_str(row.get(2))?,
        }))
    }

    /// Newest first.
    pub async fn snapshots(&self, sc: &Scope, recipe_id: i64) -> Result<Vec<SnapshotSummary>> {
        let rows = sqlx::query(
            "SELECT s.id, s.created_at FROM recipe_snapshots s JOIN recipes r ON r.id = s.recipe_id
             WHERE s.recipe_id = ? AND r.group_id = ? ORDER BY s.id DESC",
        )
        .bind(recipe_id)
        .bind(sc.group_id)
            .fetch_all(&self.pool)
            .await?;
        // Every recipe has at least one snapshot, so none means no such recipe.
        if rows.is_empty() {
            return Err(Error::NotFound("recipe"));
        }
        Ok(rows
            .iter()
            .map(|r| SnapshotSummary { id: r.get(0), recipe_id, created_at: r.get(1) })
            .collect())
    }
}
