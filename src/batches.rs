//! The batch log: one making of a recipe, with the amounts actually used.
use std::path::Path;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sqlx::{Row, Sqlite, Transaction};

use crate::{Batch, Db, Error, Image, Result, Scope};

/// One row of the batch table. `recipe_*` is the snapshot's amount times the
/// batch scale (empty for added lines); the rest is what was actually used.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct BatchLine {
    pub id: i64,
    pub added: bool,
    pub ingredient: String,
    pub recipe_qty_min: Option<f64>,
    pub recipe_qty_max: Option<f64>,
    pub recipe_unit_name: Option<String>,
    pub qty_min: Option<f64>,
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    pub unit_name: Option<String>,
    pub food_id: Option<i64>,
    pub food_name: Option<String>,
    pub substitution: String,
    pub substitute_food_id: Option<i64>,
    pub substitute_food_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct BatchDetail {
    #[serde(flatten)]
    pub batch: Batch,
    pub lines: Vec<BatchLine>,
    pub photos: Vec<Image>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct BatchLineInput {
    /// Existing line to update; absent adds a new `added` line.
    pub id: Option<i64>,
    pub qty_min: Option<f64>,
    pub qty_max: Option<f64>,
    pub unit_id: Option<i64>,
    /// Only used for added lines; a snapshot line keeps its food.
    pub food_id: Option<i64>,
    pub substitution: String,
    pub substitute_food_id: Option<i64>,
}

/// A full replacement: added lines missing from `lines` are removed; every
/// snapshot line must be present.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct BatchInput {
    /// YYYY-MM-DD
    pub made_on: Option<String>,
    pub scale: f64,
    /// 1 to 5
    pub rating: Option<i64>,
    pub notes: String,
    pub lines: Vec<BatchLineInput>,
}

const MAX_PHOTOS: i64 = 5;
const MAX_PHOTO_BYTES: usize = 10 * 1024 * 1024;

pub(crate) fn check_scale(scale: f64) -> Result<()> {
    if scale.is_finite() && scale > 0.0 {
        return Ok(());
    }
    Err(Error::Validation("scale must be greater than 0".into()))
}

fn normalise(l: &mut BatchLineInput) -> Result<()> {
    (l.qty_min, l.qty_max) = crate::recipes::range(l.qty_min, l.qty_max)?;
    Ok(())
}

async fn check_refs(tx: &mut Transaction<'_, Sqlite>, sc: &Scope, l: &BatchLineInput) -> Result<()> {
    for (table, name, id) in [("units", "unit", l.unit_id), ("foods", "food", l.food_id), ("foods", "food", l.substitute_food_id)] {
        if let Some(id) = id {
            let found = sqlx::query(&format!("SELECT 1 FROM {table} WHERE id = ? AND group_id = ?")).bind(id).bind(sc.group_id).fetch_optional(&mut **tx).await?;
            if found.is_none() {
                return Err(Error::NotFound(name));
            }
        }
    }
    Ok(())
}

