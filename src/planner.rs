//! Meal plan and shopping list writes.
use serde::Deserialize;
use utoipa::ToSchema;

use sqlx::Row;

use crate::batches::check_scale;
use crate::{Db, Error, MealPlanEntry, Result, Scope, ShoppingItem, ShoppingList, ShoppingListSummary};

/// Meal slots in display order (Mealie's entry types).
pub const SLOTS: [&str; 7] = ["breakfast", "lunch", "dinner", "side", "snack", "dessert", "drink"];

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(default)]
pub struct EntryInput {
    /// YYYY-MM-DD
    pub date: String,
    /// breakfast, lunch, dinner, side, snack, dessert or drink
    pub slot: String,
    pub recipe_id: Option<i64>,
    /// Free-text entry when there is no recipe.
    pub title: String,
    pub text: String,
    /// A `planned` batch of `recipe_id`.
    pub batch_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(default)]
pub struct ItemInput {
    pub qty: Option<f64>,
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    pub food_id: Option<i64>,
    pub note: String,
    pub checked: bool,
}

async fn check_entry(db: &Db, sc: &Scope, i: &EntryInput) -> Result<()> {
    if !SLOTS.contains(&i.slot.as_str()) {
        return Err(Error::Validation(format!("slot must be one of {}", SLOTS.join(", "))));
    }
    let ok: Option<String> = sqlx::query_scalar("SELECT date(?)").bind(&i.date).fetch_one(&db.pool).await?;
    if ok.as_deref() != Some(i.date.as_str()) {
        return Err(Error::Validation("date must be YYYY-MM-DD".into()));
    }
    if i.recipe_id.is_none() && i.title.trim().is_empty() {
        return Err(Error::Validation("an entry needs a recipe or a title".into()));
    }
    if let Some(r) = i.recipe_id {
        db.get_recipe(sc, r).await?.ok_or(Error::NotFound("recipe"))?;
    }
    if let Some(b) = i.batch_id {
        let b = db.get_batch(sc, b).await?.batch;
        if b.status != "planned" || Some(b.recipe_id) != i.recipe_id {
            return Err(Error::Validation("batch must be a planned batch of the entry's recipe".into()));
        }
    }
    Ok(())
}

impl Db {
    pub async fn add_entry(&self, sc: &Scope, input: EntryInput) -> Result<MealPlanEntry> {
        check_entry(self, sc, &input).await?;
        let id = sqlx::query("INSERT INTO meal_plan_entries (household_id, date, slot, recipe_id, title, text, batch_id) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(sc.household_id)
            .bind(&input.date)
            .bind(&input.slot)
            .bind(input.recipe_id)
            .bind(&input.title)
            .bind(&input.text)
            .bind(input.batch_id)
            .execute(&self.pool)
            .await?
            .last_insert_rowid();
        self.entry(sc, id).await
    }

    pub async fn update_entry(&self, sc: &Scope, id: i64, input: EntryInput) -> Result<MealPlanEntry> {
        self.entry(sc, id).await?;
        check_entry(self, sc, &input).await?;
        sqlx::query("UPDATE meal_plan_entries SET date = ?, slot = ?, recipe_id = ?, title = ?, text = ?, batch_id = ? WHERE id = ? AND household_id = ?")
            .bind(&input.date)
            .bind(&input.slot)
            .bind(input.recipe_id)
            .bind(&input.title)
            .bind(&input.text)
            .bind(input.batch_id)
            .bind(id)
            .bind(sc.household_id)
            .execute(&self.pool)
            .await?;
        self.entry(sc, id).await
    }

    pub async fn delete_entry(&self, sc: &Scope, id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM meal_plan_entries WHERE id = ? AND household_id = ?").bind(id).bind(sc.household_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("meal plan entry"));
        }
        Ok(())
    }

