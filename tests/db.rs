use mealier::Db;

#[tokio::test]
async fn fresh_file_db_runs_migrations_and_uses_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("mealier.db")).await.unwrap();
    assert_eq!(db.journal_mode().await.unwrap(), "wal");
}
