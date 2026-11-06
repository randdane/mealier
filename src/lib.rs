use std::path::Path;

pub mod api;
mod batches;
pub mod fetch;
mod import;
mod parse;
mod people;
mod planner;
mod recipes;
mod units;
mod urlimport;
mod web;
pub use batches::*;
pub use import::*;
pub use parse::*;
pub use people::*;
pub use planner::*;
pub use recipes::*;
pub use urlimport::*;

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
    #[error(transparent)]
    Io(#[from] std::io::Error),
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

    /// Raw pool. Exists only for tests of DB-level guarantees (snapshot trigger,
    /// units CHECK, WAL pragma); not part of the API.
    #[doc(hidden)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
