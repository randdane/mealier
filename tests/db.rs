use mealier::Db;

#[tokio::test]
async fn fresh_file_db_runs_migrations_and_uses_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("mealier.db")).await.unwrap();
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode").fetch_one(db.pool()).await.unwrap();
    assert_eq!(mode, "wal");
}
