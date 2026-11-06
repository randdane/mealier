//! Step 14, part B: share links at the store. Tokens are random, shown once and kept only as a hash;
//! a link ends by expiry or revocation, and always reads as "not found".
mod common;
use mealier::{Db, Error, RecipeInput, Scope, hash_token};

async fn soup(db: &Db, sc: &Scope) -> i64 {
    db.create_recipe(sc, RecipeInput { name: "Soup".into(), description: "warm".into(), ..Default::default() }).await.unwrap().id
}

fn not_found<T: std::fmt::Debug>(r: Result<T, Error>) {
    assert!(matches!(r, Err(Error::NotFound(_))), "expected NotFound, got {r:?}");
}

fn validation<T: std::fmt::Debug>(r: Result<T, Error>) {
    assert!(matches!(r, Err(Error::Validation(_))), "expected a validation error, got {r:?}");
}

#[tokio::test]
async fn a_token_is_32_random_bytes_shown_once_and_only_its_hash_is_stored() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = soup(&db, &sc).await;
    let a = db.create_share_link(&sc, r, None).await.unwrap();
    let b = db.create_share_link(&sc, r, None).await.unwrap();
    assert_ne!(a.token, b.token);
    // 32 bytes as unpadded base64url
    assert_eq!(a.token.len(), 43, "{}", a.token);
    assert!(a.token.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'), "{}", a.token);
    assert_eq!(a.path, format!("/share/{}", a.token));

    let rows: Vec<(String,)> = sqlx::query_as("SELECT token_hash FROM share_links ORDER BY id").fetch_all(db.pool()).await.unwrap();
    assert_eq!(rows[0].0, hash_token(&a.token));
    let everything: String = sqlx::query_scalar("SELECT json_group_array(json_array(id, group_id, recipe_id, token_hash, created_by, created_at, expires_at)) FROM share_links").fetch_one(db.pool()).await.unwrap();
    assert!(!everything.contains(&a.token) && !everything.contains(&b.token), "the token itself is nowhere in the table");
    // and the listing never carries it
    let listed = serde_json::to_string(&db.share_links(&sc, r).await.unwrap()).unwrap();
    assert!(!listed.contains(&a.token) && listed.contains(&a.link.expires_at));
}

#[tokio::test]
async fn expiry_defaults_to_30_days_and_is_at_most_365() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = soup(&db, &sc).await;
    for (days, expect) in [(None, 30), (Some(1), 1), (Some(365), 365)] {
        let l = db.create_share_link(&sc, r, days).await.unwrap().link;
        let day: String = sqlx::query_scalar(&format!("SELECT strftime('%Y-%m-%d', 'now', '+{expect} days')")).fetch_one(db.pool()).await.unwrap();
        assert!(l.expires_at.starts_with(&day) && l.expires_at.ends_with('Z') && l.expires_at.len() == 20, "{days:?}: {}", l.expires_at);
    }
    for bad in [0, -1, 366, 10_000] {
        validation(db.create_share_link(&sc, r, Some(bad)).await);
    }
    assert_eq!(db.share_links(&sc, r).await.unwrap().len(), 3, "refusals made nothing");
}

#[tokio::test]
async fn a_link_opens_its_recipe_until_it_expires_or_is_revoked() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = soup(&db, &sc).await;
    let live = db.create_share_link(&sc, r, None).await.unwrap();
    let old = db.create_share_link(&sc, r, None).await.unwrap();
    let gone = db.create_share_link(&sc, r, None).await.unwrap();
    assert_eq!(db.shared_recipe(&live.token).await.unwrap().content.name, "Soup");

    sqlx::query("UPDATE share_links SET expires_at = '2020-01-01T00:00:00Z' WHERE id = ?").bind(old.link.id).execute(db.pool()).await.unwrap();
    not_found(db.shared_recipe(&old.token).await);
    db.revoke_share_link(&sc, gone.link.id).await.unwrap();
    not_found(db.shared_recipe(&gone.token).await);
    assert_eq!(db.shared_recipe(&live.token).await.unwrap().id, r, "the others are untouched");
    for bad in ["", "nope", &live.token[..42], &format!("{}x", live.token), &live.token.to_uppercase()] {
        not_found(db.shared_recipe(bad).await);
    }
    not_found(db.revoke_share_link(&sc, gone.link.id).await);
}

#[tokio::test]
async fn an_archived_recipe_stops_being_shared() {
    let db = Db::open_memory().await.unwrap();
    let sc = common::scope(&db).await;
    let r = soup(&db, &sc).await;
    let l = db.create_share_link(&sc, r, None).await.unwrap();
    db.archive_recipe(&sc, r).await.unwrap();
    not_found(db.shared_recipe(&l.token).await);
}

#[tokio::test]
async fn links_belong_to_the_group_of_their_recipe() {
    let db = Db::open_memory().await.unwrap();
    let t = common::tenants(&db).await;
    let r = soup(&db, &t.a).await;
    not_found(db.create_share_link(&t.c, r, None).await);
    not_found(db.share_links(&t.c, r).await);
    let l = db.create_share_link(&t.a, r, None).await.unwrap();
    // another household of the group may list and revoke it; another group may not
    assert_eq!(db.share_links(&t.b, r).await.unwrap().len(), 1);
    not_found(db.revoke_share_link(&t.c, l.link.id).await);
    assert!(db.shared_recipe(&l.token).await.is_ok());
    db.revoke_share_link(&t.b, l.link.id).await.unwrap();
    not_found(db.shared_recipe(&l.token).await);
}
