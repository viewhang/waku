//! Stores provider-reported assistant usage locally, once per completed request

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params};
use uuid::Uuid;

use super::{TokenTotals, UsageProvider, UsageRecord};

const DATABASE: &str = "assistant-usage.sqlite3";

pub(crate) struct AssistantUsage {
    path: PathBuf,
    session_id: Uuid,
    project: String,
}

impl AssistantUsage {
    pub(crate) fn new(directory: &Path, session_id: Uuid, cwd: &Path) -> Self {
        Self {
            path: directory.join(DATABASE),
            session_id,
            project: cwd.to_string_lossy().into_owned(),
        }
    }

    pub(crate) fn record(&self, model: &str, totals: TokenTotals) -> anyhow::Result<()> {
        if totals == TokenTotals::default() {
            return Ok(());
        }
        let database = Connection::open(&self.path)?;
        database.execute_batch(
            "CREATE TABLE IF NOT EXISTS assistant_usage (
                timestamp_ms INTEGER NOT NULL,
                model TEXT NOT NULL,
                session_id TEXT NOT NULL,
                project TEXT NOT NULL,
                uncached_input INTEGER NOT NULL,
                cached_input INTEGER NOT NULL,
                cache_creation INTEGER NOT NULL,
                output INTEGER NOT NULL,
                reasoning INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS assistant_usage_timestamp
                ON assistant_usage (timestamp_ms);",
        )?;
        database.execute(
            "INSERT INTO assistant_usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                chrono::Utc::now().timestamp_millis(),
                model,
                self.session_id.to_string(),
                self.project,
                totals.uncached_input,
                totals.cached_input,
                totals.cache_creation,
                totals.output,
                totals.reasoning,
            ],
        )?;
        Ok(())
    }
}

pub(super) fn load(directory: &Path, since_ms: i64) -> anyhow::Result<Vec<UsageRecord>> {
    let path = directory.join(DATABASE);
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = database.prepare(
        "SELECT timestamp_ms, model, session_id, project, uncached_input, cached_input,
                cache_creation, output, reasoning
         FROM assistant_usage WHERE timestamp_ms >= ?1",
    )?;
    let rows = statement.query_map([since_ms], |row| {
        Ok(UsageRecord {
            provider: UsageProvider::Assistant,
            timestamp_ms: row.get(0)?,
            model: row.get(1)?,
            session_id: row.get(2)?,
            project: row.get(3)?,
            totals: TokenTotals {
                uncached_input: row.get(4)?,
                cached_input: row.get(5)?,
                cache_creation: row.get(6)?,
                output: row.get(7)?,
                reasoning: row.get(8)?,
            },
            reported_cost_usd: None,
            dedupe_key: None,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
