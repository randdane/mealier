use std::path::Path;

pub mod api;
mod recipes;
pub use recipes::*;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Validation(String),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Db> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        Self::connect(opts, 4).await
    }

    /// In-memory DB for tests. One connection, since each would be its own DB.
    pub async fn open_memory() -> Result<Db> {
        let opts = SqliteConnectOptions::new().in_memory(true).foreign_keys(true);
        Self::connect(opts, 1).await
    }

    async fn connect(opts: SqliteConnectOptions, max: u32) -> Result<Db> {
        let pool = SqlitePoolOptions::new().max_connections(max).connect_with(opts).await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Db { pool })
    }

    /// Raw pool, for tests of DB-level guarantees such as the snapshot trigger.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn journal_mode(&self) -> Result<String> {
        Ok(sqlx::query_scalar("PRAGMA journal_mode").fetch_one(&self.pool).await?)
    }
}
