use rusqlite::{Connection, OptionalExtension};
use std::{fs, path::PathBuf};
use tauri::{AppHandle, Manager, Wry};

pub const DATABASE_FILE: &str = "usage.db";

pub fn database_path(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("Codex Usage Monitor");
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir.join(DATABASE_FILE))
}

pub fn open_database(app: &AppHandle<Wry>) -> Result<Connection, String> {
    let connection = Connection::open(database_path(app)?).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| error.to_string())?;
    initialize_schema(&connection)?;
    // quota_intervals is derived data. Rebuild it from the immutable raw
    // samples once when upgrading legacy adjacent-sample rows; new samples
    // are rebuilt synchronously by recorder::record_rate_limit_samples.
    let needs_rebuild: bool = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM quota_intervals
               WHERE window_duration_mins = 0 OR sample_quality = 'legacy'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if needs_rebuild {
        super::quota::rebuild_all_intervals(&connection)?;
    }
    Ok(connection)
}

pub fn initialize_schema(connection: &Connection) -> Result<(), String> {
    migrate_legacy_tables(connection)?;
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS accounts (
                account_key TEXT PRIMARY KEY,
                display_name TEXT,
                email TEXT,
                plan_type TEXT,
                auth_identity TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS usage_migrations (
                name TEXT PRIMARY KEY,
                completed_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS turn_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                model TEXT,
                reasoning_effort TEXT NOT NULL,
                speed_mode TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                raw_total_tokens INTEGER NOT NULL DEFAULT 0,
                estimated_credits REAL,
                rate_card_version TEXT,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE (account_key, thread_id, turn_id)
            );

            CREATE TABLE IF NOT EXISTS turn_token_samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                model TEXT,
                reasoning_effort TEXT NOT NULL,
                speed_mode TEXT NOT NULL,
                sampled_at INTEGER NOT NULL,
                cumulative_tokens INTEGER NOT NULL,
                delta_tokens INTEGER NOT NULL,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                UNIQUE (account_key, thread_id, turn_id, cumulative_tokens)
            );

            CREATE TABLE IF NOT EXISTS account_daily_usage (
                account_key TEXT NOT NULL,
                date TEXT NOT NULL,
                official_tokens INTEGER NOT NULL,
                fetched_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                PRIMARY KEY (account_key, date)
            );

            CREATE TABLE IF NOT EXISTS thread_usage_group_samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                sampled_at INTEGER NOT NULL,
                model TEXT,
                reasoning_effort TEXT NOT NULL,
                speed_mode TEXT NOT NULL,
                estimated_usage_credits_micros INTEGER NOT NULL,
                estimated_usage_usd_micros INTEGER,
                net_new_input_tokens INTEGER,
                cached_input_tokens INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS thread_usage_capabilities (
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                last_attempted_at INTEGER NOT NULL,
                last_sampled_at INTEGER,
                capability TEXT NOT NULL,
                error TEXT,
                retry_count INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (account_key, thread_id)
            );

            CREATE TABLE IF NOT EXISTS rate_limit_samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                sampled_at INTEGER NOT NULL,
                limit_id TEXT NOT NULL,
                window TEXT NOT NULL,
                window_duration_mins INTEGER NOT NULL,
                used_percent REAL NOT NULL,
                resets_at INTEGER,
                generation INTEGER,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS quota_intervals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                limit_id TEXT NOT NULL,
                window TEXT NOT NULL,
                window_duration_mins INTEGER NOT NULL DEFAULT 0,
                cycle_id TEXT NOT NULL DEFAULT 'reset:unknown',
                start_sample_id INTEGER,
                end_sample_id INTEGER,
                start_at INTEGER NOT NULL,
                end_at INTEGER NOT NULL,
                start_percent REAL NOT NULL,
                end_percent REAL NOT NULL,
                observed_delta_percent REAL NOT NULL,
                local_weighted_credits REAL,
                unattributed_percent REAL,
                sample_quality TEXT NOT NULL DEFAULT 'quota_step',
                rejection_reason TEXT,
                confidence TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS rollout_cursors (
                file_path TEXT PRIMARY KEY,
                byte_offset INTEGER NOT NULL,
                last_modified_at INTEGER,
                last_scanned_at INTEGER NOT NULL,
                state_json TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_turn_usage_date
                ON turn_usage(account_key, started_at);
            CREATE INDEX IF NOT EXISTS idx_turn_token_samples_time
                ON turn_token_samples(account_key, sampled_at, thread_id, turn_id);
            CREATE INDEX IF NOT EXISTS idx_account_daily_usage_date
                ON account_daily_usage(account_key, date);
            CREATE INDEX IF NOT EXISTS idx_thread_usage_group_samples_time
                ON thread_usage_group_samples(account_key, thread_id, sampled_at, id);
            CREATE INDEX IF NOT EXISTS idx_thread_usage_capabilities_time
                ON thread_usage_capabilities(account_key, last_attempted_at);
            CREATE INDEX IF NOT EXISTS idx_rate_limit_samples_time
                ON rate_limit_samples(account_key, limit_id, window, sampled_at);
            CREATE INDEX IF NOT EXISTS idx_quota_intervals_time
                ON quota_intervals(account_key, limit_id, window, start_at, end_at);
            ",
        )
        .map_err(|error| error.to_string())?;

    if table_exists(connection, "thread_usage_capabilities")?
        && !has_column(connection, "thread_usage_capabilities", "retry_count")?
    {
        connection
            .execute(
                "ALTER TABLE thread_usage_capabilities
                 ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    for (column, definition) in [
        ("window_duration_mins", "INTEGER NOT NULL DEFAULT 0"),
        ("cycle_id", "TEXT NOT NULL DEFAULT 'reset:unknown'"),
        ("start_sample_id", "INTEGER"),
        ("end_sample_id", "INTEGER"),
        ("sample_quality", "TEXT NOT NULL DEFAULT 'legacy'"),
        ("rejection_reason", "TEXT"),
    ] {
        if table_exists(connection, "quota_intervals")?
            && !has_column(connection, "quota_intervals", column)?
        {
            connection
                .execute(
                    &format!("ALTER TABLE quota_intervals ADD COLUMN {column} {definition}"),
                    [],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(true),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(|error| error.to_string())
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?;
    for row in rows {
        if row.map_err(|error| error.to_string())? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The first prototype used different names and primary keys. Rebuild only
/// those known legacy tables, preserving their metadata and token totals.
fn migrate_legacy_tables(connection: &Connection) -> Result<(), String> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;

    if table_exists(&transaction, "turn_usage")?
        && !has_column(&transaction, "turn_usage", "started_at")?
    {
        transaction
            .execute_batch(
                "
            ALTER TABLE turn_usage RENAME TO turn_usage_legacy;
            CREATE TABLE turn_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                model TEXT,
                reasoning_effort TEXT NOT NULL,
                speed_mode TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                raw_total_tokens INTEGER NOT NULL DEFAULT 0,
                estimated_credits REAL,
                rate_card_version TEXT,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE (account_key, thread_id, turn_id)
            );
            INSERT OR IGNORE INTO turn_usage
              (account_key, thread_id, turn_id, started_at, completed_at, model,
               reasoning_effort, speed_mode, input_tokens, cached_input_tokens,
               output_tokens, reasoning_output_tokens, raw_total_tokens, source,
               confidence, created_at, updated_at)
            SELECT account_key, thread_id, turn_id, timestamp, timestamp, model,
               reasoning_effort,
               CASE lower(service_tier)
                 WHEN 'standard' THEN 'standard'
                 WHEN 'fast requested' THEN 'fast_requested'
                 ELSE 'unknown'
               END,
               input_tokens, cached_input_tokens, output_tokens,
               reasoning_output_tokens, total_tokens, source, 'low', timestamp, timestamp
            FROM turn_usage_legacy;
            DROP TABLE turn_usage_legacy;
            ",
            )
            .map_err(|error| error.to_string())?;
    }

    if table_exists(&transaction, "account_daily_usage")?
        && !has_column(&transaction, "account_daily_usage", "official_tokens")?
    {
        transaction
            .execute_batch(
                "
            ALTER TABLE account_daily_usage RENAME TO account_daily_usage_legacy;
            CREATE TABLE account_daily_usage (
                account_key TEXT NOT NULL,
                date TEXT NOT NULL,
                official_tokens INTEGER NOT NULL,
                fetched_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                PRIMARY KEY (account_key, date)
            );
            INSERT OR REPLACE INTO account_daily_usage
              (account_key, date, official_tokens, fetched_at, source, confidence)
            SELECT account_key, date, tokens, fetched_at, source, 'high'
            FROM account_daily_usage_legacy;
            DROP TABLE account_daily_usage_legacy;
            ",
            )
            .map_err(|error| error.to_string())?;
    }

    if table_exists(&transaction, "rate_limit_samples")?
        && !has_column(&transaction, "rate_limit_samples", "sampled_at")?
    {
        transaction
            .execute_batch(
                "
            ALTER TABLE rate_limit_samples RENAME TO rate_limit_samples_legacy;
            CREATE TABLE rate_limit_samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                sampled_at INTEGER NOT NULL,
                limit_id TEXT NOT NULL,
                window TEXT NOT NULL,
                window_duration_mins INTEGER NOT NULL,
                used_percent REAL NOT NULL,
                resets_at INTEGER,
                generation INTEGER,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL
            );
            INSERT INTO rate_limit_samples
              (account_key, sampled_at, limit_id, window, window_duration_mins,
               used_percent, resets_at, source, confidence)
            SELECT account_key, timestamp, limit_id, window, window_duration_mins,
                   used_percent, resets_at, source, 'low'
            FROM rate_limit_samples_legacy;
            DROP TABLE rate_limit_samples_legacy;
            ",
            )
            .map_err(|error| error.to_string())?;
    }

    if table_exists(&transaction, "rollout_cursors")?
        && !has_column(&transaction, "rollout_cursors", "last_modified_at")?
    {
        transaction
            .execute_batch(
                "
            ALTER TABLE rollout_cursors RENAME TO rollout_cursors_legacy;
            CREATE TABLE rollout_cursors (
                file_path TEXT PRIMARY KEY,
                byte_offset INTEGER NOT NULL,
                last_modified_at INTEGER,
                last_scanned_at INTEGER NOT NULL,
                state_json TEXT NOT NULL
            );
            INSERT INTO rollout_cursors
              (file_path, byte_offset, last_modified_at, last_scanned_at, state_json)
            SELECT file_path, byte_offset, NULL, last_seen_at, state_json
            FROM rollout_cursors_legacy;
            DROP TABLE rollout_cursors_legacy;
            ",
            )
            .map_err(|error| error.to_string())?;
    }

    transaction.commit().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_v1_tables() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for table in [
            "accounts",
            "usage_migrations",
            "turn_usage",
            "turn_token_samples",
            "account_daily_usage",
            "thread_usage_group_samples",
            "thread_usage_capabilities",
            "rate_limit_samples",
            "quota_intervals",
            "rollout_cursors",
        ] {
            assert!(table_exists(&connection, table).unwrap(), "missing {table}");
        }
    }

    #[test]
    fn migrates_the_first_prototype_without_losing_totals_or_cursors() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE account_daily_usage (
                  account_key TEXT NOT NULL, date TEXT NOT NULL, tokens INTEGER NOT NULL,
                  lifetime_tokens INTEGER, fetched_at INTEGER NOT NULL, source TEXT NOT NULL,
                  PRIMARY KEY (account_key, date)
                );
                INSERT INTO account_daily_usage VALUES ('a', '2026-08-14', 42, NULL, 1, 'account_usage');
                CREATE TABLE turn_usage (
                  id INTEGER PRIMARY KEY AUTOINCREMENT, account_key TEXT NOT NULL,
                  thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, timestamp INTEGER NOT NULL,
                  model TEXT NOT NULL, reasoning_effort TEXT NOT NULL, service_tier TEXT NOT NULL,
                  input_tokens INTEGER NOT NULL, cached_input_tokens INTEGER NOT NULL,
                  cache_write_input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                  reasoning_output_tokens INTEGER NOT NULL, total_tokens INTEGER NOT NULL,
                  source TEXT NOT NULL, UNIQUE (account_key, thread_id, turn_id)
                );
                INSERT INTO turn_usage
                  (account_key, thread_id, turn_id, timestamp, model, reasoning_effort, service_tier,
                   input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens,
                   reasoning_output_tokens, total_tokens, source)
                VALUES ('a', 'thread', 'turn', 2, 'gpt-5.6-sol', 'high', 'Standard', 10, 2, 0, 3, 1, 13, 'rollout');
                CREATE TABLE rate_limit_samples (
                  account_key TEXT NOT NULL, timestamp INTEGER NOT NULL, limit_id TEXT NOT NULL,
                  window TEXT NOT NULL, window_duration_mins INTEGER NOT NULL, used_percent REAL NOT NULL,
                  resets_at INTEGER, source TEXT NOT NULL,
                  PRIMARY KEY (account_key, timestamp, limit_id, window)
                );
                INSERT INTO rate_limit_samples VALUES ('a', 2, 'codex', 'primary', 10080, 1, 99, 'rate_limits');
                CREATE TABLE rollout_cursors (
                  file_path TEXT PRIMARY KEY, byte_offset INTEGER NOT NULL,
                  last_seen_at INTEGER NOT NULL, state_json TEXT NOT NULL
                );
                INSERT INTO rollout_cursors VALUES ('/tmp/a.jsonl', 5, 2, '{}');
                ",
            )
            .unwrap();
        initialize_schema(&connection).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT official_tokens FROM account_daily_usage",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            42
        );
        assert_eq!(
            connection
                .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            13
        );
        assert_eq!(
            connection
                .query_row("SELECT sampled_at FROM rate_limit_samples", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            2
        );
        assert_eq!(
            connection
                .query_row("SELECT byte_offset FROM rollout_cursors", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            5
        );
    }
}
