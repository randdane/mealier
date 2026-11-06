use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};

use crate::{Db, Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Nutrition {
    pub calories: Option<f64>,
    pub protein_g: Option<f64>,
    pub fat_g: Option<f64>,
    pub carbs_g: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IngredientInput {
    pub qty_min: Option<f64>,
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    pub food_id: Option<i64>,
    pub note: String,
    pub original_text: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RecipeInput {
    pub name: String,
    pub description: String,
    pub source_url: Option<String>,
    pub nutrition: Option<Nutrition>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub cookbooks: Vec<String>,
    pub ingredients: Vec<IngredientInput>,
    pub steps: Vec<String>,
}

/// An ingredient line as stored in a snapshot: unit and food names (and the
/// unit factor) are copied, so later renames do not change history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

/// Everything a snapshot freezes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Content {
    pub name: String,
    pub description: String,
    pub source_url: Option<String>,
    pub nutrition: Option<Nutrition>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub cookbooks: Vec<String>,
    pub ingredients: Vec<Ingredient>,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recipe {
    pub id: i64,
    pub archived: bool,
    pub current_snapshot_id: i64,
    pub name: String,
    pub description: String,
    pub source_url: Option<String>,
    pub nutrition: Option<Nutrition>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub cookbooks: Vec<String>,
    pub ingredients: Vec<Ingredient>,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecipeSummary {
    pub id: i64,
    pub name: String,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotSummary {
    pub id: i64,
    pub recipe_id: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub id: i64,
    pub recipe_id: i64,
    pub created_at: String,
    pub content: Content,
}

async fn build_content(tx: &mut Transaction<'_, Sqlite>, input: RecipeInput) -> Result<Content> {
    if input.name.trim().is_empty() {
        return Err(Error::Validation("recipe name must not be empty".into()));
    }
    let mut ingredients = Vec::new();
    for i in input.ingredients {
        let unit = match i.unit_id {
            Some(id) => {
                let row = sqlx::query("SELECT name, factor FROM units WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(Error::NotFound("unit"))?;
                Some((row.get::<String, _>(0), row.get::<Option<f64>, _>(1)))
            }
            None => None,
        };
        let food_name = match i.food_id {
            Some(id) => Some(
                sqlx::query_scalar::<_, String>("SELECT name FROM foods WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(Error::NotFound("food"))?,
            ),
            None => None,
        };
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
        });
    }
    Ok(Content {
        name: input.name,
        description: input.description,
        source_url: input.source_url,
        nutrition: input.nutrition,
        tags: input.tags,
        categories: input.categories,
        cookbooks: input.cookbooks,
        ingredients,
        steps: input.steps,
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
    fn as_str(self) -> &'static str {
        match self {
            Dimension::Mass => "mass",
            Dimension::Volume => "volume",
            Dimension::Count => "count",
            Dimension::Other => "other",
        }
    }
}

/// Replace a recipe's tags/categories/cookbooks. Arguments are fixed
/// identifiers from this file, never user input.
async fn set_names(
    tx: &mut Transaction<'_, Sqlite>,
    (table, join, col): (&str, &str, &str),
    recipe_id: i64,
    names: &[String],
) -> Result<()> {
    sqlx::query(&format!("DELETE FROM {join} WHERE recipe_id = ?"))
        .bind(recipe_id)
        .execute(&mut **tx)
        .await?;
    for n in names {
        sqlx::query(&format!("INSERT OR IGNORE INTO {table} (name) VALUES (?)"))
            .bind(n)
            .execute(&mut **tx)
            .await?;
        sqlx::query(&format!(
            "INSERT OR IGNORE INTO {join} (recipe_id, {col}) SELECT ?, id FROM {table} WHERE name = ?"
        ))
        .bind(recipe_id)
        .bind(n)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn get_names(db: &Db, (table, join, col): (&str, &str, &str), recipe_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT t.name FROM {table} t JOIN {join} j ON j.{col} = t.id WHERE j.recipe_id = ? ORDER BY t.name"
    ))
    .bind(recipe_id)
    .fetch_all(&db.pool)
    .await?)
}

const TAGS: (&str, &str, &str) = ("tags", "recipe_tags", "tag_id");
const CATEGORIES: (&str, &str, &str) = ("categories", "recipe_categories", "category_id");
const COOKBOOKS: (&str, &str, &str) = ("cookbooks", "recipe_cookbooks", "cookbook_id");

async fn write_snapshot(tx: &mut Transaction<'_, Sqlite>, recipe_id: i64, c: &Content) -> Result<()> {
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
    set_names(tx, TAGS, recipe_id, &c.tags).await?;
    set_names(tx, CATEGORIES, recipe_id, &c.categories).await?;
    set_names(tx, COOKBOOKS, recipe_id, &c.cookbooks).await?;
    Ok(())
}

fn snapshot_from(row: &sqlx::sqlite::SqliteRow) -> Result<Snapshot> {
    Ok(Snapshot {
        id: row.get(0),
        recipe_id: row.get(1),
        created_at: row.get(2),
        content: serde_json::from_str(row.get(3))?,
    })
}

impl Db {
    pub async fn add_unit(&self, name: &str, dimension: Dimension, factor: Option<f64>) -> Result<i64> {
        Ok(sqlx::query("INSERT INTO units (name, dimension, factor) VALUES (?, ?, ?)")
            .bind(name)
            .bind(dimension.as_str())
            .bind(factor)
            .execute(&self.pool)
            .await?
            .last_insert_rowid())
    }

    pub async fn rename_unit(&self, id: i64, name: &str) -> Result<()> {
        let n = sqlx::query("UPDATE units SET name = ? WHERE id = ?").bind(name).bind(id).execute(&self.pool).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("unit"));
        }
        Ok(())
    }

    pub async fn add_food(&self, name: &str) -> Result<i64> {
        Ok(sqlx::query("INSERT INTO foods (name) VALUES (?)").bind(name).execute(&self.pool).await?.last_insert_rowid())
    }

    pub async fn rename_food(&self, id: i64, name: &str) -> Result<()> {
        let n = sqlx::query("UPDATE foods SET name = ? WHERE id = ?").bind(name).bind(id).execute(&self.pool).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("food"));
        }
        Ok(())
    }

    pub async fn update_recipe(&self, id: i64, input: RecipeInput) -> Result<Recipe> {
        let mut tx = self.pool.begin().await?;
        let content = build_content(&mut tx, input).await?;
        if sqlx::query_scalar::<_, i64>("SELECT id FROM recipes WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .is_none()
        {
            return Err(Error::NotFound("recipe"));
        }
        write_snapshot(&mut tx, id, &content).await?;
        tx.commit().await?;
        self.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))
    }

    pub async fn archive_recipe(&self, id: i64) -> Result<()> {
        let n = sqlx::query("UPDATE recipes SET archived = 1 WHERE id = ?").bind(id).execute(&self.pool).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("recipe"));
        }
        Ok(())
    }

    pub async fn list_recipes(&self, include_archived: bool) -> Result<Vec<RecipeSummary>> {
        let rows = sqlx::query("SELECT id, name, archived FROM recipes WHERE archived = 0 OR ? ORDER BY name, id")
            .bind(include_archived)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| RecipeSummary { id: r.get(0), name: r.get(1), archived: r.get(2) }).collect())
    }

    pub async fn snapshot(&self, id: i64) -> Result<Snapshot> {
        let row = sqlx::query("SELECT id, recipe_id, created_at, content FROM recipe_snapshots WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("snapshot"))?;
        snapshot_from(&row)
    }

    pub async fn create_recipe(&self, input: RecipeInput) -> Result<Recipe> {
        let mut tx = self.pool.begin().await?;
        let content = build_content(&mut tx, input).await?;
        let id = sqlx::query("INSERT INTO recipes (name) VALUES (?)")
            .bind(&content.name)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        write_snapshot(&mut tx, id, &content).await?;
        tx.commit().await?;
        self.get_recipe(id).await?.ok_or(Error::NotFound("recipe"))
    }

    pub async fn get_recipe(&self, id: i64) -> Result<Option<Recipe>> {
        let Some(row) = sqlx::query(
            "SELECT r.archived, r.current_snapshot_id, s.content FROM recipes r
             JOIN recipe_snapshots s ON s.id = r.current_snapshot_id WHERE r.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let c: Content = serde_json::from_str(row.get(2))?;
        Ok(Some(Recipe {
            id,
            archived: row.get(0),
            current_snapshot_id: row.get(1),
            name: c.name,
            description: c.description,
            source_url: c.source_url,
            nutrition: c.nutrition,
            tags: get_names(self, TAGS, id).await?,
            categories: get_names(self, CATEGORIES, id).await?,
            cookbooks: get_names(self, COOKBOOKS, id).await?,
            ingredients: c.ingredients,
            steps: c.steps,
        }))
    }

    /// Newest first.
    pub async fn snapshots(&self, recipe_id: i64) -> Result<Vec<SnapshotSummary>> {
        let rows = sqlx::query("SELECT id, created_at FROM recipe_snapshots WHERE recipe_id = ? ORDER BY id DESC")
            .bind(recipe_id)
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
