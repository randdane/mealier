//! Who is asking: the tenant a request may touch.
use sqlx::{Row, SqliteConnection};

use crate::{Error, Result, User};

/// The signed-in user's tenancy, derived once per request by the guard.
/// Every store method that touches group- or household-scoped data takes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub user_id: i64,
    pub group_id: i64,
    pub household_id: i64,
    pub admin: bool,
}

impl From<&User> for Scope {
    fn from(u: &User) -> Scope {
        Scope { user_id: u.id, group_id: u.group_id, household_id: u.household_id, admin: u.role == "admin" }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct Household {
    pub id: i64,
    pub group_id: i64,
    pub name: String,
    pub slug: String,
}

const DEFAULT_KEY: &str = "default_household_id";

/// Lowercase ASCII letters and digits, runs of anything else become one `-`.
fn slugify(name: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() { fallback.into() } else { out.into() }
}

fn clean_name(name: &str, what: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(Error::Validation(format!("a {what} needs a name")));
    }
    Ok(n.into())
}

/// `base`, `base-2`, `base-3`... whichever `taken` says is free.
async fn free_slug(conn: &mut SqliteConnection, table: &str, scope: Option<i64>, base: &str) -> Result<String> {
    for n in 1..1000 {
        let slug = if n == 1 { base.to_string() } else { format!("{base}-{n}") };
        let sql = match scope {
            Some(_) => format!("SELECT 1 FROM {table} WHERE group_id = ? AND slug = ?"),
            None => format!("SELECT 1 FROM {table} WHERE slug = ?"),
        };
        let q = sqlx::query_scalar::<_, i64>(&sql);
        let q = if let Some(g) = scope { q.bind(g) } else { q };
        if q.bind(&slug).fetch_optional(&mut *conn).await?.is_none() {
            return Ok(slug);
        }
    }
    Err(Error::Validation(format!("no free {table} slug for '{base}'")))
}

pub(crate) async fn insert_group(conn: &mut SqliteConnection, name: &str, slug_hint: &str) -> Result<Group> {
    let name = clean_name(name, "group")?;
    let slug = free_slug(conn, "groups", None, &slugify(slug_hint, "group")).await?;
    let id: i64 = sqlx::query_scalar("INSERT INTO groups (name, slug) VALUES (?, ?) RETURNING id").bind(&name).bind(&slug).fetch_one(&mut *conn).await?;
    sqlx::query("INSERT INTO group_preferences (group_id) VALUES (?)").bind(id).execute(&mut *conn).await?;
    Ok(Group { id, name, slug })
}

pub(crate) async fn insert_household(conn: &mut SqliteConnection, group_id: i64, name: &str, slug_hint: &str) -> Result<Household> {
    let name = clean_name(name, "household")?;
    let known: Option<i64> = sqlx::query_scalar("SELECT 1 FROM groups WHERE id = ?").bind(group_id).fetch_optional(&mut *conn).await?;
    if known.is_none() {
        return Err(Error::NotFound("group"));
    }
    let slug = free_slug(conn, "households", Some(group_id), &slugify(slug_hint, "household")).await?;
    let id: i64 = sqlx::query_scalar("INSERT INTO households (group_id, name, slug) VALUES (?, ?, ?) RETURNING id").bind(group_id).bind(&name).bind(&slug).fetch_one(&mut *conn).await?;
    sqlx::query("INSERT INTO household_preferences (household_id) VALUES (?)").bind(id).execute(&mut *conn).await?;
    Ok(Household { id, group_id, name, slug })
}

fn household_row(r: &sqlx::sqlite::SqliteRow) -> Household {
    Household { id: r.get(0), group_id: r.get(1), name: r.get(2), slug: r.get(3) }
}

const HOUSEHOLD_COLS: &str = "id, group_id, name, slug";

async fn household(conn: &mut SqliteConnection, id: i64) -> Result<Household> {
    let r = sqlx::query(&format!("SELECT {HOUSEHOLD_COLS} FROM households WHERE id = ?")).bind(id).fetch_optional(&mut *conn).await?;
    r.as_ref().map(household_row).ok_or(Error::NotFound("household"))
}

/// The household new users join. Made ("Home" in "Home") when none was chosen. Runs inside the
/// caller's write transaction, so two first logins cannot both make one.
pub(crate) async fn default_household_in(conn: &mut SqliteConnection) -> Result<Household> {
    let chosen: Option<String> = sqlx::query_scalar("SELECT value FROM instance_settings WHERE key = ?").bind(DEFAULT_KEY).fetch_optional(&mut *conn).await?;
    if let Some(id) = chosen.and_then(|v| v.parse::<i64>().ok())
        && let Ok(h) = household(conn, id).await
    {
        return Ok(h);
    }
    let group: Option<i64> = sqlx::query_scalar("SELECT id FROM groups WHERE slug = 'home'").fetch_optional(&mut *conn).await?;
    let group = match group {
        Some(g) => g,
        None => insert_group(conn, "Home", "home").await?.id,
    };
    let existing = sqlx::query(&format!("SELECT {HOUSEHOLD_COLS} FROM households WHERE group_id = ? AND slug = 'home'")).bind(group).fetch_optional(&mut *conn).await?;
    let home = match existing {
        Some(r) => household_row(&r),
        None => insert_household(conn, group, "Home", "home").await?,
    };
    set_default(conn, home.id).await?;
    Ok(home)
}

pub(crate) async fn set_default(conn: &mut SqliteConnection, household_id: i64) -> Result<()> {
    sqlx::query("INSERT INTO instance_settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
        .bind(DEFAULT_KEY)
        .bind(household_id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

impl crate::Db {
    pub async fn create_group(&self, name: &str) -> Result<Group> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let g = insert_group(&mut tx, name, name).await?;
        tx.commit().await?;
        Ok(g)
    }

    pub async fn create_household(&self, group_id: i64, name: &str) -> Result<Household> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let h = insert_household(&mut tx, group_id, name, name).await?;
        tx.commit().await?;
        Ok(h)
    }

    pub async fn groups(&self) -> Result<Vec<Group>> {
        let rows = sqlx::query("SELECT id, name, slug FROM groups ORDER BY id").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| Group { id: r.get(0), name: r.get(1), slug: r.get(2) }).collect())
    }

    pub async fn households(&self) -> Result<Vec<Household>> {
        let rows = sqlx::query(&format!("SELECT {HOUSEHOLD_COLS} FROM households ORDER BY id")).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(household_row).collect())
    }

    /// The household new users join; "Home" in "Home" is made when none was chosen.
    pub async fn default_household(&self) -> Result<Household> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let h = default_household_in(&mut tx).await?;
        tx.commit().await?;
        Ok(h)
    }

    pub async fn set_default_household(&self, id: i64) -> Result<Household> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let h = household(&mut tx, id).await?;
        set_default(&mut tx, id).await?;
        tx.commit().await?;
        Ok(h)
    }

    /// Moves a user to another household, in any group. Their old household keeps its data; the guard
    /// derives the scope from this row on every request, so the move takes effect at once.
    pub async fn move_user(&self, user_id: i64, household_id: i64) -> Result<User> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        household(&mut tx, household_id).await?;
        let n = sqlx::query("UPDATE users SET household_id = ? WHERE id = ?").bind(household_id).bind(user_id).execute(&mut *tx).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("user"));
        }
        tx.commit().await?;
        self.user(user_id).await
    }
}