    async fn entry(&self, sc: &Scope, id: i64) -> Result<MealPlanEntry> {
        let r = sqlx::query("SELECT id, date, slot, recipe_id, title, text, batch_id FROM meal_plan_entries WHERE id = ? AND household_id = ?")
            .bind(id)
            .bind(sc.household_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("meal plan entry"))?;
        Ok(MealPlanEntry { id: r.get(0), date: r.get(1), slot: r.get(2), recipe_id: r.get(3), title: r.get(4), text: r.get(5), batch_id: r.get(6) })
    }
    pub async fn create_list(&self, sc: &Scope, name: &str) -> Result<ShoppingListSummary> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Validation("list name must not be empty".into()));
        }
        let id = sqlx::query("INSERT INTO shopping_lists (household_id, name) VALUES (?, ?)").bind(sc.household_id).bind(name).execute(&self.pool).await?.last_insert_rowid();
        Ok(ShoppingListSummary { id, name: name.into() })
    }

    pub async fn add_item(&self, sc: &Scope, list_id: i64, input: ItemInput) -> Result<ShoppingItem> {
        let line = check_item(self, sc, input).await?;
        self.list_exists(sc, list_id).await?;
        let id = insert_item(&mut *self.pool.acquire().await?, list_id, &line, None).await?;
        self.item(sc, id).await
    }

    pub async fn update_item(&self, sc: &Scope, id: i64, input: ItemInput) -> Result<ShoppingItem> {
        self.item(sc, id).await?;
        let l = check_item(self, sc, input).await?;
        sqlx::query("UPDATE shopping_list_items SET qty = ?, qty_max = ?, unit_id = ?, food_id = ?, note = ?, checked = ? WHERE id = ? AND list_id IN (SELECT id FROM shopping_lists WHERE household_id = ?)")
            .bind(l.qty)
            .bind(l.qty_max)
            .bind(l.unit_id)
            .bind(l.food_id)
            .bind(&l.note)
            .bind(l.checked)
            .bind(id)
            .bind(sc.household_id)
            .execute(&self.pool)
            .await?;
        self.item(sc, id).await
    }

    pub async fn set_item_checked(&self, sc: &Scope, id: i64, checked: bool) -> Result<()> {
        let n = sqlx::query("UPDATE shopping_list_items SET checked = ? WHERE id = ? AND list_id IN (SELECT id FROM shopping_lists WHERE household_id = ?)").bind(checked).bind(id).bind(sc.household_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("shopping list item"));
        }
        Ok(())
    }

    pub async fn delete_item(&self, sc: &Scope, id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM shopping_list_items WHERE id = ? AND list_id IN (SELECT id FROM shopping_lists WHERE household_id = ?)").bind(id).bind(sc.household_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("shopping list item"));
        }
        Ok(())
    }

    pub async fn add_recipe_to_list(&self, sc: &Scope, list_id: i64, recipe_id: i64, scale: f64) -> Result<ShoppingList> {
        check_scale(scale)?;
        self.list_exists(sc, list_id).await?;
        let r = self.get_recipe(sc, recipe_id).await?.ok_or(Error::NotFound("recipe"))?;
        let lines = r.content.ingredients.iter().map(|i| ItemInput {
            qty: i.qty_min.map(|q| q * scale),
            qty_max: i.qty_max.map(|q| q * scale),
            unit_id: i.unit_id,
            food_id: i.food_id,
            note: if i.food_id.is_some() { i.note.clone() } else { i.original_text.clone() },
            checked: false,
        });
        self.merge_lines(sc, list_id, lines.collect(), recipe_id).await
    }

    pub async fn add_batch_to_list(&self, sc: &Scope, list_id: i64, batch_id: i64) -> Result<ShoppingList> {
        self.list_exists(sc, list_id).await?;
        let b = self.get_batch(sc, batch_id).await?;
        let lines = b.lines.iter().map(|l| ItemInput {
            qty: l.qty_min,
            qty_max: l.qty_max,
            unit_id: l.unit_id,
            food_id: l.substitute_food_id.or(l.food_id),
            note: if l.substitute_food_id.or(l.food_id).is_some() { String::new() } else { l.ingredient.clone() },
            checked: false,
        });
        self.merge_lines(sc, list_id, lines.collect(), b.batch.recipe_id).await
    }

    /// Adds each line to the list, folding it into an unchecked item of the same food when the units agree.
    async fn merge_lines(&self, sc: &Scope, list_id: i64, lines: Vec<ItemInput>, recipe_id: i64) -> Result<ShoppingList> {
        let mut tx = self.pool.begin().await?;
        for l in lines {
            let l = ItemInput { qty_max: l.qty_max.or(l.qty), ..l };
            let into = match (l.food_id, l.qty) {
                (Some(food), Some(_)) => find_merge(&mut tx, sc, list_id, food, &l).await?,
                _ => None,
            };
            match into {
                Some((id, min, max)) => {
                    sqlx::query("UPDATE shopping_list_items SET qty = ?, qty_max = ? WHERE id = ? AND list_id = ?").bind(min).bind(max).bind(id).bind(list_id).execute(&mut *tx).await?;
                }
                None => {
                    insert_item(&mut tx, list_id, &l, Some(recipe_id)).await?;
                }
            }
        }
        tx.commit().await?;
        self.shopping_list(sc, list_id).await
    }

    /// The list an item is on.
    pub(crate) async fn item_list_id(&self, sc: &Scope, item: i64) -> Result<i64> {
        sqlx::query_scalar("SELECT list_id FROM shopping_list_items WHERE id = ? AND list_id IN (SELECT id FROM shopping_lists WHERE household_id = ?)").bind(item).bind(sc.household_id).fetch_optional(&self.pool).await?.ok_or(Error::NotFound("shopping list item"))
    }

    async fn list_exists(&self, sc: &Scope, id: i64) -> Result<()> {
        self.shopping_list(sc, id).await.map(|_| ())
    }

    async fn item(&self, sc: &Scope, id: i64) -> Result<ShoppingItem> {
        let r = sqlx::query("SELECT id, qty, COALESCE(qty_max, qty), unit_id, food_id, note, checked, recipe_id FROM shopping_list_items WHERE id = ? AND list_id IN (SELECT id FROM shopping_lists WHERE household_id = ?)")
            .bind(id)
            .bind(sc.household_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("shopping list item"))?;
        Ok(ShoppingItem { id: r.get(0), qty: r.get(1), qty_max: r.get(2), unit_id: r.get(3), food_id: r.get(4), note: r.get(5), checked: r.get(6), recipe_id: r.get(7) })
    }
}