async fn insert_added(tx: &mut Transaction<'_, Sqlite>, batch_id: i64, l: &BatchLineInput) -> Result<()> {
    sqlx::query(
        "INSERT INTO batch_lines (batch_id, qty_min, qty_max, unit_id, food_id, substitution, substitute_food_id, added)
         VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
    )
    .bind(batch_id)
    .bind(l.qty_min)
    .bind(l.qty_max)
    .bind(l.unit_id)
    .bind(l.food_id)
    .bind(&l.substitution)
    .bind(l.substitute_food_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Db {
    pub async fn start_batch(&self, sc: &Scope, recipe_id: i64, scale: f64) -> Result<BatchDetail> {
        check_scale(scale)?;
        let recipe = self.get_recipe(sc, recipe_id).await?.ok_or(Error::NotFound("recipe"))?;
        let mut tx = self.pool.begin().await?;
        let id = sqlx::query("INSERT INTO batches (household_id, status, recipe_id, snapshot_id, source, scale) VALUES (?, 'planned', ?, ?, 'native', ?)")
            .bind(sc.household_id)
            .bind(recipe_id)
            .bind(recipe.current_snapshot_id)
            .bind(scale)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        for (pos, i) in recipe.content.ingredients.iter().enumerate() {
            sqlx::query(
                "INSERT INTO batch_lines (batch_id, qty_min, qty_max, unit_id, food_id, position) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(i.qty_min.map(|q| q * scale))
            .bind(i.qty_max.map(|q| q * scale))
            .bind(i.unit_id)
            .bind(i.food_id)
            .bind(pos as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.get_batch(sc, id).await
    }

    pub async fn get_batch(&self, sc: &Scope, id: i64) -> Result<BatchDetail> {
        let row = sqlx::query("SELECT id, status, recipe_id, snapshot_id, source, made_on, scale, rating, notes FROM batches WHERE id = ? AND household_id = ?")
            .bind(id)
            .bind(sc.household_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound("batch"))?;
        let batch = Batch {
            id: row.get(0),
            status: row.get(1),
            recipe_id: row.get(2),
            snapshot_id: row.get(3),
            source: row.get(4),
            made_on: row.get(5),
            scale: row.get(6),
            rating: row.get(7),
            notes: row.get(8),
        };
        let snapshot = match batch.snapshot_id {
            Some(s) => Some(self.snapshot(sc, s).await?.content),
            None => None,
        };
        let rows = sqlx::query(
            "SELECT l.id, l.position, l.added, l.qty_min, l.qty_max, l.unit_id, u.name, l.food_id, f.name,
                    l.substitution, l.substitute_food_id, sf.name
             FROM batch_lines l
             LEFT JOIN units u ON u.id = l.unit_id AND u.group_id = ?1
             LEFT JOIN foods f ON f.id = l.food_id AND f.group_id = ?1
             LEFT JOIN foods sf ON sf.id = l.substitute_food_id AND sf.group_id = ?1
             WHERE l.batch_id = ?2 ORDER BY l.id",
        )
        .bind(sc.group_id)
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        let lines = rows
            .iter()
            .map(|r| {
                let from = r.get::<Option<i64>, _>(1).and_then(|p| snapshot.as_ref()?.ingredients.get(p as usize));
                let food_name: Option<String> = r.get(8);
                let ingredient = match from {
                    Some(i) => [i.food_name.as_deref(), Some(&i.note), Some(&i.original_text)]
                        .into_iter()
                        .flatten()
                        .find(|s| !s.is_empty())
                        .unwrap_or_default()
                        .to_string(),
                    None => food_name.clone().unwrap_or_default(),
                };
                BatchLine {
                    id: r.get(0),
                    added: r.get(2),
                    ingredient,
                    recipe_qty_min: from.and_then(|i| i.qty_min).map(|q| q * batch.scale),
                    recipe_qty_max: from.and_then(|i| i.qty_max).map(|q| q * batch.scale),
                    recipe_unit_name: from.and_then(|i| i.unit_name.clone()),
                    qty_min: r.get(3),
                    qty_max: r.get(4),
                    unit_id: r.get(5),
                    unit_name: r.get(6),
                    food_id: r.get(7),
                    food_name,
                    substitution: r.get(9),
                    substitute_food_id: r.get(10),
                    substitute_food_name: r.get(11),
                }
            })
            .collect();
        let photos = sqlx::query("SELECT i.hash, i.mime FROM batch_photos p JOIN images i ON i.hash = p.hash WHERE p.batch_id = ? ORDER BY p.position, p.rowid")
            .bind(id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| Image { hash: r.get(0), mime: r.get(1) })
            .collect();
        Ok(BatchDetail { batch, lines, photos })
    }
    pub async fn update_batch(&self, sc: &Scope, id: i64, input: BatchInput) -> Result<BatchDetail> {
        check_scale(input.scale)?;
        if input.rating.is_some_and(|r| !(1..=5).contains(&r)) {
            return Err(Error::Validation("rating must be 1 to 5".into()));
        }
        let mut tx = self.pool.begin().await?;
        if let Some(d) = &input.made_on {
            let ok = sqlx::query_scalar::<_, Option<String>>("SELECT date(?)").bind(d).fetch_one(&mut *tx).await?;
            if ok.as_deref() != Some(d) {
                return Err(Error::Validation("made_on must be a date as YYYY-MM-DD".into()));
            }
        }
        let n = sqlx::query("UPDATE batches SET made_on = ?, scale = ?, rating = ?, notes = ? WHERE id = ? AND household_id = ?")
            .bind(&input.made_on)
            .bind(input.scale)
            .bind(input.rating)
            .bind(&input.notes)
            .bind(id)
            .bind(sc.household_id)
            .execute(&mut *tx)
            .await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("batch"));
        }
        let existing: Vec<(i64, bool)> = sqlx::query_as("SELECT id, added FROM batch_lines WHERE batch_id = ?")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        let given: Vec<i64> = input.lines.iter().filter_map(|l| l.id).collect();
        if given.iter().any(|g| !existing.iter().any(|(e, _)| e == g)) {
            return Err(Error::NotFound("batch line"));
        }
        for (line_id, added) in &existing {
            if !given.contains(line_id) {
                if !added {
                    return Err(Error::Validation("a line from the recipe cannot be removed".into()));
                }
                sqlx::query("DELETE FROM batch_lines WHERE id = ? AND batch_id = ?").bind(line_id).bind(id).execute(&mut *tx).await?;
            }
        }
        for mut line in input.lines {
            normalise(&mut line)?;
            check_refs(&mut tx, sc, &line).await?;
            match line.id {
                // A snapshot line keeps its food; only added lines choose one.
                Some(lid) => {
                    sqlx::query(
                        "UPDATE batch_lines SET qty_min = ?, qty_max = ?, unit_id = ?, substitution = ?, substitute_food_id = ?,
                                food_id = CASE WHEN added THEN ? ELSE food_id END
                         WHERE id = ? AND batch_id = ?",
                    )
                    .bind(line.qty_min)
                    .bind(line.qty_max)
                    .bind(line.unit_id)
                    .bind(&line.substitution)
                    .bind(line.substitute_food_id)
                    .bind(line.food_id)
                    .bind(lid)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                }
                None => insert_added(&mut tx, id, &line).await?,
            }
        }
        tx.commit().await?;
        self.get_batch(sc, id).await
    }
    pub async fn add_batch_line(&self, sc: &Scope, id: i64, mut line: BatchLineInput) -> Result<BatchDetail> {
        normalise(&mut line)?;
        let mut tx = self.pool.begin().await?;
        if sqlx::query("SELECT 1 FROM batches WHERE id = ? AND household_id = ?").bind(id).bind(sc.household_id).fetch_optional(&mut *tx).await?.is_none() {
            return Err(Error::NotFound("batch"));
        }
        check_refs(&mut tx, sc, &line).await?;
        insert_added(&mut tx, id, &line).await?;
        tx.commit().await?;
        self.get_batch(sc, id).await
    }

    pub async fn finish_batch(&self, sc: &Scope, id: i64) -> Result<BatchDetail> {
        let n = sqlx::query("UPDATE batches SET status = 'done', made_on = COALESCE(made_on, date('now')) WHERE id = ? AND household_id = ? AND status = 'planned'")
            .bind(id)
            .bind(sc.household_id)
            .execute(&self.pool)
            .await?;
        if n.rows_affected() == 0 {
            self.get_batch(sc, id).await?;
            return Err(Error::Validation("batch is already done".into()));
        }
        let detail = self.get_batch(sc, id).await?;
        let b = &detail.batch;
        let recipe_name = self.get_recipe(sc, b.recipe_id).await.ok().flatten().map(|r| r.content.name).unwrap_or_default();
        self.notify(crate::Event::BatchLogged { batch_id: id, recipe_id: b.recipe_id, recipe_name, rating: b.rating, made_on: b.made_on.clone() });
        Ok(detail)
    }
    /// Stores one photo for the batch (png, jpeg or webp; see `images`).
    pub async fn add_batch_photo(&self, sc: &Scope, id: i64, images_dir: &Path, bytes: &[u8]) -> Result<Image> {
        if bytes.len() > MAX_PHOTO_BYTES {
            return Err(Error::Validation("photo must be at most 10 MB".into()));
        }
        let photo = crate::images::prepare(bytes).await?;
        let mut tx = self.pool.begin().await?;
        if sqlx::query("SELECT 1 FROM batches WHERE id = ? AND household_id = ?").bind(id).bind(sc.household_id).fetch_optional(&mut *tx).await?.is_none() {
            return Err(Error::NotFound("batch"));
        }
        let already = sqlx::query("SELECT 1 FROM batch_photos WHERE batch_id = ? AND hash = ?")
            .bind(id)
            .bind(&photo.hash)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM batch_photos WHERE batch_id = ?").bind(id).fetch_one(&mut *tx).await?;
        if !already && count >= MAX_PHOTOS {
            return Err(Error::Validation("a batch holds at most 5 photos".into()));
        }
        photo.write(images_dir)?;
        photo.record(&mut tx).await?;
        sqlx::query("INSERT OR IGNORE INTO batch_photos (batch_id, hash, position) VALUES (?, ?, ?)")
            .bind(id)
            .bind(&photo.hash)
            .bind(count)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(photo.image())
    }
}
