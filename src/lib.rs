use std::path::Path;

pub mod api;
mod auth;
mod guard;
mod errors;
mod households;
mod images;
mod login;
mod oidc;
pub mod backup;
mod batches;
mod cookbooks;
pub mod fetch;
pub mod filter;
mod import;
mod labels;
mod llm;
mod parse;
mod people;
mod planner;
mod recipes;
mod rules;
mod tenancy;
mod units;
mod webhooks;
mod urlimport;
mod web;
pub use auth::*;
pub use guard::AuthConfig;
pub(crate) use guard::{Admin, Auth, CurrentUser, Guard, SessionUser};
pub use oidc::*;
pub use batches::*;
pub use cookbooks::*;
pub use import::*;
pub use labels::*;
pub use llm::*;
pub use parse::*;
pub use people::*;
pub use planner::*;
pub use recipes::*;
pub use rules::*;
pub use tenancy::{Group, Household, Scope};
pub use urlimport::*;
pub use webhooks::*;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Validation(String),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("sign in required")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    TooMany(String),
    #[error("{0}")]
    Unavailable(String),
    /// A failure that is not I/O and not the caller's doing: a task that panicked, a template that did not render.
    #[error("{0}")]
    Internal(String),
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
    notifier: Notifier,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Db> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // sqlx's default too; spelled out because a writer waiting on another is what this buys.
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        Self::connect(opts, 4).await
    }

    /// In-memory DB for tests. One connection, since each would be its own DB.
    pub async fn open_memory() -> Result<Db> {
        let opts = SqliteConnectOptions::new().in_memory(true).foreign_keys(true);
        Self::connect(opts, 1).await
    }

    async fn connect(opts: SqliteConnectOptions, max: u32) -> Result<Db> {
        // Migrations rebuild tables (0009), and SQLite ignores `PRAGMA foreign_keys` inside the
        // transaction a migration runs in, so they run on one connection that has it off.
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts.clone().foreign_keys(false)).await?;
        let applied = || async { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap_or(0) };
        let before = applied().await;
        sqlx::migrate!().run(&pool).await?;
        if applied().await != before {
            let broken = sqlx::query("PRAGMA foreign_key_check").fetch_all(&pool).await?.len();
            if broken > 0 {
                return Err(Error::Validation(format!("a migration left {broken} dangling references; the database was not opened")));
            }
        }
        // An in-memory database lives and dies with its one connection, so that one is kept and switched on.
        // A file database gets a fresh pool, every connection of which starts with foreign keys on.
        let pool = if max == 1 {
            sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await?;
            pool
        } else {
            pool.close().await;
            SqlitePoolOptions::new().max_connections(max).connect_with(opts).await?
        };
        Ok(Db { pool, notifier: Notifier::from_env() })
    }

    /// Raw pool. Exists only for tests of DB-level guarantees (snapshot trigger,
    /// units CHECK, WAL pragma); not part of the API.
    #[doc(hidden)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