/// A range per `recipes::range`, known units and foods, and an item with something to buy.
async fn check_item(db: &Db, sc: &Scope, mut i: ItemInput) -> Result<ItemInput> {
    (i.qty, i.qty_max) = crate::recipes::range(i.qty, i.qty_max)?;
    if i.food_id.is_none() && i.note.trim().is_empty() {
        return Err(Error::Validation("an item needs a food or a note".into()));
    }
    for (table, name, id) in [("units", "unit", i.unit_id), ("foods", "food", i.food_id)] {
        if let Some(id) = id
            && sqlx::query(&format!("SELECT 1 FROM {table} WHERE id = ? AND group_id = ?")).bind(id).bind(sc.group_id).fetch_optional(&db.pool).await?.is_none()
        {
            return Err(Error::NotFound(name));
        }
    }
    Ok(i)
}

async fn insert_item(pool: &mut sqlx::SqliteConnection, list_id: i64, l: &ItemInput, recipe_id: Option<i64>) -> Result<i64> {
    Ok(sqlx::query("INSERT INTO shopping_list_items (list_id, qty, qty_max, unit_id, food_id, note, checked, recipe_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(list_id)
        .bind(l.qty)
        .bind(l.qty_max)
        .bind(l.unit_id)
        .bind(l.food_id)
        .bind(&l.note)
        .bind(l.checked)
        .bind(recipe_id)
        .execute(pool)
        .await?
        .last_insert_rowid())
}

/// The first unchecked item of `food` that `l` can be added to, with the summed
/// range in that item's unit. Same unit adds directly; two mass or two volume
/// units convert through their factors; anything else (count, other, mass
/// against volume, a missing amount) does not merge.
async fn find_merge(tx: &mut sqlx::SqliteConnection, sc: &Scope, list_id: i64, food: i64, l: &ItemInput) -> Result<Option<(i64, f64, f64)>> {
    let (qty, qty_max) = (l.qty.unwrap_or_default(), l.qty_max.unwrap_or_default());
    let items = sqlx::query("SELECT id, qty, COALESCE(qty_max, qty), unit_id FROM shopping_list_items WHERE list_id = ? AND food_id = ? AND checked = 0 AND qty IS NOT NULL ORDER BY id")
        .bind(list_id)
        .bind(food)
        .fetch_all(&mut *tx)
        .await?;
    for r in items {
        let (id, min, max, unit): (i64, f64, f64, Option<i64>) = (r.get(0), r.get(1), r.get(2), r.get(3));
        let k = if unit == l.unit_id {
            1.0
        } else {
            let (Some(from), Some(to)) = (l.unit_id, unit) else { continue };
            let a = unit_dim(tx, sc, from).await?;
            let b = unit_dim(tx, sc, to).await?;
            match (a, b) {
                ((da, Some(fa)), (db, Some(fb))) if da == db && (da == "mass" || da == "volume") => fa / fb,
                _ => continue,
            }
        };
        return Ok(Some((id, min + qty * k, max + qty_max * k)));
    }
    Ok(None)
}

async fn unit_dim(tx: &mut sqlx::SqliteConnection, sc: &Scope, id: i64) -> Result<(String, Option<f64>)> {
    Ok(sqlx::query_as("SELECT dimension, factor FROM units WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).fetch_one(tx).await?)
}
