//! Shared by the HTTP tests: every route needs a user now, so this makes one.
#![allow(dead_code)]
use mealier::{Db, hash_token};

/// The `Authorization` header value of the suite's admin; request builders add it.
pub const BEARER: &str = "Bearer mlr_test-suite-token";

/// Creates the suite's admin user and API token in `db` (once; calling it again is harmless).
pub async fn sign_in(db: &Db) {
    let id: i64 = sqlx::query_scalar("INSERT INTO users (username, email, role) VALUES ('suite-admin', 'suite-admin@x', 'admin') ON CONFLICT (username) DO UPDATE SET role = 'admin' RETURNING id")
        .fetch_one(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT OR IGNORE INTO api_tokens (hash, user_id, name) VALUES (?, ?, 'suite')")
        .bind(hash_token(BEARER.strip_prefix("Bearer ").unwrap()))
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
}
