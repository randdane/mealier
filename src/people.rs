//! Read side of the tables the Mealie importer fills.
use serde::Serialize;
use utoipa::ToSchema;
use sqlx::Row;

use crate::{Db, Dimension, Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Unit {
    pub id: i64,
    pub name: String,
    pub aliases: Vec<String>,
    pub dimension: Dimension,
    pub factor: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Food {
    pub id: i64,
    pub name: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub full_name: String,
    pub role: String,
    pub oidc_issuer: Option<String>,
    pub oidc_subject: Option<String>,
    pub disabled: bool,
    pub cook_theme: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Batch {
    pub id: i64,
    pub status: String,
    pub recipe_id: i64,
    pub snapshot_id: Option<i64>,
    pub source: String,
    pub made_on: Option<String>,
    pub scale: f64,
    pub rating: Option<i64>,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct MealPlanEntry {
    pub id: i64,
    pub date: String,
    pub slot: String,
    pub recipe_id: Option<i64>,
    pub title: String,
    pub text: String,
    pub batch_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ShoppingListSummary {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ShoppingItem {
    pub id: i64,
    pub qty: Option<f64>,
    /// Equal to `qty` unless the amount is a range.
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    pub food_id: Option<i64>,
    pub note: String,
    pub checked: bool,
    pub recipe_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ShoppingList {
    pub id: i64,
    pub name: String,
    pub items: Vec<ShoppingItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Image {
    pub hash: String,
    pub mime: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comment {
    pub user_id: Option<i64>,
    pub text: String,
    pub created_at: String,
}

impl Image {
    /// The file under the images dir: `<hash>.<ext>`.
    pub fn file_name(&self) -> String {
        let ext = match self.mime.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "jpg",
        };
        format!("{}.{ext}", self.hash)
    }
}

/// The values `users.cook_theme` accepts (its CHECK says the same).
pub const COOK_THEMES: [&str; 3] = ["normal", "high-contrast", "max-white"];

/// Column list `user_row` reads, in order. Table-qualified callers alias the table `users`.
pub(crate) const USER_COLS: &str = "users.id, users.username, users.email, users.full_name, users.role, users.oidc_issuer, users.oidc_subject, users.disabled, users.cook_theme";

pub(crate) fn user_row(r: &sqlx::sqlite::SqliteRow) -> User {
    User {
        id: r.get(0),
        username: r.get(1),
        email: r.get(2),
        full_name: r.get(3),
        role: r.get(4),
        oidc_issuer: r.get(5),
        oidc_subject: r.get(6),
        disabled: r.get(7),
        cook_theme: r.get(8),
    }
}

fn names(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

impl Db {
    pub async fn units(&self) -> Result<Vec<Unit>> {
        let rows = sqlx::query("SELECT id, name, aliases, dimension, factor FROM units ORDER BY id").fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                Ok(Unit {
                    id: r.get(0),
                    name: r.get(1),
                    aliases: names(r.get(2)),
                    dimension: serde_json::from_value(r.get::<String, _>(3).into())?,
                    factor: r.get(4),
                })
            })
            .collect()
    }

    pub async fn foods(&self) -> Result<Vec<Food>> {
        let rows = sqlx::query("SELECT id, name, aliases FROM foods ORDER BY id").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Food { id: r.get(0), name: r.get(1), aliases: names(r.get(2)) }).collect())
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        let rows = sqlx::query(&format!("SELECT {USER_COLS} FROM users ORDER BY id")).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(user_row).collect())
    }

    pub async fn set_cook_theme(&self, user_id: i64, theme: &str) -> Result<()> {
        if !COOK_THEMES.contains(&theme) {
            return Err(Error::Validation(format!("cook theme must be one of {}", COOK_THEMES.join(", "))));
        }
        let n = sqlx::query("UPDATE users SET cook_theme = ? WHERE id = ?").bind(theme).bind(user_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("user"));
        }
        Ok(())
    }

    /// Newest first; undated (planned) batches lead.
    pub async fn list_batches(&self, recipe_id: i64) -> Result<Vec<Batch>> {
        let rows = sqlx::query(
            "SELECT id, status, recipe_id, snapshot_id, source, made_on, scale, rating, notes FROM batches
             WHERE recipe_id = ? ORDER BY made_on IS NULL DESC, made_on DESC, id DESC",
        )
        .bind(recipe_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Batch {
                id: r.get(0),
                status: r.get(1),
                recipe_id: r.get(2),
                snapshot_id: r.get(3),
                source: r.get(4),
                made_on: r.get(5),
                scale: r.get(6),
                rating: r.get(7),
                notes: r.get(8),
            })
            .collect())
    }

    pub async fn comments(&self, recipe_id: i64) -> Result<Vec<Comment>> {
        let rows = sqlx::query("SELECT user_id, text, created_at FROM recipe_comments WHERE recipe_id = ? ORDER BY id")
            .bind(recipe_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| Comment { user_id: r.get(0), text: r.get(1), created_at: r.get(2) }).collect())
    }

    /// Entries with `from <= date <= to` (YYYY-MM-DD), by date then slot (`SLOTS` order; unknown imported slots last).
    pub async fn meal_plan(&self, from: &str, to: &str) -> Result<Vec<MealPlanEntry>> {
        let rows = sqlx::query(
            "SELECT id, date, slot, recipe_id, title, text, batch_id FROM meal_plan_entries
             WHERE date BETWEEN ? AND ?
             ORDER BY date, CASE slot WHEN 'breakfast' THEN 0 WHEN 'lunch' THEN 1 WHEN 'dinner' THEN 2 WHEN 'side' THEN 3
                                      WHEN 'snack' THEN 4 WHEN 'dessert' THEN 5 WHEN 'drink' THEN 6 ELSE 7 END, id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| MealPlanEntry {
                id: r.get(0),
                date: r.get(1),
                slot: r.get(2),
                recipe_id: r.get(3),
                title: r.get(4),
                text: r.get(5),
                batch_id: r.get(6),
            })
            .collect())
    }

    pub async fn shopping_lists(&self) -> Result<Vec<ShoppingListSummary>> {
        let rows = sqlx::query("SELECT id, name FROM shopping_lists ORDER BY id").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| ShoppingListSummary { id: r.get(0), name: r.get(1) }).collect())
    }

    pub async fn shopping_list(&self, id: i64) -> Result<ShoppingList> {
        let name: String = sqlx::query_scalar("SELECT name FROM shopping_lists WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("shopping list"))?;
        let rows = sqlx::query("SELECT id, qty, COALESCE(qty_max, qty), unit_id, food_id, note, checked, recipe_id FROM shopping_list_items WHERE list_id = ? ORDER BY id")
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        let items = rows
            .iter()
            .map(|r| ShoppingItem {
                id: r.get(0),
                qty: r.get(1),
                qty_max: r.get(2),
                unit_id: r.get(3),
                food_id: r.get(4),
                note: r.get(5),
                checked: r.get(6),
                recipe_id: r.get(7),
            })
            .collect();
        Ok(ShoppingList { id, name, items })
    }

    pub async fn recipe_images(&self, recipe_id: i64) -> Result<Vec<Image>> {
        let rows = sqlx::query("SELECT i.hash, i.mime FROM recipe_images r JOIN images i ON i.hash = r.hash WHERE r.recipe_id = ? ORDER BY r.position, i.hash")
            .bind(recipe_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| Image { hash: r.get(0), mime: r.get(1) }).collect())
    }
}
