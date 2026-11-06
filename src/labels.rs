//! Labels: group-scoped names with a colour that sort shopping-list items into sections.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sqlx::Row;

use crate::{Db, Error, Result, Scope, ShoppingItem, ShoppingList, ShoppingSection};

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Label {
    pub id: i64,
    pub name: String,
    /// `#rrggbb`, lowercase.
    pub color: String,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(default)]
pub struct LabelInput {
    pub name: String,
    /// `#rrggbb`
    pub color: String,
}

/// Trimmed name and lowercase `#rrggbb`.
fn check(input: LabelInput) -> Result<(String, String)> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(Error::Validation("label name must not be empty".into()));
    }
    let color = input.color.trim().to_lowercase();
    if color.len() != 7 || !color.starts_with('#') || !color[1..].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Validation("colour must be #rrggbb".into()));
    }
    Ok((name, color))
}

impl Db {
    /// By name.
    pub async fn labels(&self, sc: &Scope) -> Result<Vec<Label>> {
        let rows = sqlx::query("SELECT id, name, color FROM labels WHERE group_id = ? ORDER BY name").bind(sc.group_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Label { id: r.get(0), name: r.get(1), color: r.get(2) }).collect())
    }

    pub async fn add_label(&self, sc: &Scope, input: LabelInput) -> Result<Label> {
        let (name, color) = check(input)?;
        self.name_free(sc, &name, 0).await?;
        let id = sqlx::query("INSERT INTO labels (group_id, name, color) VALUES (?, ?, ?)").bind(sc.group_id).bind(&name).bind(&color).execute(&self.pool).await?.last_insert_rowid();
        Ok(Label { id, name, color })
    }

    pub async fn update_label(&self, sc: &Scope, id: i64, input: LabelInput) -> Result<Label> {
        self.label(sc, id).await?;
        let (name, color) = check(input)?;
        self.name_free(sc, &name, id).await?;
        sqlx::query("UPDATE labels SET name = ?, color = ? WHERE id = ? AND group_id = ?").bind(&name).bind(&color).bind(id).bind(sc.group_id).execute(&self.pool).await?;
        Ok(Label { id, name, color })
    }

    /// Foods and items that had it lose it; lists forget its place.
    pub async fn delete_label(&self, sc: &Scope, id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM labels WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("label"));
        }
        Ok(())
    }

    /// `None` clears it.
    pub async fn set_food_label(&self, sc: &Scope, food_id: i64, label_id: Option<i64>) -> Result<()> {
        if let Some(l) = label_id {
            self.label(sc, l).await?;
        }
        let n = sqlx::query("UPDATE foods SET label_id = ? WHERE id = ? AND group_id = ?").bind(label_id).bind(food_id).bind(sc.group_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("food"));
        }
        Ok(())
    }

    pub async fn food_label(&self, sc: &Scope, food_id: i64) -> Result<Option<Label>> {
        let found: Option<Option<i64>> = sqlx::query_scalar("SELECT label_id FROM foods WHERE id = ? AND group_id = ?").bind(food_id).bind(sc.group_id).fetch_optional(&self.pool).await?;
        match found.ok_or(Error::NotFound("food"))? {
            Some(l) => Ok(Some(self.label(sc, l).await?)),
            None => Ok(None),
        }
    }

    /// Puts the named labels first, in the given order; any others follow by name.
    pub async fn set_section_order(&self, sc: &Scope, list_id: i64, label_ids: &[i64]) -> Result<ShoppingList> {
        self.list_exists(sc, list_id).await?;
        if label_ids.iter().enumerate().any(|(i, l)| label_ids[..i].contains(l)) {
            return Err(Error::Validation("a label can be listed once".into()));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM shopping_list_labels WHERE list_id = ?").bind(list_id).execute(&mut *tx).await?;
        for (pos, l) in label_ids.iter().enumerate() {
            if sqlx::query("SELECT 1 FROM labels WHERE id = ? AND group_id = ?").bind(l).bind(sc.group_id).fetch_optional(&mut *tx).await?.is_none() {
                return Err(Error::NotFound("label"));
            }
            sqlx::query("INSERT INTO shopping_list_labels (list_id, label_id, position) VALUES (?, ?, ?)").bind(list_id).bind(l).bind(pos as i64).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.shopping_list(sc, list_id).await
    }

    /// Moves a non-empty section one place up or down among the non-empty ones.
    pub async fn move_section(&self, sc: &Scope, list_id: i64, label_id: i64, up: bool) -> Result<()> {
        self.label(sc, label_id).await?;
        let shown: Vec<i64> = self.shopping_list(sc, list_id).await?.sections.iter().filter_map(|s| s.label.as_ref().map(|l| l.id)).collect();
        let Some(at) = shown.iter().position(|l| *l == label_id) else { return Ok(()) };
        let other = if up { at.checked_sub(1) } else { Some(at + 1).filter(|n| *n < shown.len()) };
        let Some(other) = other.map(|o| shown[o]) else { return Ok(()) };
        let mut order = self.section_order(sc, list_id).await?;
        let (a, b) = (order.iter().position(|l| *l == label_id).unwrap(), order.iter().position(|l| *l == other).unwrap());
        order.swap(a, b);
        self.set_section_order(sc, list_id, &order).await.map(|_| ())
    }

    async fn label(&self, sc: &Scope, id: i64) -> Result<Label> {
        let r = sqlx::query("SELECT id, name, color FROM labels WHERE id = ? AND group_id = ?").bind(id).bind(sc.group_id).fetch_optional(&self.pool).await?.ok_or(Error::NotFound("label"))?;
        Ok(Label { id: r.get(0), name: r.get(1), color: r.get(2) })
    }

    async fn name_free(&self, sc: &Scope, name: &str, except: i64) -> Result<()> {
        if sqlx::query("SELECT 1 FROM labels WHERE group_id = ? AND name = ? AND id != ?").bind(sc.group_id).bind(name).bind(except).fetch_optional(&self.pool).await?.is_some() {
            return Err(Error::Validation(format!("a label called {name:?} exists")));
        }
        Ok(())
    }

    /// Every group label's id in this list's section order: its positions, then the rest by name.
    async fn section_order(&self, sc: &Scope, list_id: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT l.id FROM labels l LEFT JOIN shopping_list_labels p ON p.list_id = ? AND p.label_id = l.id
             WHERE l.group_id = ? ORDER BY p.position IS NULL, p.position, l.name",
        )
        .bind(list_id)
        .bind(sc.group_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// `items` grouped by effective label, in section order, unlabelled last; empty sections left out.
    pub(crate) async fn sections(&self, sc: &Scope, list_id: i64, items: &[ShoppingItem]) -> Result<Vec<ShoppingSection>> {
        let food_labels: std::collections::HashMap<i64, i64> = sqlx::query_as::<_, (i64, i64)>(
            "SELECT i.id, f.label_id FROM shopping_list_items i JOIN foods f ON f.id = i.food_id WHERE i.list_id = ? AND f.label_id IS NOT NULL",
        )
        .bind(list_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect();
        let labels = self.labels(sc).await?;
        let mut out = vec![];
        for id in self.section_order(sc, list_id).await? {
            let items: Vec<_> = items.iter().filter(|i| i.label_id.or(food_labels.get(&i.id).copied()) == Some(id)).cloned().collect();
            if !items.is_empty() {
                out.push(ShoppingSection { label: labels.iter().find(|l| l.id == id).cloned(), items });
            }
        }
        let rest: Vec<_> = items.iter().filter(|i| i.label_id.or(food_labels.get(&i.id).copied()).is_none()).cloned().collect();
        if !rest.is_empty() {
            out.push(ShoppingSection { label: None, items: rest });
        }
        Ok(out)
    }
}
