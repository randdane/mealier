//! S25: sessions, API tokens and disabling users, at the store.
use mealier::{Db, Error, hash_token};

async fn user(db: &Db, name: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO users (username, email, role) VALUES (?, ?, 'user') RETURNING id")
        .bind(name)
        .bind(format!("{name}@x"))
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn count(db: &Db, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(db.pool()).await.unwrap()
}

#[tokio::test]
async fn a_session_finds_its_user_and_stores_only_a_hash() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db, "a").await;
    let cookie = db.create_session(id).await.unwrap();
    assert_eq!(db.session_user(&cookie).await.unwrap().unwrap().id, id);

    let stored: String = sqlx::query_scalar("SELECT id_hash FROM sessions").fetch_one(db.pool()).await.unwrap();
    assert_ne!(stored, cookie);
    assert_eq!(stored, hash_token(&cookie));
    assert_eq!(stored.len(), 64);
}

#[tokio::test]
async fn session_ids_are_32_random_bytes_and_never_repeat() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db, "a").await;
    let a = db.create_session(id).await.unwrap();
    let b = db.create_session(id).await.unwrap();
    assert_ne!(a, b);
    assert_eq!(a.len(), 64, "32 bytes as hex");
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn a_session_lasts_seven_days() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db, "a").await;
    db.create_session(id).await.unwrap();
    let secs: f64 = sqlx::query_scalar("SELECT (julianday(expires_at) - julianday(created_at)) * 86400 FROM sessions").fetch_one(db.pool()).await.unwrap();
    assert!((secs - 7.0 * 86400.0).abs() < 5.0, "{secs}");
}

#[tokio::test]
async fn an_expired_or_unknown_session_finds_nobody() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db, "a").await;
    let cookie = db.create_session(id).await.unwrap();
    assert!(db.session_user("0".repeat(64).as_str()).await.unwrap().is_none());
    assert!(db.session_user("").await.unwrap().is_none());
    sqlx::query("UPDATE sessions SET expires_at = '2000-01-01T00:00:00.000Z'").execute(db.pool()).await.unwrap();
    assert!(db.session_user(&cookie).await.unwrap().is_none());
}

#[tokio::test]
async fn logout_deletes_only_that_session() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db, "a").await;
    let (a, b) = (db.create_session(id).await.unwrap(), db.create_session(id).await.unwrap());
    db.delete_session(&a).await.unwrap();
    assert!(db.session_user(&a).await.unwrap().is_none());
    assert!(db.session_user(&b).await.unwrap().is_some());
    assert_eq!(count(&db, "sessions").await, 1);
}

#[tokio::test]
async fn a_token_finds_its_user_is_shown_once_and_records_use() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db, "a").await;
    let (row, secret) = db.create_api_token(id, "laptop").await.unwrap();
    assert_eq!((row.name.as_str(), row.last_used_at.is_none()), ("laptop", true));
    assert!(secret.len() >= 40, "{secret}");

    assert_eq!(db.token_user(&secret).await.unwrap().unwrap().id, id);
    let listed = db.api_tokens(id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].last_used_at.is_some());
    assert!(!format!("{listed:?}").contains(&secret));

    let stored: String = sqlx::query_scalar("SELECT hash FROM api_tokens").fetch_one(db.pool()).await.unwrap();
    assert_eq!(stored, hash_token(&secret));
    assert_ne!(stored, secret);
}

#[tokio::test]
async fn an_unknown_token_finds_nobody_and_a_session_is_not_a_token() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db, "a").await;
    let cookie = db.create_session(id).await.unwrap();
    assert!(db.token_user("nope").await.unwrap().is_none());
    assert!(db.token_user(&cookie).await.unwrap().is_none());
    let (_, secret) = db.create_api_token(id, "t").await.unwrap();
    assert!(db.session_user(&secret).await.unwrap().is_none());
}

#[tokio::test]
async fn a_token_needs_a_name_and_only_its_owner_can_revoke_it() {
    let db = Db::open_memory().await.unwrap();
    let (a, b) = (user(&db, "a").await, user(&db, "b").await);
    assert!(matches!(db.create_api_token(a, "  ").await, Err(Error::Validation(_))));
    let (row, secret) = db.create_api_token(a, "t").await.unwrap();

    assert!(matches!(db.revoke_api_token(b, row.id).await, Err(Error::NotFound(_))));
    assert!(db.token_user(&secret).await.unwrap().is_some(), "someone else's revoke must not work");
    db.revoke_api_token(a, row.id).await.unwrap();
    assert!(db.token_user(&secret).await.unwrap().is_none());
    assert!(db.api_tokens(a).await.unwrap().is_empty());
}

#[tokio::test]
async fn disabling_a_user_revokes_their_sessions_and_tokens_at_once() {
    let db = Db::open_memory().await.unwrap();
    let (a, b) = (user(&db, "a").await, user(&db, "b").await);
    let (sa, sb) = (db.create_session(a).await.unwrap(), db.create_session(b).await.unwrap());
    let (_, ta) = db.create_api_token(a, "t").await.unwrap();
    let (_, tb) = db.create_api_token(b, "t").await.unwrap();

    db.set_user_disabled(a, true).await.unwrap();
    assert!(db.user(a).await.unwrap().disabled);
    assert!(db.session_user(&sa).await.unwrap().is_none());
    assert!(db.token_user(&ta).await.unwrap().is_none());
    assert!(db.session_user(&sb).await.unwrap().is_some(), "other users are untouched");
    assert!(db.token_user(&tb).await.unwrap().is_some());
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = ?").bind(a).fetch_one(db.pool()).await.unwrap();
    let left_t: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens WHERE user_id = ?").bind(a).fetch_one(db.pool()).await.unwrap();
    assert_eq!((left, left_t), (0, 0), "rows are deleted, not just ignored");

    db.set_user_disabled(a, false).await.unwrap();
    assert!(db.session_user(&sa).await.unwrap().is_none(), "enabling does not resurrect them");
}

#[tokio::test]
async fn every_lookup_rejects_a_disabled_user_even_if_their_rows_survive() {
    let db = Db::open_memory().await.unwrap();
    let a = user(&db, "a").await;
    let cookie = db.create_session(a).await.unwrap();
    let (_, token) = db.create_api_token(a, "t").await.unwrap();
    sqlx::query("UPDATE users SET disabled = 1").execute(db.pool()).await.unwrap();
    assert!(db.session_user(&cookie).await.unwrap().is_none());
    assert!(db.token_user(&token).await.unwrap().is_none());
}

#[tokio::test]
async fn user_and_disable_of_a_missing_id_are_not_found() {
    let db = Db::open_memory().await.unwrap();
    assert!(matches!(db.user(99).await, Err(Error::NotFound(_))));
    assert!(matches!(db.set_user_disabled(99, true).await, Err(Error::NotFound(_))));
}
