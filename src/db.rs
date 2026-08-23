//! Database connection setup.

use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// Opens (creating if necessary) the SQLite database at `database_url` and
/// runs any pending migrations from `./migrations`.
pub async fn init(database_url: &str) -> anyhow::Result<SqlitePool> {
    // Ensure the DB file's parent directory exists.
    if let Some(path) = database_url.strip_prefix("sqlite://")
        && let Some(parent) = Path::new(path).parent()
    {
        std::fs::create_dir_all(parent)?;
    }

    let options = SqliteConnectOptions::from_str(database_url)?.create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}
