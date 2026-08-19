use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager, Wry};

pub const DATABASE_FILE: &str = "usage.db";

fn database_directory(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("Codex Usage Monitor"))
}

/// Resolve the database path without changing the filesystem. Read-only UI
/// queries use this function and must not create an app-data directory.
pub fn database_path(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    Ok(database_directory(app)?.join(DATABASE_FILE))
}

fn database_path_for_writer(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    let dir = database_directory(app)?;
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir.join(DATABASE_FILE))
}

/// Open a read-only usage connection. Query paths must use this API; it does
/// not create the database, run migrations, change journal mode, or rebuild
/// derived data.
pub fn open_database(app: &AppHandle<Wry>) -> Result<Connection, String> {
    let path = database_path(app)?;
    if !path.is_file() {
        return Err("usage database is not initialized".into());
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "query_only", true)
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

pub(crate) fn open_database_for_collector(app: &AppHandle<Wry>) -> Result<Connection, String> {
    if !super::collector_core::writer_token_active() {
        return Err("collector writer token is not held".into());
    }
    open_database_rw(app)
}

pub(crate) fn open_database_for_lock(
    app: &AppHandle<Wry>,
    lock: &super::collector_core::CollectorLock,
) -> Result<Connection, String> {
    if !lock.is_owner() {
        return Err("collector lock ownership was lost".into());
    }
    open_database_rw(app)
}

fn open_database_rw(app: &AppHandle<Wry>) -> Result<Connection, String> {
    let connection =
        Connection::open(database_path_for_writer(app)?).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| error.to_string())?;
    initialize_schema(&connection)?;
    // Keep legacy/quarantined account diagnostics accurate even when the UI
    // reads usage before the rollout collector runs.
    super::rollout::classify_legacy_accounts(&connection)?;
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

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), String> {
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

            CREATE TABLE IF NOT EXISTS account_usage_data_versions (
                account_key TEXT PRIMARY KEY,
                rollout_parser_version INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'legacy_unverified',
                timeline_status TEXT NOT NULL DEFAULT 'legacy_pre_timeline',
                missing_timeline_turns INTEGER NOT NULL DEFAULT 0,
                orphan_timeline_samples INTEGER NOT NULL DEFAULT 0,
                mismatched_turns INTEGER NOT NULL DEFAULT 0,
                parse_error_count INTEGER NOT NULL DEFAULT 0,
                last_rebuild_batch_id TEXT,
                source_incomplete_count INTEGER NOT NULL DEFAULT 0,
                source_lag_seconds INTEGER NOT NULL DEFAULT 0,
                verified_at INTEGER,
                updated_at INTEGER NOT NULL,
                CHECK (rollout_parser_version >= 0),
                CHECK (missing_timeline_turns >= 0),
                CHECK (orphan_timeline_samples >= 0),
                CHECK (mismatched_turns >= 0),
                CHECK (parse_error_count >= 0),
                CHECK (source_incomplete_count >= 0),
                CHECK (source_lag_seconds >= 0)
            );

            CREATE TABLE IF NOT EXISTS turn_timeline_audits (
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                canonical_tokens INTEGER NOT NULL,
                timeline_tokens INTEGER NOT NULL,
                reason TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                PRIMARY KEY (account_key, thread_id, turn_id)
            );

            CREATE TABLE IF NOT EXISTS rollout_parse_errors (
                file_path TEXT NOT NULL,
                byte_offset INTEGER NOT NULL,
                error TEXT NOT NULL,
                account_key TEXT,
                source_id TEXT,
                rebuild_batch_id TEXT,
                first_seen_at INTEGER NOT NULL,
                PRIMARY KEY (file_path, byte_offset)
            );

            CREATE TABLE IF NOT EXISTS rollout_file_bindings (
                file_path TEXT PRIMARY KEY,
                account_key TEXT,
                status TEXT NOT NULL,
                reason TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                last_modified_at INTEGER,
                CHECK (status IN ('bound', 'pending', 'quarantined'))
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
                segment_no INTEGER NOT NULL DEFAULT 0,
                model TEXT,
                reasoning_effort TEXT NOT NULL,
                speed_mode TEXT NOT NULL,
                sampled_at INTEGER NOT NULL,
                cumulative_tokens INTEGER NOT NULL,
                delta_tokens INTEGER NOT NULL,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                UNIQUE (account_key, thread_id, turn_id, segment_no, cumulative_tokens),
                CHECK (segment_no >= 0),
                CHECK (cumulative_tokens >= 0),
                CHECK (delta_tokens >= 0)
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

            -- Collector-owned durable data plane. The legacy binding/cursor
            -- tables remain for compatibility with existing databases and
            -- are mirrored by collector_core as sources are observed.
            CREATE TABLE IF NOT EXISTS rollout_sources (
                source_id TEXT PRIMARY KEY,
                canonical_path TEXT NOT NULL,
                file_identity TEXT NOT NULL,
                session_id TEXT,
                thread_id TEXT,
                account_key TEXT,
                binding_status TEXT NOT NULL DEFAULT 'unresolved',
                binding_source TEXT NOT NULL DEFAULT 'unresolved',
                binding_confidence TEXT NOT NULL DEFAULT 'unknown',
                first_seen_at INTEGER NOT NULL,
                first_activity_at INTEGER,
                last_activity_at INTEGER,
                last_offset INTEGER NOT NULL DEFAULT 0,
                last_size INTEGER NOT NULL DEFAULT 0,
                last_mtime INTEGER,
                cursor_state_json TEXT NOT NULL DEFAULT '{}',
                file_fingerprint TEXT,
                cursor_prefix_fingerprint TEXT,
                generation INTEGER NOT NULL DEFAULT 0,
                parser_version INTEGER NOT NULL DEFAULT 0,
                health_status TEXT NOT NULL DEFAULT 'unknown',
                last_error TEXT,
                last_error_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                CHECK (binding_status IN ('verified', 'inferred', 'unresolved', 'quarantined'))
            );

            CREATE TABLE IF NOT EXISTS rollout_turn_sources (
                source_id TEXT NOT NULL,
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                PRIMARY KEY (source_id, account_key, thread_id, turn_id)
            );

            CREATE TABLE IF NOT EXISTS account_presence_intervals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                account_id TEXT,
                email TEXT,
                started_at INTEGER NOT NULL,
                ended_at INTEGER,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                collector_instance_id TEXT,
                CHECK (ended_at IS NULL OR ended_at >= started_at)
            );

            CREATE TABLE IF NOT EXISTS collector_sessions (
                session_id TEXT PRIMARY KEY,
                collector_instance_id TEXT NOT NULL,
                pid INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                last_heartbeat_at INTEGER NOT NULL,
                stopped_at INTEGER,
                status TEXT NOT NULL DEFAULT 'running',
                recovery_reason TEXT,
                version TEXT NOT NULL,
                CHECK (status IN ('running', 'stopped', 'recovered'))
            );

            CREATE TABLE IF NOT EXISTS collector_gaps (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                start_at INTEGER NOT NULL,
                end_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                reason TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                CHECK (end_at >= start_at),
                CHECK (duration_ms >= 0)
            );

            CREATE TABLE IF NOT EXISTS source_binding_audits (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                old_account_key TEXT,
                new_account_key TEXT,
                old_status TEXT,
                new_status TEXT NOT NULL,
                reason TEXT NOT NULL,
                evidence TEXT,
                changed_at INTEGER NOT NULL,
                collector_instance_id TEXT
            );

            CREATE TABLE IF NOT EXISTS collector_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                collector_instance_id TEXT NOT NULL,
                pid INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                heartbeat_at INTEGER NOT NULL,
                version TEXT NOT NULL,
                status TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_turn_usage_date
                ON turn_usage(account_key, started_at);
            CREATE INDEX IF NOT EXISTS idx_turn_token_samples_time
                ON turn_token_samples(account_key, sampled_at, thread_id, turn_id);
            CREATE INDEX IF NOT EXISTS idx_turn_token_samples_turn_segment
                ON turn_token_samples(account_key, thread_id, turn_id, segment_no, sampled_at, id);
            CREATE INDEX IF NOT EXISTS idx_turn_timeline_audits_account
                ON turn_timeline_audits(account_key, reason, last_seen_at);
            CREATE INDEX IF NOT EXISTS idx_account_usage_data_versions_status
                ON account_usage_data_versions(status, rollout_parser_version);
            CREATE INDEX IF NOT EXISTS idx_rollout_file_bindings_account
                ON rollout_file_bindings(account_key, status);
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
            CREATE INDEX IF NOT EXISTS idx_rollout_sources_path
                ON rollout_sources(canonical_path);
            CREATE INDEX IF NOT EXISTS idx_rollout_sources_account
                ON rollout_sources(account_key, binding_status);
            CREATE INDEX IF NOT EXISTS idx_presence_account_time
                ON account_presence_intervals(account_key, started_at, ended_at);
            CREATE INDEX IF NOT EXISTS idx_presence_open
                ON account_presence_intervals(ended_at, started_at);
            CREATE INDEX IF NOT EXISTS idx_collector_gaps_time
                ON collector_gaps(start_at, end_at);
            CREATE INDEX IF NOT EXISTS idx_binding_audits_source_time
                ON source_binding_audits(source_id, changed_at);
            ",
        )
        .map_err(|error| error.to_string())?;

    for (table, column, definition) in [
        (
            "account_usage_data_versions",
            "parse_error_count",
            "INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "account_usage_data_versions",
            "last_rebuild_batch_id",
            "TEXT",
        ),
        (
            "account_usage_data_versions",
            "source_incomplete_count",
            "INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "account_usage_data_versions",
            "source_lag_seconds",
            "INTEGER NOT NULL DEFAULT 0",
        ),
        ("rollout_parse_errors", "account_key", "TEXT"),
        ("rollout_parse_errors", "source_id", "TEXT"),
        ("rollout_parse_errors", "rebuild_batch_id", "TEXT"),
        ("rollout_file_bindings", "last_modified_at", "INTEGER"),
    ] {
        if table_exists(connection, table)? && !has_column(connection, table, column)? {
            connection
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_rollout_parse_errors_account
             ON rollout_parse_errors(account_key, rebuild_batch_id);",
        )
        .map_err(|error| error.to_string())?;
    migrate_rollout_file_binding_status(connection)?;

    connection
        .execute(
            "INSERT OR IGNORE INTO usage_migrations (name, completed_at)
             VALUES ('durable_collector_v1', strftime('%s', 'now'))",
            [],
        )
        .map_err(|error| error.to_string())?;

    // Existing account-bound rows predate account-scoped parser versions.
    // Register them as legacy without overwriting a status established by a
    // completed account migration.
    connection
        .execute(
            "INSERT OR IGNORE INTO account_usage_data_versions
             (account_key, rollout_parser_version, status, timeline_status,
              missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
              verified_at, updated_at)
             SELECT account_key, 0, 'legacy_unverified', 'legacy_pre_timeline',
                    0, 0, 0, NULL, strftime('%s', 'now')
             FROM (
               SELECT account_key FROM accounts
               UNION SELECT account_key FROM turn_usage
               UNION SELECT account_key FROM turn_token_samples
             )",
            [],
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
    // These columns are intentionally additive so opening a database created
    // by an earlier collector never discards its cursor or binding history.
    for (column, definition) in [
        ("file_identity", "TEXT NOT NULL DEFAULT ''"),
        ("health_status", "TEXT NOT NULL DEFAULT 'unknown'"),
        ("last_error", "TEXT"),
        ("last_error_at", "INTEGER"),
        ("cursor_state_json", "TEXT NOT NULL DEFAULT '{}'"),
        ("file_fingerprint", "TEXT"),
        ("cursor_prefix_fingerprint", "TEXT"),
        ("generation", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if table_exists(connection, "rollout_sources")?
            && !has_column(connection, "rollout_sources", column)?
        {
            connection
                .execute(
                    &format!("ALTER TABLE rollout_sources ADD COLUMN {column} {definition}"),
                    [],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    let migration = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    migrate_legacy_rollout_sources(&migration)?;
    quarantine_legacy_non_verified_ledgers(&migration)?;
    repair_rollout_source_identities(&migration)?;
    migration
        .execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_rollout_sources_identity
             ON rollout_sources(file_identity);",
        )
        .map_err(|error| error.to_string())?;
    migration.commit().map_err(|error| error.to_string())?;
    Ok(())
}

fn repair_rollout_source_identities(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare("SELECT source_id, canonical_path, file_identity FROM rollout_sources")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (source_id, canonical_path, identity) in rows {
        let duplicate: bool = connection
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM rollout_sources other
                   WHERE other.file_identity = ?1 AND other.source_id != ?2
                 )",
                rusqlite::params![identity, source_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if identity.is_empty() || duplicate {
            let path = Path::new(&canonical_path);
            let base = super::collector_core::file_identity(path)
                .unwrap_or_else(|_| format!("legacy-path:{canonical_path}"));
            let repaired = if duplicate {
                format!("{base}:legacy-source:{source_id}")
            } else {
                base
            };
            connection
                .execute(
                    "UPDATE rollout_sources SET file_identity = ?2 WHERE source_id = ?1",
                    rusqlite::params![source_id, repaired],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn migrate_legacy_rollout_sources(connection: &Connection) -> Result<(), String> {
    if !table_exists(connection, "rollout_sources")?
        || !table_exists(connection, "rollout_file_bindings")?
    {
        return Ok(());
    }

    #[derive(Default)]
    struct LegacySource {
        path: String,
        account_key: Option<String>,
        status: Option<String>,
        first_seen_at: i64,
        last_seen_at: i64,
    }

    let mut statement = connection
        .prepare(
            "SELECT file_path, account_key, status, first_seen_at, last_seen_at
             FROM rollout_file_bindings
             UNION ALL
             SELECT file_path, NULL, NULL, last_scanned_at, last_scanned_at
             FROM rollout_cursors",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut entries: Vec<LegacySource> = Vec::new();
    for row in rows {
        let (path, account_key, status, first_seen_at, last_seen_at) =
            row.map_err(|error| error.to_string())?;
        if let Some(existing) = entries.iter_mut().find(|entry| entry.path == path) {
            if account_key.is_some() {
                existing.account_key = account_key;
            }
            if status.is_some() {
                existing.status = status;
            }
            existing.first_seen_at = existing.first_seen_at.min(first_seen_at);
            existing.last_seen_at = existing.last_seen_at.max(last_seen_at);
        } else {
            entries.push(LegacySource {
                path,
                account_key,
                status,
                first_seen_at,
                last_seen_at,
            });
        }
    }
    drop(statement);

    for entry in entries {
        let raw_path = Path::new(&entry.path);
        let canonical_path = raw_path
            .canonicalize()
            .unwrap_or_else(|_| raw_path.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let identity = super::collector_core::file_identity(raw_path)
            .unwrap_or_else(|_| format!("legacy-path:{canonical_path}"));
        let proposed_source_id = super::collector_core::source_id_for_identity(&identity);
        let source_id: String = connection
            .query_row(
                "SELECT source_id FROM rollout_sources
                 WHERE source_id = ?1 OR file_identity = ?2 OR canonical_path = ?3 LIMIT 1",
                rusqlite::params![proposed_source_id, identity, canonical_path],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .unwrap_or(proposed_source_id);
        let now = entry.last_seen_at.max(entry.first_seen_at);
        let legacy_cursor: Option<(i64, Option<i64>, String)> = connection
            .query_row(
                "SELECT byte_offset, last_modified_at, state_json
                 FROM rollout_cursors WHERE file_path = ?1",
                [&entry.path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let (legacy_offset, legacy_mtime, legacy_state) =
            legacy_cursor.unwrap_or((0, None, "{}".into()));
        let (last_size, file_fingerprint, prefix_fingerprint) = if raw_path.is_file() {
            let size = fs::metadata(raw_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let full = super::collector_core::file_fingerprint(raw_path).ok();
            let prefix = super::collector_core::file_prefix_fingerprint(
                raw_path,
                legacy_offset.max(0) as u64,
            )
            .ok();
            (size as i64, full, prefix)
        } else {
            (0, None, None)
        };
        let binding_verified =
            entry.account_key.is_some() && entry.status.as_deref() == Some("bound");
        connection
            .execute(
                "INSERT OR IGNORE INTO rollout_sources
                 (source_id, canonical_path, file_identity, account_key, binding_status,
                  binding_source, binding_confidence, first_seen_at, last_offset, last_size,
                  last_mtime, cursor_state_json, file_fingerprint,
                  cursor_prefix_fingerprint, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'legacy_migration',
                         CASE WHEN ?5 = 'verified' THEN 'high' ELSE 'unknown' END,
                         ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?6, ?13)",
                rusqlite::params![
                    source_id,
                    canonical_path,
                    identity,
                    if binding_verified {
                        entry.account_key.clone()
                    } else {
                        None
                    },
                    if binding_verified {
                        "verified"
                    } else {
                        "unresolved"
                    },
                    entry.first_seen_at,
                    legacy_offset.max(0),
                    last_size,
                    legacy_mtime,
                    legacy_state.clone(),
                    file_fingerprint.clone(),
                    prefix_fingerprint.clone(),
                    now,
                ],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "UPDATE rollout_sources SET
                   account_key = CASE
                     WHEN binding_status = 'verified' THEN account_key
                     WHEN ?2 = 'verified' THEN ?3 ELSE NULL END,
                   binding_status = CASE
                     WHEN binding_status = 'verified' THEN binding_status
                     WHEN ?2 = 'verified' THEN 'verified' ELSE binding_status END,
                   binding_source = CASE
                     WHEN binding_status = 'verified' THEN binding_source
                     WHEN ?2 = 'verified' THEN 'legacy_migration' ELSE 'unresolved' END,
                   binding_confidence = CASE
                     WHEN binding_status = 'verified' THEN binding_confidence
                     WHEN ?2 = 'verified' THEN 'high' ELSE 'unknown' END,
                   last_offset = CASE WHEN last_offset = 0 THEN ?4 ELSE last_offset END,
                   last_size = CASE WHEN last_offset = 0 THEN ?5 ELSE last_size END,
                   last_mtime = CASE WHEN last_offset = 0 THEN ?6 ELSE last_mtime END,
                   cursor_state_json = CASE WHEN last_offset = 0 THEN ?7 ELSE cursor_state_json END,
                   file_fingerprint = CASE WHEN last_offset = 0 THEN ?8 ELSE file_fingerprint END,
                   cursor_prefix_fingerprint = CASE WHEN last_offset = 0 THEN ?9 ELSE cursor_prefix_fingerprint END,
                   updated_at = ?10
                 WHERE source_id = ?1",
                rusqlite::params![
                    source_id,
                    if binding_verified { "verified" } else { "unresolved" },
                    entry.account_key.clone(),
                    legacy_offset.max(0),
                    last_size,
                    legacy_mtime,
                    legacy_state,
                    file_fingerprint,
                    prefix_fingerprint,
                    now,
                ],
            )
            .map_err(|error| error.to_string())?;
        let final_status: String = connection
            .query_row(
                "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                [&source_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if binding_verified && entry.account_key.is_some() && final_status == "verified" {
            let already_audited: bool = connection
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM source_binding_audits
                       WHERE source_id = ?1
                         AND old_account_key = ?2
                         AND new_account_key = ?2
                         AND new_status = 'verified'
                         AND reason = 'legacy_verified_binding_import'
                     )",
                    rusqlite::params![source_id, entry.account_key.as_deref()],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if !already_audited {
                connection
                    .execute(
                        "INSERT INTO source_binding_audits
                         (source_id, old_account_key, new_account_key, old_status,
                          new_status, reason, evidence, changed_at)
                         VALUES (?1, ?2, ?2, 'bound', 'verified',
                                 'legacy_verified_binding_import', ?3, ?4)",
                        rusqlite::params![
                            source_id,
                            entry.account_key.as_deref(),
                            "legacy rollout_file_bindings status=bound",
                            now,
                        ],
                    )
                    .map_err(|error| error.to_string())?;
            }
        } else if !binding_verified && entry.account_key.is_some() && final_status == "unresolved" {
            let already_audited: bool = connection
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM source_binding_audits
                       WHERE source_id = ?1
                         AND old_account_key = ?2
                         AND new_account_key IS NULL
                         AND reason = 'legacy_non_verified_account_cleared'
                     )",
                    rusqlite::params![source_id, entry.account_key.as_deref()],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if !already_audited {
                connection
                    .execute(
                        "INSERT INTO source_binding_audits
                         (source_id, old_account_key, new_account_key, old_status,
                          new_status, reason, evidence, changed_at)
                         VALUES (?1, ?2, NULL, ?3, 'unresolved',
                                 'legacy_non_verified_account_cleared', ?4, ?5)",
                        rusqlite::params![
                            source_id,
                            entry.account_key.as_deref(),
                            entry.status.as_deref().unwrap_or("unknown"),
                            entry.status.as_deref().unwrap_or("unknown"),
                            now,
                        ],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

fn quarantine_legacy_non_verified_ledgers(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT account_key FROM rollout_file_bindings
             WHERE account_key IS NOT NULL AND status != 'bound'",
        )
        .map_err(|error| error.to_string())?;
    let accounts = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for account_key in accounts {
        super::rollout::quarantine_unverified_account_data(connection, &account_key)?;
        super::rollout::set_account_data_health(
            connection,
            &account_key,
            super::rollout::DATA_HEALTH_LEGACY_UNVERIFIED,
            0,
        )?;
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

fn migrate_rollout_file_binding_status(connection: &Connection) -> Result<(), String> {
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'rollout_file_bindings'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(sql) = sql else {
        return Ok(());
    };
    if sql.contains("'pending'") {
        return Ok(());
    }
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute_batch(
            "
            ALTER TABLE rollout_file_bindings RENAME TO rollout_file_bindings_legacy;
            CREATE TABLE rollout_file_bindings (
                file_path TEXT PRIMARY KEY,
                account_key TEXT,
                status TEXT NOT NULL,
                reason TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                last_modified_at INTEGER,
                CHECK (status IN ('bound', 'pending', 'quarantined'))
            );
            INSERT INTO rollout_file_bindings
              (file_path, account_key, status, reason, first_seen_at, last_seen_at)
            SELECT file_path, account_key,
                   CASE WHEN status = 'quarantined' AND account_key IS NULL
                        THEN 'pending' ELSE status END,
                   CASE WHEN status = 'quarantined' AND account_key IS NULL
                        THEN 'legacy_quarantine_pending' ELSE reason END,
                   first_seen_at, last_seen_at
            FROM rollout_file_bindings_legacy;
            DROP TABLE rollout_file_bindings_legacy;
            ",
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
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

    // Before segment identity existed, a token reset could not be
    // represented: the old unique key rejected a repeated cumulative value
    // in the same turn. Rebuild this derived table with the versioned key
    // while retaining existing rows as segment zero.
    if table_exists(&transaction, "turn_token_samples")?
        && !has_column(&transaction, "turn_token_samples", "segment_no")?
    {
        transaction
            .execute_batch(
                "
            ALTER TABLE turn_token_samples RENAME TO turn_token_samples_legacy;
            CREATE TABLE turn_token_samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_key TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                segment_no INTEGER NOT NULL DEFAULT 0,
                model TEXT,
                reasoning_effort TEXT NOT NULL,
                speed_mode TEXT NOT NULL,
                sampled_at INTEGER NOT NULL,
                cumulative_tokens INTEGER NOT NULL,
                delta_tokens INTEGER NOT NULL,
                source TEXT NOT NULL,
                confidence TEXT NOT NULL,
                UNIQUE (account_key, thread_id, turn_id, segment_no, cumulative_tokens),
                CHECK (segment_no >= 0),
                CHECK (cumulative_tokens >= 0),
                CHECK (delta_tokens >= 0)
            );
            INSERT OR IGNORE INTO turn_token_samples
              (account_key, thread_id, turn_id, segment_no, model,
               reasoning_effort, speed_mode, sampled_at, cumulative_tokens,
               delta_tokens, source, confidence)
            SELECT account_key, thread_id, turn_id, 0, model,
                   reasoning_effort, speed_mode, sampled_at, cumulative_tokens,
                   delta_tokens, source, confidence
            FROM turn_token_samples_legacy;
            DROP TABLE turn_token_samples_legacy;
            ",
            )
            .map_err(|error| error.to_string())?;
    }

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
    use crate::usage::collector_core::BINDING_VERIFIED;

    #[test]
    fn creates_v1_tables() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for table in [
            "accounts",
            "usage_migrations",
            "account_usage_data_versions",
            "turn_timeline_audits",
            "rollout_parse_errors",
            "rollout_file_bindings",
            "turn_usage",
            "turn_token_samples",
            "account_daily_usage",
            "thread_usage_group_samples",
            "thread_usage_capabilities",
            "rate_limit_samples",
            "quota_intervals",
            "rollout_cursors",
            "rollout_sources",
            "rollout_turn_sources",
            "account_presence_intervals",
            "collector_sessions",
            "collector_gaps",
            "source_binding_audits",
            "collector_state",
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

    #[test]
    fn backfills_missing_rollout_sources_from_legacy_binding_and_cursor() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE rollout_file_bindings (
                  file_path TEXT PRIMARY KEY, account_key TEXT, status TEXT NOT NULL,
                  reason TEXT, first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL
                );
                INSERT INTO rollout_file_bindings
                  VALUES ('/tmp/codex-archived-missing.jsonl', 'account:a', 'bound', 'legacy', 10, 20);
                CREATE TABLE rollout_cursors (
                  file_path TEXT PRIMARY KEY, byte_offset INTEGER NOT NULL,
                  last_modified_at INTEGER, last_scanned_at INTEGER NOT NULL,
                  state_json TEXT NOT NULL
                );
                INSERT INTO rollout_cursors
                  VALUES ('/tmp/codex-archived-missing.jsonl', 42, 19, 20, '{\"sessionId\":\"s\"}');
                ",
            )
            .unwrap();
        initialize_schema(&connection).unwrap();
        let source: (String, Option<String>, String, i64, String) = connection
            .query_row(
                "SELECT source_id, account_key, binding_status, last_offset, cursor_state_json
                 FROM rollout_sources",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(source.1.as_deref(), Some("account:a"));
        assert_eq!(source.2, BINDING_VERIFIED);
        assert_eq!(source.3, 42);
        assert_eq!(source.4, r#"{"sessionId":"s"}"#);
        initialize_schema(&connection).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM rollout_sources", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT last_offset FROM rollout_sources", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            42
        );
    }

    #[test]
    fn clears_non_verified_legacy_accounts_but_keeps_audit_evidence() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE rollout_file_bindings (
                  file_path TEXT PRIMARY KEY, account_key TEXT, status TEXT NOT NULL,
                  reason TEXT, first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL
                );
                INSERT INTO rollout_file_bindings
                  VALUES
                    ('/tmp/pending-with-old-account.jsonl', 'account:old', 'pending', 'watcher', 10, 20),
                    ('/tmp/quarantined-with-old-account.jsonl', 'account:old2', 'quarantined', 'reset', 11, 21);
                CREATE TABLE rollout_cursors (
                  file_path TEXT PRIMARY KEY, byte_offset INTEGER NOT NULL,
                  last_modified_at INTEGER, last_scanned_at INTEGER NOT NULL,
                  state_json TEXT NOT NULL
                );
                INSERT INTO rollout_cursors VALUES
                  ('/tmp/pending-with-old-account.jsonl', 12, 19, 20, '{}'),
                  ('/tmp/quarantined-with-old-account.jsonl', 13, 20, 21, '{}');
                ",
            )
            .unwrap();

        initialize_schema(&connection).unwrap();
        let rows: Vec<(Option<String>, String)> = connection
            .prepare(
                "SELECT account_key, binding_status FROM rollout_sources
                 ORDER BY canonical_path",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![(None, "unresolved".into()), (None, "unresolved".into())]
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM source_binding_audits
                     WHERE reason = 'legacy_non_verified_account_cleared'
                       AND new_account_key IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        initialize_schema(&connection).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM source_binding_audits
                     WHERE reason = 'legacy_non_verified_account_cleared'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn migrates_intermediate_sources_before_creating_identity_index() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE rollout_sources (
                  source_id TEXT PRIMARY KEY, canonical_path TEXT NOT NULL,
                  session_id TEXT, thread_id TEXT, account_key TEXT,
                  binding_status TEXT NOT NULL DEFAULT 'unresolved',
                  binding_source TEXT NOT NULL DEFAULT 'unresolved',
                  binding_confidence TEXT NOT NULL DEFAULT 'unknown',
                  first_seen_at INTEGER NOT NULL, first_activity_at INTEGER,
                  last_activity_at INTEGER, last_offset INTEGER NOT NULL DEFAULT 0,
                  last_size INTEGER NOT NULL DEFAULT 0, last_mtime INTEGER,
                  cursor_state_json TEXT NOT NULL DEFAULT '{}', generation INTEGER NOT NULL DEFAULT 0,
                  parser_version INTEGER NOT NULL DEFAULT 0, health_status TEXT NOT NULL DEFAULT 'unknown',
                  last_error TEXT, last_error_at INTEGER, created_at INTEGER NOT NULL,
                  updated_at INTEGER NOT NULL
                );
                INSERT INTO rollout_sources
                  (source_id, canonical_path, first_seen_at, created_at, updated_at)
                  VALUES ('source:old', '/tmp/old-intermediate.jsonl', 1, 1, 1);
                ",
            )
            .unwrap();
        initialize_schema(&connection).unwrap();
        assert!(has_column(&connection, "rollout_sources", "file_identity").unwrap());
        assert!(has_column(&connection, "rollout_sources", "cursor_prefix_fingerprint").unwrap());
        let identity: String = connection
            .query_row(
                "SELECT file_identity FROM rollout_sources WHERE source_id = 'source:old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!identity.is_empty());
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'idx_rollout_sources_identity'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn migrates_token_samples_to_segment_aware_identity() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE turn_token_samples (
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
                INSERT INTO turn_token_samples
                  (account_key, thread_id, turn_id, model, reasoning_effort,
                   speed_mode, sampled_at, cumulative_tokens, delta_tokens,
                   source, confidence)
                VALUES ('a', 'thread', 'turn', 'gpt-5.6-sol', 'high',
                        'standard', 1, 100, 100, 'rollout', 'high');
                ",
            )
            .unwrap();

        initialize_schema(&connection).unwrap();

        assert!(has_column(&connection, "turn_token_samples", "segment_no").unwrap());
        let segment: i64 = connection
            .query_row("SELECT segment_no FROM turn_token_samples", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(segment, 0);
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, model,
                  reasoning_effort, speed_mode, sampled_at, cumulative_tokens,
                  delta_tokens, source, confidence)
                 VALUES ('a', 'thread', 'turn', 1, 'gpt-5.6-sol', 'high',
                         'standard', 2, 100, 100, 'rollout', 'high')",
                [],
            )
            .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM turn_token_samples", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            2
        );
    }

    #[test]
    fn migrates_parse_error_and_health_columns_without_losing_legacy_rows() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE rollout_parse_errors (
                  file_path TEXT NOT NULL,
                  byte_offset INTEGER NOT NULL,
                  error TEXT NOT NULL,
                  first_seen_at INTEGER NOT NULL,
                  PRIMARY KEY (file_path, byte_offset)
                );
                INSERT INTO rollout_parse_errors
                  VALUES ('/tmp/legacy.jsonl', 12, 'invalid json', 1);
                CREATE TABLE account_usage_data_versions (
                  account_key TEXT PRIMARY KEY,
                  rollout_parser_version INTEGER NOT NULL DEFAULT 0,
                  status TEXT NOT NULL DEFAULT 'legacy_unverified',
                  timeline_status TEXT NOT NULL DEFAULT 'legacy_pre_timeline',
                  missing_timeline_turns INTEGER NOT NULL DEFAULT 0,
                  orphan_timeline_samples INTEGER NOT NULL DEFAULT 0,
                  mismatched_turns INTEGER NOT NULL DEFAULT 0,
                  verified_at INTEGER,
                  updated_at INTEGER NOT NULL
                );
                INSERT INTO account_usage_data_versions
                  (account_key, updated_at) VALUES ('a', 1);
                ",
            )
            .unwrap();
        initialize_schema(&connection).unwrap();

        assert!(has_column(&connection, "rollout_parse_errors", "account_key").unwrap());
        assert!(has_column(&connection, "rollout_parse_errors", "rebuild_batch_id").unwrap());
        assert!(has_column(
            &connection,
            "account_usage_data_versions",
            "parse_error_count"
        )
        .unwrap());
        assert!(has_column(
            &connection,
            "account_usage_data_versions",
            "last_rebuild_batch_id"
        )
        .unwrap());
        assert_eq!(
            connection
                .query_row("SELECT error FROM rollout_parse_errors", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "invalid json"
        );
    }

    #[test]
    fn migrates_old_quarantine_bindings_to_watcher_pending() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE rollout_file_bindings (
                  file_path TEXT PRIMARY KEY,
                  account_key TEXT,
                  status TEXT NOT NULL,
                  reason TEXT,
                  first_seen_at INTEGER NOT NULL,
                  last_seen_at INTEGER NOT NULL,
                  CHECK (status IN ('bound', 'quarantined'))
                );
                INSERT INTO rollout_file_bindings
                  VALUES ('/tmp/old.jsonl', NULL, 'quarantined',
                          'unattributed_legacy_file', 1, 1);
                ",
            )
            .unwrap();
        initialize_schema(&connection).unwrap();
        let status: String = connection
            .query_row(
                "SELECT status FROM rollout_file_bindings
                 WHERE file_path = '/tmp/old.jsonl'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }
}
