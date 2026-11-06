//! Sessions and API tokens (S25). Only SHA-256 hashes are stored.
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use utoipa::ToSchema;

use crate::people::{USER_COLS, user_row};
use crate::{Db, Error, Result, User};

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "mealier_session";

/// Sessions last this long.
pub const SESSION_DAYS: u32 = 7;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// Hex SHA-256 of a secret, as stored in `sessions.id_hash` and `api_tokens.hash`.
pub fn hash_token(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// `prefix` plus 32 random bytes from the OS, as hex.
pub(crate) fn random_secret(prefix: &str) -> Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(format!("{prefix}{}", b.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// An API token as listed: never carries the secret.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ApiToken {
    pub id: i64,
    pub name: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

impl Db {
    /// New session for the user; returns the cookie value (shown only here).
    pub async fn create_session(&self, user_id: i64) -> Result<String> {
        let cookie = random_secret("")?;
        // Expired rows are only ever dead weight; sweep them whenever one is made.
        sqlx::query(&format!("DELETE FROM sessions WHERE expires_at <= {NOW}")).execute(&self.pool).await?;
        sqlx::query(&format!("INSERT INTO sessions (id_hash, user_id, expires_at) VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+{SESSION_DAYS} days'))"))
            .bind(hash_token(&cookie))
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(cookie)
    }

    /// The enabled user a live session belongs to.
    pub async fn session_user(&self, cookie: &str) -> Result<Option<User>> {
        let row = sqlx::query(&format!(
            "SELECT {USER_COLS} FROM sessions JOIN users ON users.id = sessions.user_id
             WHERE sessions.id_hash = ? AND sessions.expires_at > {NOW} AND users.disabled = 0"
        ))
        .bind(hash_token(cookie))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(user_row))
    }

    pub async fn delete_session(&self, cookie: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id_hash = ?").bind(hash_token(cookie)).execute(&self.pool).await?;
        Ok(())
    }

    /// New API token; returns its listing row and the secret (shown only here).
    pub async fn create_api_token(&self, user_id: i64, name: &str) -> Result<(ApiToken, String)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Validation("a token needs a name".into()));
        }
        let secret = random_secret("mlr_")?;
        let row = sqlx::query("INSERT INTO api_tokens (hash, user_id, name) VALUES (?, ?, ?) RETURNING id, name, created_at, last_used_at")
            .bind(hash_token(&secret))
            .bind(user_id)
            .bind(name)
            .fetch_one(&self.pool)
            .await?;
        Ok((token_row(&row), secret))
    }

    /// The enabled user a token belongs to; records the use.
    pub async fn token_user(&self, token: &str) -> Result<Option<User>> {
        let hash = hash_token(token);
        let row = sqlx::query(&format!(
            "SELECT {USER_COLS} FROM api_tokens JOIN users ON users.id = api_tokens.user_id
             WHERE api_tokens.hash = ? AND users.disabled = 0"
        ))
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        sqlx::query(&format!("UPDATE api_tokens SET last_used_at = {NOW} WHERE hash = ?")).bind(&hash).execute(&self.pool).await?;
        Ok(Some(user_row(&row)))
    }

    pub async fn api_tokens(&self, user_id: i64) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query("SELECT id, name, created_at, last_used_at FROM api_tokens WHERE user_id = ? ORDER BY id").bind(user_id).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(token_row).collect())
    }

    /// Revokes one of the user's own tokens.
    pub async fn revoke_api_token(&self, user_id: i64, token_id: i64) -> Result<()> {
        let n = sqlx::query("DELETE FROM api_tokens WHERE id = ? AND user_id = ?").bind(token_id).bind(user_id).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("token"));
        }
        Ok(())
    }

    /// The break-glass admin, created on first use. Never bound to OIDC (a trigger enforces it).
    pub async fn local_admin(&self) -> Result<User> {
        let sql = format!("SELECT {USER_COLS} FROM users WHERE local_admin = 1");
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(r) = sqlx::query(&sql).fetch_optional(&mut *tx).await? {
            return Ok(user_row(&r));
        }
        // 'admin' may belong to a user imported from Mealie; never take that account over.
        let taken: Option<i64> = sqlx::query_scalar("SELECT 1 FROM users WHERE username = 'admin'").fetch_optional(&mut *tx).await?;
        let name = if taken.is_some() { "local-admin" } else { "admin" };
        let home = crate::tenancy::default_household_in(&mut tx).await?;
        sqlx::query("INSERT INTO users (username, email, role, local_admin, household_id) VALUES (?, 'local-admin@mealier.invalid', 'admin', 1, ?)").bind(name).bind(home.id).execute(&mut *tx).await?;
        let r = sqlx::query(&sql).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(user_row(&r))
    }

    pub async fn user(&self, id: i64) -> Result<User> {
        let row = sqlx::query(&format!("SELECT {USER_COLS} FROM users WHERE id = ?")).bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(user_row).ok_or(Error::NotFound("user"))
    }

    /// Disabling deletes every session and token of the user in the same transaction.
    pub async fn set_user_disabled(&self, id: i64, disabled: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query("UPDATE users SET disabled = ? WHERE id = ?").bind(disabled).bind(id).execute(&mut *tx).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("user"));
        }
        if disabled {
            sqlx::query("DELETE FROM sessions WHERE user_id = ?").bind(id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM api_tokens WHERE user_id = ?").bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn token_row(r: &sqlx::sqlite::SqliteRow) -> ApiToken {
    ApiToken { id: r.get(0), name: r.get(1), created_at: r.get(2), last_used_at: r.get(3) }
}

/// Who the identity provider says is signing in, after its token and the group rules were checked.
pub struct OidcIdentity {
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub name: String,
    pub username: String,
    /// Member of the admin group on this login.
    pub admin: bool,
}

impl Db {
    /// The user an OIDC login is. Looks up (issuer, subject); failing that links the one unbound user with
    /// the same email (case-insensitively); failing that creates one. Refuses, with `Forbidden`, when the
    /// email belongs to several users, to an identity already bound elsewhere, or to the local admin, and
    /// when the user is disabled. Admin is set or cleared from `id.admin` every time.
    pub async fn oidc_login(&self, id: &OidcIdentity) -> Result<User> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let role = if id.admin { "admin" } else { "user" };
        let refuse = |m: &str| Err(Error::Forbidden(m.into()));

        let bound: Option<(i64, bool)> = sqlx::query_as("SELECT id, disabled FROM users WHERE oidc_issuer = ? AND oidc_subject = ?").bind(&id.issuer).bind(&id.subject).fetch_optional(&mut *tx).await?;
        let uid = match bound {
            Some((_, true)) => return refuse("This account is disabled."),
            Some((uid, false)) => {
                sqlx::query("UPDATE users SET role = ? WHERE id = ?").bind(role).bind(uid).execute(&mut *tx).await?;
                uid
            }
            None => {
                let same: Vec<(i64, Option<String>, bool, bool)> = sqlx::query_as("SELECT id, oidc_issuer, disabled, local_admin FROM users WHERE lower(email) = lower(?)").bind(&id.email).fetch_all(&mut *tx).await?;
                match same.as_slice() {
                    [] => {
                        let name = free_username(&mut tx, &id.username).await?;
                        let home = crate::tenancy::default_household_in(&mut tx).await?;
                        sqlx::query_scalar("INSERT INTO users (username, email, full_name, role, oidc_issuer, oidc_subject, household_id) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id")
                            .bind(name)
                            .bind(&id.email)
                            .bind(&id.name)
                            .bind(role)
                            .bind(&id.issuer)
                            .bind(&id.subject)
                            .bind(home.id)
                            .fetch_one(&mut *tx)
                            .await?
                    }
                    [(uid, None, false, false)] => {
                        sqlx::query("UPDATE users SET oidc_issuer = ?, oidc_subject = ?, role = ? WHERE id = ?").bind(&id.issuer).bind(&id.subject).bind(role).bind(uid).execute(&mut *tx).await?;
                        *uid
                    }
                    [(_, None, true, false)] => return refuse("This account is disabled."),
                    _ => return refuse("An account with your email address exists but cannot be linked automatically. Ask an admin."),
                }
            }
        };
        let row = sqlx::query(&format!("SELECT {USER_COLS} FROM users WHERE id = ?")).bind(uid).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(user_row(&row))
    }
}

/// `want`, or `want-2`, `want-3`... whichever no user has.
async fn free_username(tx: &mut sqlx::SqliteConnection, want: &str) -> Result<String> {
    for n in 1..100 {
        let name = if n == 1 { want.to_string() } else { format!("{want}-{n}") };
        let taken: Option<i64> = sqlx::query_scalar("SELECT 1 FROM users WHERE username = ?").bind(&name).fetch_optional(&mut *tx).await?;
        if taken.is_none() {
            return Ok(name);
        }
    }
    Err(Error::Forbidden("No free username for your account. Ask an admin.".into()))
}

/// Shortest break-glass password `hash_password` accepts.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Argon2id PHC string for the break-glass admin password (`mealier hash-password`).
pub fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::PasswordHasher;
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(Error::Validation(format!("the password must be at least {MIN_PASSWORD_LEN} characters")));
    }
    let hash = argon2::Argon2::default().hash_password(password.as_bytes()).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(hash.to_string())
}

/// Does `password` match the PHC string? A string that does not parse matches nothing.
pub(crate) fn verify_password(phc: &str, password: &str) -> bool {
    use argon2::password_hash::{PasswordVerifier, phc::PasswordHash};
    PasswordHash::new(phc).is_ok_and(|h| argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}
