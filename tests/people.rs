mod common;
use mealier::{Db, Error};

async fn user(db: &Db) -> i64 {
    sqlx::query_scalar("INSERT INTO users (username, email, role, household_id) VALUES ('a', 'a@x', 'user', ?) RETURNING id")
        .bind(common::household(db).await)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn theme_of(db: &Db, id: i64) -> String {
    db.users().await.unwrap().into_iter().find(|u| u.id == id).unwrap().cook_theme
}

#[tokio::test]
async fn cook_theme_defaults_to_normal_and_can_be_set() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db).await;
    assert_eq!(theme_of(&db, id).await, "normal");
    for t in ["high-contrast", "max-white", "normal"] {
        db.set_cook_theme(id, t).await.unwrap();
        assert_eq!(theme_of(&db, id).await, t);
    }
}

#[tokio::test]
async fn invalid_cook_theme_is_a_validation_error_and_changes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let id = user(&db).await;
    db.set_cook_theme(id, "max-white").await.unwrap();
    assert!(matches!(db.set_cook_theme(id, "neon").await, Err(Error::Validation(_))));
    assert_eq!(theme_of(&db, id).await, "max-white");
}

#[tokio::test]
async fn cook_theme_for_a_missing_user_is_not_found() {
    let db = Db::open_memory().await.unwrap();
    assert!(matches!(db.set_cook_theme(99, "normal").await, Err(Error::NotFound(_))));
}
