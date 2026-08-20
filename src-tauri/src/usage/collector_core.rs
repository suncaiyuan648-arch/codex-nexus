//! Durable collector boundary.
//!
//! The Tauri scheduler still owns the current process for now, but all state
//! needed to resume collection lives here and in SQLite. This keeps the
//! parser/data-plane contract reusable by a future `nexus-collector` binary
//! without making the first extraction a risky workspace-wide rewrite.

use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{db, recorder, rollout};

pub const BINDING_VERIFIED: &str = "verified";
#[allow(dead_code)]
pub const BINDING_INFERRED: &str = "inferred";
pub const BINDING_UNRESOLVED: &str = "unresolved";
#[allow(dead_code)]
pub const BINDING_QUARANTINED: &str = "quarantined";

static WRITER_TOKEN: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn writer_token_slot() -> &'static Mutex<Option<String>> {
    WRITER_TOKEN.get_or_init(|| Mutex::new(None))
}

pub(crate) fn writer_token_active() -> bool {
    writer_token_slot()
        .lock()
        .map(|token| token.is_some())
        .unwrap_or(false)
}

fn activate_writer_token(token: &str) {
    if let Ok(mut current) = writer_token_slot().lock() {
        *current = Some(token.to_owned());
    }
}

fn deactivate_writer_token(token: &str) {
    if let Ok(mut current) = writer_token_slot().lock() {
        if current.as_deref() == Some(token) {
            *current = None;
        }
    }
}

#[derive(Debug)]
pub struct CollectorLock {
    path: PathBuf,
    token: String,
    _file: File,
}

impl CollectorLock {
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        // The sidecar is a stable handle for the OS advisory lock. Do not
        // use create_new/unlink as ownership: that leaves a race between
        // creating the file and writing the owner token.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("collector singleton lock {}: {error}", path.display()))?;
        if let Err(error) = file.try_lock_exclusive() {
            let owner = fs::read_to_string(&path)
                .ok()
                .and_then(|contents| parse_lock_pid(&contents));
            return Err(match owner {
                Some(pid) => format!(
                    "collector singleton lock {} is held by pid {} ({error})",
                    path.display(),
                    pid
                ),
                None => format!(
                    "collector singleton lock {} is held ({error})",
                    path.display()
                ),
            });
        }
        let token = format!("{}:{}", std::process::id(), unique_suffix());
        file.set_len(0).map_err(|error| error.to_string())?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        writeln!(file, "pid={} token={token}", std::process::id())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            path,
            token,
            _file: file,
        })
    }

    pub(crate) fn is_owner(&self) -> bool {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|contents| contents.lines().next().map(str::to_owned))
            .is_some_and(|line| line.ends_with(&format!("token={}", self.token)))
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}

fn parse_lock_pid(contents: &str) -> Option<u32> {
    contents.lines().find_map(|line| {
        line.strip_prefix("pid=")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

impl Drop for CollectorLock {
    fn drop(&mut self) {
        let owned = fs::read_to_string(&self.path)
            .ok()
            .and_then(|contents| contents.lines().next().map(str::to_owned))
            .is_some_and(|line| line.ends_with(&format!("token={}", self.token)));
        if owned {
            // Keep the sidecar inode. Unlinking it while releasing the
            // advisory lock can let another process create a second inode.
            let _ = self._file.unlock();
        }
    }
}

#[derive(Debug)]
pub struct CollectorSessionGuard {
    lock: Option<CollectorLock>,
    database_path: PathBuf,
    pub session_id: String,
    pub instance_id: String,
}

impl CollectorSessionGuard {
    #[allow(dead_code)]
    pub fn start(
        connection: &Connection,
        database_path: impl Into<PathBuf>,
        lock_path: impl Into<PathBuf>,
        now: i64,
    ) -> Result<Self, String> {
        let lock = CollectorLock::acquire(lock_path)?;
        Self::start_with_lock(connection, database_path, lock, now)
    }

    pub fn start_with_lock(
        connection: &Connection,
        database_path: impl Into<PathBuf>,
        lock: CollectorLock,
        now: i64,
    ) -> Result<Self, String> {
        let database_path = database_path.into();
        let instance_id = format!("instance:{}:{}", std::process::id(), now);
        let session_id = format!("session:{}:{}", std::process::id(), unique_suffix());
        recover_open_sessions(connection, now)?;
        connection
            .execute(
                "INSERT INTO collector_sessions
                 (session_id, collector_instance_id, pid, started_at,
                  last_heartbeat_at, version)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![
                    session_id,
                    instance_id,
                    std::process::id() as i64,
                    now,
                    rollout::ROLLOUT_PARSER_VERSION.to_string()
                ],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO collector_state
                 (id, collector_instance_id, pid, started_at, heartbeat_at, version, status)
                 VALUES (1, ?1, ?2, ?3, ?3, ?4, 'running')
                 ON CONFLICT(id) DO UPDATE SET collector_instance_id = excluded.collector_instance_id,
                   pid = excluded.pid, started_at = excluded.started_at,
                   heartbeat_at = excluded.heartbeat_at, version = excluded.version,
                   status = excluded.status",
                params![
                    instance_id,
                    std::process::id() as i64,
                    now,
                    rollout::ROLLOUT_PARSER_VERSION.to_string()
                ],
            )
            .map_err(|error| error.to_string())?;
        let guard = Self {
            lock: Some(lock),
            database_path,
            session_id,
            instance_id,
        };
        if let Some(lock) = guard.lock.as_ref() {
            activate_writer_token(lock.token());
        }
        Ok(guard)
    }
}

impl Drop for CollectorSessionGuard {
    fn drop(&mut self) {
        if let Ok(connection) = Connection::open(&self.database_path) {
            let _ = connection.busy_timeout(std::time::Duration::from_secs(2));
            let now = now_seconds();
            let _ = stop_session(&connection, &self.session_id, now, &self.instance_id);
            let _ = flush(&connection);
        }
        if let Some(lock) = self.lock.take() {
            deactivate_writer_token(lock.token());
            drop(lock);
        }
    }
}

pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

pub fn recover_open_sessions(connection: &Connection, now: i64) -> Result<usize, String> {
    let mut statement = connection
        .prepare(
            "SELECT session_id, started_at, last_heartbeat_at
             FROM collector_sessions WHERE stopped_at IS NULL",
        )
        .map_err(|error| error.to_string())?;
    let rows: Vec<(String, i64, i64)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|error| error.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (session_id, started_at, heartbeat_at) in &rows {
        let reason = if now.saturating_sub(*heartbeat_at) > 5 * 60 {
            "os_sleep"
        } else {
            "crash"
        };
        if now > *heartbeat_at {
            record_gap(
                connection,
                Some(session_id),
                *heartbeat_at,
                now,
                reason,
                now,
            )?;
        } else if now > *started_at {
            record_gap(
                connection,
                Some(session_id),
                *started_at,
                now,
                "unknown",
                now,
            )?;
        }
        connection
            .execute(
                "UPDATE collector_sessions
                 SET stopped_at = ?2, status = 'recovered', recovery_reason = ?3
                 WHERE session_id = ?1 AND stopped_at IS NULL",
                params![session_id, now, reason],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(rows.len())
}

pub fn touch_session(
    connection: &Connection,
    session_id: &str,
    now: i64,
    instance_id: &str,
) -> Result<(), String> {
    connection
        .execute(
            "UPDATE collector_sessions SET last_heartbeat_at = ?2
             WHERE session_id = ?1 AND collector_instance_id = ?3 AND stopped_at IS NULL",
            params![session_id, now, instance_id],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE collector_state SET heartbeat_at = ?1, status = 'running'
             WHERE id = 1 AND collector_instance_id = ?2",
            params![now, instance_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn stop_session(
    connection: &Connection,
    session_id: &str,
    now: i64,
    instance_id: &str,
) -> Result<(), String> {
    connection
        .execute(
            "UPDATE collector_sessions SET stopped_at = ?2, last_heartbeat_at = ?2,
             status = 'stopped' WHERE session_id = ?1 AND collector_instance_id = ?3",
            params![session_id, now, instance_id],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE collector_state SET heartbeat_at = ?1, status = 'stopped'
             WHERE id = 1 AND collector_instance_id = ?2",
            params![now, instance_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn record_gap(
    connection: &Connection,
    session_id: Option<&str>,
    start_at: i64,
    end_at: i64,
    reason: &str,
    created_at: i64,
) -> Result<(), String> {
    let start_at = start_at.min(end_at);
    let end_at = end_at.max(start_at);
    connection
        .execute(
            "INSERT INTO collector_gaps
             (session_id, start_at, end_at, duration_ms, reason, created_at)
             VALUES (?1, ?2, ?3, (?3 - ?2) * 1000, ?4, ?5)",
            params![session_id, start_at, end_at, reason, created_at],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Make the graceful-shutdown boundary explicit. Every parser write is
/// already transactional; this checkpoint only asks SQLite to make WAL pages
/// available to the next reader before the collector process exits.
pub fn flush(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .map_err(|error| error.to_string())
}

pub(crate) fn file_identity(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("dev:{}:ino:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        // std::fs does not expose Windows volume serial/file-index pairs.
        // Keep the path as the continuity key and use the persisted full and
        // prefix SHA-256 fingerprints to conservatively detect replacement.
        // A false replacement is safe; silently appending to a replacement is
        // not.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        Ok(format!("path:{}", canonical.to_string_lossy()))
    }
}

/// Full-content fingerprint is intentionally conservative. It catches an
/// in-place same-size replacement even when the platform reuses the same
/// inode/path; a false positive only causes a safe replay from byte zero.
pub(crate) fn file_fingerprint(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn file_prefix_fingerprint(path: &Path, length: u64) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        let read = file
            .read(&mut buffer[..requested])
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn source_id_for_identity(identity: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identity.as_bytes());
    format!("source:{}", hex::encode(hasher.finalize()))
}

fn prepare_source_cursor(
    connection: &Connection,
    source_id: &str,
    path: &Path,
) -> Result<(i64, u64, bool), String> {
    let (last_offset, last_size, last_fingerprint, last_prefix_fingerprint): (
        i64,
        i64,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            "SELECT last_offset, last_size, file_fingerprint, cursor_prefix_fingerprint
             FROM rollout_sources WHERE source_id = ?1",
            [source_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| error.to_string())?;
    let current_size = fs::metadata(path).map_err(|error| error.to_string())?.len();
    let current_fingerprint = file_fingerprint(path)?;
    let prefix_changed = if last_offset > 0 {
        match last_prefix_fingerprint {
            Some(expected) => file_prefix_fingerprint(path, last_offset as u64)? != expected,
            // Old databases did not persist a prefix digest. Replaying is
            // safer than treating an unverified larger replacement as append.
            None => true,
        }
    } else {
        false
    };
    let generation_changed = current_size < last_size.max(last_offset) as u64
        || prefix_changed
        || (current_size == last_size as u64
            && last_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| fingerprint != current_fingerprint));
    if generation_changed {
        // Treat same-size/inode replacement as a new parser generation. Remove
        // only rows exclusively owned by this source; rows shared with another
        // source remain intact. This prevents replaying a replacement file
        // from silently doubling the old generation's token timeline.
        for table in ["turn_token_samples", "turn_usage", "turn_timeline_audits"] {
            connection
                .execute(
                    &format!(
                        "DELETE FROM {table} WHERE EXISTS (
                           SELECT 1 FROM rollout_turn_sources s
                           WHERE s.source_id = ?1 AND s.account_key = {table}.account_key
                             AND s.thread_id = {table}.thread_id
                             AND s.turn_id = {table}.turn_id
                         ) AND NOT EXISTS (
                           SELECT 1 FROM rollout_turn_sources other
                           WHERE other.source_id != ?1
                             AND other.account_key = {table}.account_key
                             AND other.thread_id = {table}.thread_id
                             AND other.turn_id = {table}.turn_id
                         )"
                    ),
                    [source_id],
                )
                .map_err(|error| error.to_string())?;
        }
        connection
            .execute(
                "DELETE FROM rollout_turn_sources WHERE source_id = ?1",
                [source_id],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "UPDATE rollout_sources
                 SET last_offset = 0, last_size = 0, file_fingerprint = NULL,
                     cursor_prefix_fingerprint = NULL,
                     cursor_state_json = '{}', generation = generation + 1,
                     updated_at = ?2 WHERE source_id = ?1",
                params![source_id, now_seconds()],
            )
            .map_err(|error| error.to_string())?;
        Ok((0, current_size, true))
    } else {
        Ok((last_offset.max(0), current_size, false))
    }
}

#[allow(clippy::too_many_arguments)]
fn audit_binding(
    connection: &Connection,
    source_id: &str,
    old_account_key: Option<&str>,
    new_account_key: Option<&str>,
    old_status: Option<&str>,
    new_status: &str,
    reason: &str,
    evidence: Option<&str>,
    changed_at: i64,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO source_binding_audits
             (source_id, old_account_key, new_account_key, old_status, new_status,
              reason, evidence, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                source_id,
                old_account_key,
                new_account_key,
                old_status,
                new_status,
                reason,
                evidence,
                changed_at
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn quarantine_replaced_source(
    connection: &Connection,
    source_id: &str,
    old_account_key: Option<&str>,
    now: i64,
) -> Result<(), String> {
    let owned_turns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM rollout_turn_sources WHERE source_id = ?1",
            [source_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;

    if owned_turns == 0 {
        // Old migrations did not always create rollout_turn_sources. There is
        // no safe way to attribute those account-key-only rows to a source,
        // so quarantine the complete legacy account rather than silently
        // allowing the replacement generation to inherit it.
        if let Some(account_key) = old_account_key {
            rollout::quarantine_unverified_account_data(connection, account_key)?;
            rollout::set_account_data_health(connection, account_key, "legacy_unverified", 0)?;
        }
    } else {
        for table in ["turn_token_samples", "turn_usage", "turn_timeline_audits"] {
            connection
                .execute(
                    &format!(
                        "DELETE FROM {table} WHERE EXISTS (
                           SELECT 1 FROM rollout_turn_sources owned
                           WHERE owned.source_id = ?1
                             AND owned.account_key = {table}.account_key
                             AND owned.thread_id = {table}.thread_id
                             AND owned.turn_id = {table}.turn_id
                         ) AND NOT EXISTS (
                           SELECT 1 FROM rollout_turn_sources other
                           WHERE other.source_id != ?1
                             AND other.account_key = {table}.account_key
                             AND other.thread_id = {table}.thread_id
                             AND other.turn_id = {table}.turn_id
                         )"
                    ),
                    [source_id],
                )
                .map_err(|error| error.to_string())?;
        }
        connection
            .execute(
                "DELETE FROM rollout_parse_errors WHERE source_id = ?1",
                [source_id],
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "DELETE FROM rollout_turn_sources WHERE source_id = ?1",
                [source_id],
            )
            .map_err(|error| error.to_string())?;
        if let Some(account_key) = old_account_key {
            super::quota::rebuild_account_intervals(connection, account_key)?;
            rollout::refresh_account_data_health(connection, account_key)?;
        }
    }

    connection
        .execute(
            "UPDATE rollout_sources SET canonical_path = canonical_path || '#archived:' || source_id,
             file_identity = file_identity || '#archived:' || source_id,
             account_key = NULL, binding_status = 'quarantined',
             binding_source = 'replacement_generation', binding_confidence = 'unknown',
             health_status = 'historical_reset', last_error = 'source replacement',
             last_error_at = ?2, updated_at = ?2 WHERE source_id = ?1",
            params![source_id, now],
        )
        .map_err(|error| error.to_string())?;
    audit_binding(
        connection,
        source_id,
        old_account_key,
        None,
        Some(BINDING_VERIFIED),
        BINDING_QUARANTINED,
        "replacement_generation_quarantined",
        Some("file identity/fingerprint changed; historical ledger quarantined"),
        now,
    )
}

pub fn register_source(connection: &Connection, path: &Path, now: i64) -> Result<String, String> {
    let canonical_path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let raw_path = path.to_string_lossy().into_owned();
    let identity = file_identity(path)?;
    let proposed_source_id = source_id_for_identity(&identity);
    let existing: Option<(
        String,
        String,
        Option<String>,
        String,
        String,
        i64,
        i64,
        Option<String>,
        Option<String>,
        String,
    )> = connection
        .query_row(
            "SELECT source_id, canonical_path, account_key, binding_status, file_identity,
                    last_offset, last_size, file_fingerprint,
                    cursor_prefix_fingerprint, health_status
             FROM rollout_sources
             WHERE file_identity = ?2 OR canonical_path = ?3 OR canonical_path = ?4
             ORDER BY CASE WHEN source_id = ?1 AND file_identity = ?2 THEN 0
                           WHEN file_identity = ?2 THEN 1
                           ELSE 2 END LIMIT 1",
            params![proposed_source_id, identity, canonical_path, raw_path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;

    if let Some((
        old_source_id,
        old_canonical_path,
        old_account,
        old_status,
        old_identity,
        old_offset,
        old_size,
        old_fingerprint,
        old_prefix_fingerprint,
        health,
    )) = existing.as_ref()
    {
        let identity_changed = old_identity != &identity;
        let path_changed = old_canonical_path != &canonical_path && old_canonical_path != &raw_path;
        let current_size = fs::metadata(path).map_err(|error| error.to_string())?.len();
        let prefix_changed = if *old_offset > 0 {
            match old_prefix_fingerprint.as_deref() {
                Some(expected) => file_prefix_fingerprint(path, *old_offset as u64)? != expected,
                None => !path_changed,
            }
        } else {
            false
        };
        let current_full_fingerprint = if current_size == (*old_size).max(0) as u64 {
            Some(file_fingerprint(path)?)
        } else {
            None
        };
        let full_fingerprint_changed = old_fingerprint
            .as_deref()
            .zip(current_full_fingerprint.as_deref())
            .is_some_and(|(expected, current)| expected != current);
        let content_replaced = prefix_changed || full_fingerprint_changed;
        let returning_after_reset = identity_changed
            && matches!(
                old_status.as_str(),
                BINDING_QUARANTINED | BINDING_UNRESOLVED
            )
            && health == "historical_reset"
            && *old_offset == 0;
        let replacement = (identity_changed || content_replaced)
            && !returning_after_reset
            && (old_status == BINDING_VERIFIED || *old_offset > 0 || health != "unknown");
        if replacement {
            let transaction = connection
                .unchecked_transaction()
                .map_err(|error| error.to_string())?;
            quarantine_replaced_source(&transaction, old_source_id, old_account.as_deref(), now)?;
            let replacement_source_id = if old_identity == &identity {
                source_id_for_identity(&format!("{identity}:replacement:{old_source_id}:{now}"))
            } else {
                proposed_source_id.clone()
            };
            transaction
                .execute(
                    "INSERT INTO rollout_sources
                     (source_id, canonical_path, file_identity, first_seen_at,
                      created_at, updated_at, binding_status, binding_source,
                      binding_confidence, health_status)
                     VALUES (?1, ?2, ?3, ?4, ?4, ?4, 'unresolved',
                             'replacement_generation', 'unknown', 'unknown')",
                    params![replacement_source_id, canonical_path, identity, now],
                )
                .map_err(|error| error.to_string())?;
            audit_binding(
                &transaction,
                &replacement_source_id,
                None,
                None,
                None,
                BINDING_UNRESOLVED,
                "replacement_generation_registered",
                Some("replacement history starts unresolved"),
                now,
            )?;
            transaction.commit().map_err(|error| error.to_string())?;
            return Ok(replacement_source_id);
        }
    }
    let source_id = existing
        .as_ref()
        .map(|entry| entry.0.clone())
        .unwrap_or(proposed_source_id);
    connection
        .execute(
            "INSERT INTO rollout_sources
             (source_id, canonical_path, file_identity, first_seen_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4, ?4)
             ON CONFLICT(source_id) DO UPDATE SET canonical_path = excluded.canonical_path,
               file_identity = excluded.file_identity,
               generation = CASE WHEN rollout_sources.file_identity != excluded.file_identity
                                  THEN rollout_sources.generation + 1
                                  ELSE rollout_sources.generation END,
               updated_at = excluded.updated_at",
            params![source_id, canonical_path, identity, now],
        )
        .map_err(|error| error.to_string())?;

    if existing.is_none() {
        audit_binding(
            connection,
            &source_id,
            None,
            None,
            None,
            BINDING_UNRESOLVED,
            "source_registered",
            None,
            now,
        )?;
    }
    Ok(source_id)
}

fn source_binding(
    connection: &Connection,
    source_id: &str,
) -> Result<(Option<String>, String), String> {
    connection
        .query_row(
            "SELECT CASE WHEN binding_status = 'verified' THEN account_key ELSE NULL END,
                    binding_status
             FROM rollout_sources WHERE source_id = ?1",
            [source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())
}

/// Return source paths persisted by the registry, including files that are
/// outside the current discovery roots. Startup uses this to resume known
/// sources after a restart or an archive/root-layout change.
pub fn registered_source_paths(connection: &Connection) -> Result<Vec<PathBuf>, String> {
    let mut statement = connection
        // Source IDs are content hashes and intentionally do not encode the
        // user-visible path. Keep service traversal deterministic by the
        // canonical path instead of hash ordering, so per-source account
        // evidence cannot be assigned by an unstable source-id order.
        .prepare("SELECT canonical_path FROM rollout_sources ORDER BY canonical_path")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    rows.map(|row| row.map(PathBuf::from).map_err(|error| error.to_string()))
        .collect()
}

/// Cheap preflight used by the standalone service before it asks Codex for
/// account identity. Most registered rollout files are historical and do not
/// change between refreshes; re-reading account/read for every one of them
/// turns a five-second refresh into an unbounded RPC loop. A source still goes
/// through the full fingerprint/replacement checks when its metadata changes,
/// and direct catch-up callers retain the conservative deep check.
pub fn source_needs_refresh(connection: &Connection, path: &Path) -> Result<bool, String> {
    let canonical_path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let raw_path = path.to_string_lossy().into_owned();
    let previous: Option<(String, i64, Option<i64>)> = connection
        .query_row(
            "SELECT file_identity, last_size, last_mtime
             FROM rollout_sources
             WHERE canonical_path = ?1 OR canonical_path = ?2
             ORDER BY updated_at DESC LIMIT 1",
            params![canonical_path, raw_path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let current_size = metadata.len() as i64;
    let current_identity = file_identity(path)?;
    let current_mtime = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64);

    Ok(previous.is_none_or(|(identity, last_size, last_mtime)| {
        identity != current_identity
            || last_size != current_size
            || last_mtime.is_none()
            || last_mtime != current_mtime
    }))
}

#[allow(clippy::too_many_arguments)]
pub fn bind_source(
    connection: &Connection,
    source_id: &str,
    account_key: &str,
    binding_source: &str,
    confidence: &str,
    reason: &str,
    evidence: Option<&str>,
    now: i64,
) -> Result<bool, String> {
    let (old_account, old_status) = source_binding(connection, source_id)?;
    if old_status == BINDING_VERIFIED {
        return Ok(old_account.as_deref() == Some(account_key));
    }
    connection
        .execute(
            "UPDATE rollout_sources SET account_key = ?2, binding_status = 'verified',
             binding_source = ?3, binding_confidence = ?4, updated_at = ?5
             WHERE source_id = ?1",
            params![source_id, account_key, binding_source, confidence, now],
        )
        .map_err(|error| error.to_string())?;
    audit_binding(
        connection,
        source_id,
        old_account.as_deref(),
        Some(account_key),
        Some(&old_status),
        BINDING_VERIFIED,
        reason,
        evidence,
        now,
    )?;
    Ok(true)
}

pub fn catch_up_path(
    connection: &Connection,
    path: &Path,
    active_account: Option<&str>,
    allow_new_binding: bool,
    now: i64,
) -> Result<bool, String> {
    let source_id = register_source(connection, path, now)?;
    let (before_offset, current_size, generation_changed) =
        prepare_source_cursor(connection, &source_id, path)?;
    let (account_key, mut status) = source_binding(connection, &source_id)?;
    let ledger_account = account_key.unwrap_or_else(|| format!("unresolved:{source_id}"));
    let health_status: String = connection
        .query_row(
            "SELECT health_status FROM rollout_sources WHERE source_id = ?1",
            [&source_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if matches!(status.as_str(), BINDING_QUARANTINED | BINDING_UNRESOLVED)
        && health_status == "historical_reset"
        && before_offset == 0
        && current_size > 0
    {
        // A source that disappeared during reset may return with its old
        // history plus a new tail. Rebase the durable parser state through
        // the returned file without creating ledger rows; the next append can
        // then be bound using real tail evidence.
        rollout::advance_source_cursor_without_persisting(
            connection,
            &source_id,
            path,
            &ledger_account,
        )?;
        return Ok(false);
    }
    recorder::ensure_account(connection, &ledger_account, now)?;
    let batch = format!("collector:{}:{}", rollout::ROLLOUT_PARSER_VERSION, now);
    let collected = rollout::collect_one_for_source(
        connection,
        path,
        &ledger_account,
        &batch,
        Some(&source_id),
    );
    let changed = match collected {
        Ok(changed) => changed,
        Err(error) => {
            connection
                .execute(
                    "UPDATE rollout_sources SET health_status = 'error', last_error = ?2,
                     last_error_at = ?3, updated_at = ?3 WHERE source_id = ?1",
                    params![source_id, error, now],
                )
                .map_err(|db_error| db_error.to_string())?;
            return Err(error);
        }
    };
    let cursor: Option<(i64, String)> = connection
        .query_row(
            "SELECT last_offset, cursor_state_json FROM rollout_sources WHERE source_id = ?1",
            [&source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let (_offset, state_json) = cursor.unwrap_or_default();
    // A watcher notification for a newly discovered file is not ownership
    // evidence: the entire file may be offline history. Only an already
    // consumed source with a normal append can use the currently-online
    // account as retroactive evidence. Replacements and generation resets
    // must remain unresolved until a subsequent live tail exists.
    let live_tail =
        before_offset > 0 && current_size > before_offset as u64 && !generation_changed && changed;
    if status != BINDING_VERIFIED && allow_new_binding && live_tail {
        if let Some(active_account) = active_account {
            retroactively_bind_source(
                connection,
                &source_id,
                active_account,
                "new token content observed while account evidence was online",
                now,
            )?;
            status = BINDING_VERIFIED.into();
        }
    }
    let state: serde_json::Value = serde_json::from_str(&state_json).unwrap_or_default();
    let session_id = state
        .get("sessionId")
        .and_then(|value| value.as_str())
        .or_else(|| state.get("session_id").and_then(|value| value.as_str()));
    let thread_id = state
        .get("threadId")
        .and_then(|value| value.as_str())
        .or_else(|| state.get("thread_id").and_then(|value| value.as_str()));
    connection
        .execute(
            "UPDATE rollout_sources SET session_id = COALESCE(?2, session_id),
             thread_id = COALESCE(?3, thread_id), account_key = CASE
               WHEN binding_status = 'verified' THEN account_key ELSE NULL END,
             parser_version = ?4, health_status = 'healthy', updated_at = ?5,
             first_activity_at = COALESCE(first_activity_at, CASE WHEN last_offset > 0 THEN ?5 END),
             last_activity_at = CASE WHEN last_offset > 0 THEN ?5 ELSE last_activity_at END
             WHERE source_id = ?1",
            params![
                source_id,
                session_id,
                thread_id,
                rollout::ROLLOUT_PARSER_VERSION,
                now,
            ],
        )
        .map_err(|error| error.to_string())?;
    // Keep unresolved explicit even when the parser had no token rows.
    if status != BINDING_VERIFIED {
        connection
            .execute(
                "UPDATE rollout_sources SET binding_status = 'unresolved',
                 binding_source = 'unresolved', binding_confidence = 'unknown'
                 WHERE source_id = ?1 AND binding_status != 'verified'",
                [source_id.as_str()],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(changed)
}

pub fn record_account_presence(
    connection: &Connection,
    account_key: &str,
    started_at: i64,
    source: &str,
    confidence: &str,
    instance_id: Option<&str>,
) -> Result<(), String> {
    let current: Option<(i64, String, i64)> = connection
        .query_row(
            "SELECT id, account_key, started_at FROM account_presence_intervals
             WHERE ended_at IS NULL ORDER BY started_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    match current {
        Some((id, current_account, current_started_at)) if current_account == account_key => {
            if started_at <= current_started_at {
                return Ok(());
            }
            connection
                .execute(
                    "UPDATE account_presence_intervals SET source = ?2, confidence = ?3
                     WHERE id = ?1",
                    params![id, source, confidence],
                )
                .map_err(|error| error.to_string())?;
        }
        Some((id, _, current_started_at)) if started_at <= current_started_at => {
            // A delayed observation must not close an interval that began
            // after it. Keep the durable timeline monotonic and ignore it.
            let _ = id;
            return Ok(());
        }
        Some((id, _, _)) => {
            connection
                .execute(
                    "UPDATE account_presence_intervals SET ended_at = ?2 WHERE id = ?1",
                    params![id, started_at.max(0)],
                )
                .map_err(|error| error.to_string())?;
            insert_presence(
                connection,
                account_key,
                started_at,
                source,
                confidence,
                instance_id,
            )?;
        }
        None => insert_presence(
            connection,
            account_key,
            started_at,
            source,
            confidence,
            instance_id,
        )?,
    }
    Ok(())
}

fn insert_presence(
    connection: &Connection,
    account_key: &str,
    started_at: i64,
    source: &str,
    confidence: &str,
    instance_id: Option<&str>,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO account_presence_intervals
             (account_key, account_id, email, started_at, source, confidence,
              collector_instance_id)
             SELECT ?1, ?1, email, ?2, ?3, ?4, ?5 FROM accounts WHERE account_key = ?1
             UNION ALL SELECT ?1, ?1, NULL, ?2, ?3, ?4, ?5
             WHERE NOT EXISTS (SELECT 1 FROM accounts WHERE account_key = ?1)",
            params![account_key, started_at, source, confidence, instance_id],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn retroactively_bind_source(
    connection: &Connection,
    source_id: &str,
    account_key: &str,
    evidence: &str,
    now: i64,
) -> Result<bool, String> {
    let (old_account, status) = source_binding(connection, source_id)?;
    if status == BINDING_VERIFIED {
        return Ok(old_account.as_deref() == Some(account_key));
    }
    let unresolved = old_account.unwrap_or_else(|| format!("unresolved:{source_id}"));
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    recorder::ensure_account(&transaction, account_key, now)?;
    transaction
        .execute(
            "DELETE FROM turn_token_samples
             WHERE account_key = ?3 AND EXISTS (
               SELECT 1 FROM rollout_turn_sources s
               WHERE s.source_id = ?2 AND s.account_key = ?1
                 AND s.thread_id = turn_token_samples.thread_id
                 AND s.turn_id = turn_token_samples.turn_id
            ) AND EXISTS (
               SELECT 1 FROM turn_token_samples source WHERE source.account_key = ?1
                 AND source.thread_id = turn_token_samples.thread_id
                 AND source.turn_id = turn_token_samples.turn_id
             )",
            params![unresolved, source_id, account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_usage
             WHERE account_key = ?3 AND EXISTS (
               SELECT 1 FROM rollout_turn_sources s
               WHERE s.source_id = ?2 AND s.account_key = ?1
                 AND s.thread_id = turn_usage.thread_id
                 AND s.turn_id = turn_usage.turn_id
             ) AND EXISTS (
               SELECT 1 FROM turn_usage source WHERE source.account_key = ?1
                 AND source.thread_id = turn_usage.thread_id
                 AND source.turn_id = turn_usage.turn_id
             )",
            params![unresolved, source_id, account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_timeline_audits
             WHERE account_key = ?3 AND EXISTS (
               SELECT 1 FROM rollout_turn_sources s
               WHERE s.source_id = ?2 AND s.account_key = ?1
                 AND s.thread_id = turn_timeline_audits.thread_id
                 AND s.turn_id = turn_timeline_audits.turn_id
             ) AND EXISTS (
               SELECT 1 FROM turn_usage source WHERE source.account_key = ?1
                 AND source.thread_id = turn_timeline_audits.thread_id
                 AND source.turn_id = turn_timeline_audits.turn_id
             )",
            params![unresolved, source_id, account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE turn_usage SET account_key = ?2 WHERE account_key = ?1 AND EXISTS (
               SELECT 1 FROM rollout_turn_sources s WHERE s.source_id = ?3
                 AND s.account_key = ?1 AND s.thread_id = turn_usage.thread_id
                 AND s.turn_id = turn_usage.turn_id
             )",
            params![unresolved, account_key, source_id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE turn_token_samples SET account_key = ?2 WHERE account_key = ?1 AND EXISTS (
               SELECT 1 FROM rollout_turn_sources s WHERE s.source_id = ?3
                 AND s.account_key = ?1 AND s.thread_id = turn_token_samples.thread_id
                 AND s.turn_id = turn_token_samples.turn_id
             )",
            params![unresolved, account_key, source_id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE turn_timeline_audits SET account_key = ?2 WHERE account_key = ?1
             AND EXISTS (
               SELECT 1 FROM rollout_turn_sources owned
               WHERE owned.source_id = ?3 AND owned.account_key = ?1
                 AND owned.thread_id = turn_timeline_audits.thread_id
                 AND owned.turn_id = turn_timeline_audits.turn_id
             )
             AND NOT EXISTS (
               SELECT 1 FROM turn_timeline_audits target
               WHERE target.account_key = ?2
                 AND target.thread_id = turn_timeline_audits.thread_id
                 AND target.turn_id = turn_timeline_audits.turn_id
             )",
            params![unresolved, account_key, source_id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE rollout_parse_errors SET account_key = ?2
             WHERE account_key = ?1 AND source_id = ?3",
            params![unresolved, account_key, source_id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM rollout_turn_sources
             WHERE source_id = ?1 AND account_key = ?2
               AND EXISTS (
                 SELECT 1 FROM rollout_turn_sources target
                 WHERE target.source_id = ?1 AND target.account_key = ?3
                   AND target.thread_id = rollout_turn_sources.thread_id
                   AND target.turn_id = rollout_turn_sources.turn_id
               )",
            params![source_id, unresolved, account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE rollout_turn_sources SET account_key = ?2 WHERE source_id = ?1",
            params![source_id, account_key],
        )
        .map_err(|error| error.to_string())?;
    bind_source(
        &transaction,
        source_id,
        account_key,
        "retroactive_activity",
        "high",
        "retroactive_binding",
        Some(evidence),
        now,
    )?;
    // Rebuild all account-scoped derived views after the raw move. These are
    // deterministic and therefore safe to repeat after a crash/retry. Keep
    // them in the same transaction so a failed rebuild cannot expose a
    // half-migrated account.
    super::quota::rebuild_account_intervals(&transaction, account_key)?;
    super::rollout::audit_timeline_gaps(&transaction, Some(account_key), "source_missing")?;
    super::rollout::verify_token_accounting(&transaction, Some(account_key))?;
    super::rollout::refresh_account_data_health(&transaction, account_key)?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(true)
}

pub fn lock_path_for_database(database_path: &Path) -> PathBuf {
    database_path.with_extension("collector.lock")
}

pub fn start_for_app(app: &tauri::AppHandle<tauri::Wry>) -> Result<CollectorSessionGuard, String> {
    let database_path = db::database_path(app)?;
    let lock_path = lock_path_for_database(&database_path);
    let lock = CollectorLock::acquire(&lock_path)?;
    let connection = db::open_database_for_lock(app, &lock)?;
    CollectorSessionGuard::start_with_lock(&connection, &database_path, lock, now_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::db::initialize_schema;
    use std::io::Write;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "codex-nexus-collector-{label}-{}-{}.jsonl",
            std::process::id(),
            unique_suffix()
        ))
    }

    fn rollout_file(path: &Path, total: i64) {
        let mut file = File::create(path).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{{"session_id":"s"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total},"total_tokens":{total}}}}}}}}}"#).unwrap();
    }

    #[test]
    fn known_source_catch_up_is_incremental_and_idempotent() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("known");
        rollout_file(&path, 100);
        let source = register_source(&connection, &path, 10).unwrap();
        bind_source(
            &connection,
            &source,
            "account:a",
            "realtime_account_observation",
            "high",
            "test",
            None,
            10,
        )
        .unwrap();
        assert!(catch_up_path(&connection, &path, Some("account:a"), false, 11).unwrap());
        assert!(!catch_up_path(&connection, &path, Some("account:a"), false, 12).unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT raw_total_tokens FROM turn_usage WHERE account_key = 'account:a'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            100
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT last_offset FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            fs::metadata(&path).unwrap().len() as i64
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unknown_source_is_preserved_but_unresolved_until_evidence_arrives() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("unknown");
        rollout_file(&path, 77);
        let source = register_source(&connection, &path, 10).unwrap();
        assert!(catch_up_path(&connection, &path, None, false, 11).unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            BINDING_UNRESOLVED
        );
        assert_eq!(
            connection
                .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            77
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn first_watcher_discovery_of_offline_history_stays_unresolved() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("offline-history-watcher");
        rollout_file(&path, 77);

        // This is the first event for the source. The active account is not
        // evidence that the complete pre-existing file belongs to it.
        assert!(catch_up_path(&connection, &path, Some("account:a"), true, 11).unwrap());
        let (status, account): (String, Option<String>) = connection
            .query_row(
                "SELECT binding_status, account_key FROM rollout_sources",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, BINDING_UNRESOLVED);
        assert_eq!(account, None);
        let ledger_account: String = connection
            .query_row(
                "SELECT account_key FROM turn_usage WHERE thread_id = 's'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(ledger_account.starts_with("unresolved:source:"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unresolved_source_does_not_bind_on_a_watcher_event_without_growth() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("no-growth");
        rollout_file(&path, 77);
        let source = register_source(&connection, &path, 10).unwrap();
        catch_up_path(&connection, &path, None, false, 11).unwrap();
        assert!(!catch_up_path(&connection, &path, Some("account:a"), true, 12).unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            BINDING_UNRESOLVED
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn retroactive_binding_moves_only_source_rows_and_keeps_verified_binding() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("retro");
        rollout_file(&path, 55);
        let source = register_source(&connection, &path, 10).unwrap();
        catch_up_path(&connection, &path, None, false, 11).unwrap();
        assert!(retroactively_bind_source(
            &connection,
            &source,
            "account:a",
            "later online evidence",
            20
        )
        .unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT account_key FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "account:a"
        );
        assert_eq!(
            connection
                .query_row("SELECT account_key FROM turn_usage", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "account:a"
        );
        assert!(!retroactively_bind_source(
            &connection,
            &source,
            "account:b",
            "ordinary switch",
            21
        )
        .unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT account_key FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "account:a"
        );
        let audits: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM source_binding_audits WHERE source_id = ?1",
                [&source],
                |row| row.get(0),
            )
            .unwrap();
        assert!(audits >= 2);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn retroactive_binding_merges_conflicting_target_turn_deterministically() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("retro-conflict");
        rollout_file(&path, 55);
        let source = register_source(&connection, &path, 10).unwrap();
        catch_up_path(&connection, &path, None, false, 11).unwrap();
        let other_path = temp_path("retro-other-source");
        fs::write(&other_path, "not-json\n").unwrap();
        let other_source = register_source(&connection, &other_path, 10).unwrap();
        catch_up_path(&connection, &other_path, None, false, 11).unwrap();
        let other_account = format!("unresolved:{other_source}");
        connection
            .execute(
                "INSERT INTO rollout_turn_sources
                 (source_id, account_key, thread_id, turn_id, first_seen_at)
                 VALUES (?1, ?2, 's', 't', 11)",
                rusqlite::params![other_source, other_account],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_timeline_audits
                 (account_key, thread_id, turn_id, canonical_tokens, timeline_tokens,
                  reason, first_seen_at, last_seen_at)
                 VALUES (?1, 's', 't', 7, 3, 'accounting_error', 11, 11)",
                [&other_account],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO accounts
                 (account_key, first_seen_at, last_seen_at)
                 VALUES ('account:a', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, completed_at,
                  reasoning_effort, speed_mode, input_tokens, cached_input_tokens,
                  output_tokens, reasoning_output_tokens, raw_total_tokens,
                  source, confidence, created_at, updated_at)
                 VALUES ('account:a', 's', 't', 1, 2, 'high', 'standard',
                         99, 0, 0, 0, 99, 'legacy', 'high', 1, 2)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, reasoning_effort,
                  speed_mode, sampled_at, cumulative_tokens, delta_tokens,
                  source, confidence)
                 VALUES ('account:a', 's', 't', 0, 'high', 'standard', 2, 99, 99,
                         'legacy', 'high')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_timeline_audits
                 (account_key, thread_id, turn_id, canonical_tokens, timeline_tokens,
                  reason, first_seen_at, last_seen_at)
                 VALUES ('account:a', 's', 't', 99, 99, 'legacy', 1, 2)",
                [],
            )
            .unwrap();

        assert!(retroactively_bind_source(
            &connection,
            &source,
            "account:a",
            "online evidence",
            20
        )
        .unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM turn_usage WHERE account_key = 'account:a'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT raw_total_tokens FROM turn_usage WHERE account_key = 'account:a'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            55
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT SUM(delta_tokens) FROM turn_token_samples WHERE account_key = 'account:a'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            55
        );
        rollout::verify_token_accounting(&connection, Some("account:a")).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT account_key FROM turn_timeline_audits
                     WHERE thread_id = 's' AND turn_id = 't'
                       AND account_key = ?1",
                    [&other_account],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            other_account
        );
        let _ = fs::remove_file(other_path);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn online_evidence_retroactively_binds_an_unresolved_source() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("online-retro");
        rollout_file(&path, 31);
        let source = register_source(&connection, &path, 10).unwrap();
        catch_up_path(&connection, &path, None, false, 11).unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        let line = serde_json::json!({
            "timestamp": "2026-08-14T00:00:03Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": { "total_token_usage": { "input_tokens": 50, "total_tokens": 50 } }
            }
        })
        .to_string();
        file.write_all(line.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
        assert!(catch_up_path(&connection, &path, Some("account:a"), true, 20).unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            BINDING_VERIFIED
        );
        assert_eq!(
            connection
                .query_row("SELECT account_key FROM turn_usage", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "account:a"
        );
        assert_eq!(
            connection
                .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            50
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn account_presence_closes_old_interval_without_rewriting_verified_history() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        recorder::ensure_account(&connection, "account:a", 10).unwrap();
        recorder::ensure_account(&connection, "account:b", 20).unwrap();
        record_account_presence(
            &connection,
            "account:a",
            10,
            "account_read",
            "high",
            Some("i"),
        )
        .unwrap();
        record_account_presence(
            &connection,
            "account:b",
            20,
            "account_read",
            "high",
            Some("i"),
        )
        .unwrap();
        let intervals: Vec<(String, Option<i64>)> = connection
            .prepare(
                "SELECT account_key, ended_at FROM account_presence_intervals ORDER BY started_at",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(
            intervals,
            vec![("account:a".into(), Some(20)), ("account:b".into(), None)]
        );
        record_account_presence(
            &connection,
            "account:a",
            15,
            "late_account_read",
            "high",
            Some("i"),
        )
        .unwrap();
        let intervals_after_late: Vec<(String, Option<i64>)> = connection
            .prepare(
                "SELECT account_key, ended_at FROM account_presence_intervals
                 ORDER BY started_at",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(intervals_after_late, intervals);
    }

    #[test]
    fn unresolved_accounts_are_not_current_or_category_inputs() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let unresolved = "unresolved:source:test";
        recorder::ensure_account(&connection, unresolved, 10).unwrap();
        record_account_presence(&connection, unresolved, 10, "rollout", "unknown", None).unwrap();
        connection
            .execute(
                "INSERT INTO rate_limit_samples
                 (account_key, sampled_at, limit_id, window, window_duration_mins,
                  used_percent, resets_at, source, confidence)
                 VALUES (?1, 10, 'codex', 'primary', 10080, 10, 100, 'rollout', 'unknown')",
                [unresolved],
            )
            .unwrap();
        crate::usage::quota::rebuild_account_intervals(&connection, unresolved).unwrap();
        assert_eq!(recorder::current_account_key(&connection).unwrap(), None);
        let usage = crate::usage::category_usage::category_usage(&connection, "day").unwrap();
        assert_eq!(usage.account_key, None);
        assert_eq!(usage.local_tokens, 0);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM quota_intervals", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn moved_file_inherits_source_identity_and_cursor() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let original = temp_path("moved-original");
        let moved = original.with_extension("archived.jsonl");
        rollout_file(&original, 12);
        let source = register_source(&connection, &original, 10).unwrap();
        connection
            .execute(
                "UPDATE rollout_sources SET last_offset = 42 WHERE source_id = ?1",
                [&source],
            )
            .unwrap();
        fs::rename(&original, &moved).unwrap();
        assert_eq!(register_source(&connection, &moved, 20).unwrap(), source);
        assert_eq!(
            connection
                .query_row(
                    "SELECT last_offset FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            42
        );
        let _ = fs::remove_file(moved);
    }

    #[test]
    fn same_size_replacement_changes_generation_and_replays_safely() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("replacement");
        let first = format!(
            "{}\n",
            serde_json::json!({
                "timestamp":"2026-08-14T00:00:00Z",
                "type":"session_meta",
                "payload":{"session_id":"s"}
            })
        ) + &format!(
            "{}\n",
            serde_json::json!({
                "timestamp":"2026-08-14T00:00:01Z",
                "type":"turn_context",
                "payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}
            })
        ) + &format!(
            "{}\n",
            serde_json::json!({
                "timestamp":"2026-08-14T00:00:02Z",
                "type":"event_msg",
                "payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}
            })
        );
        let replacement = first.replace("100", "200");
        assert_eq!(first.len(), replacement.len());
        fs::write(&path, &first).unwrap();
        let source = register_source(&connection, &path, 10).unwrap();
        bind_source(
            &connection,
            &source,
            "account:verified-old",
            "test",
            "high",
            "test",
            None,
            10,
        )
        .unwrap();
        catch_up_path(&connection, &path, None, false, 11).unwrap();
        fs::write(&path, &replacement).unwrap();
        let replacement_source = register_source(&connection, &path, 12).unwrap();
        assert_ne!(replacement_source, source);
        catch_up_path(&connection, &path, None, false, 12).unwrap();
        let total: i64 = connection
            .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        let generation: i64 = connection
            .query_row(
                "SELECT generation FROM rollout_sources WHERE source_id = ?1",
                [&source],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total, 200);
        assert_eq!(generation, 0);
        assert_eq!(
            connection
                .query_row(
                    "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "quarantined"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM turn_usage WHERE account_key = 'account:verified-old'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let larger_replacement = replacement.replace("200", "9999");
        assert!(larger_replacement.len() > replacement.len());
        fs::write(&path, larger_replacement).unwrap();
        let larger_source = register_source(&connection, &path, 13).unwrap();
        assert_ne!(larger_source, replacement_source);
        catch_up_path(&connection, &path, None, false, 13).unwrap();
        let total_after_larger_replacement: i64 = connection
            .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        let generation_after_larger_replacement: i64 = connection
            .query_row(
                "SELECT generation FROM rollout_sources WHERE source_id = ?1",
                [&source],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total_after_larger_replacement, 9999);
        assert_eq!(generation_after_larger_replacement, 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn recovery_records_crash_and_sleep_gaps() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection.execute("INSERT INTO collector_sessions (session_id, collector_instance_id, pid, started_at, last_heartbeat_at, version) VALUES ('crash', 'i', 1, 100, 390, 'test'), ('sleep', 'j', 1, 100, 90, 'test')", []).unwrap();
        assert_eq!(recover_open_sessions(&connection, 400).unwrap(), 2);
        let reasons: Vec<String> = connection
            .prepare("SELECT reason FROM collector_gaps ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(reasons, vec!["crash", "os_sleep"]);
    }

    #[test]
    fn singleton_lock_is_exclusive_and_released() {
        let path = std::env::temp_dir().join(format!("codex-nexus-lock-{}", unique_suffix()));
        let first = CollectorLock::acquire(&path).unwrap();
        assert!(CollectorLock::acquire(&path).is_err());
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        assert!(CollectorSessionGuard::start(
            &connection,
            "/tmp/collector-lock-failure.db",
            &path,
            100,
        )
        .is_err());
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM collector_sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(first);
        let second = CollectorLock::acquire(&path).unwrap();
        drop(second);
        fs::write(&path, "pid=4294967295\n").unwrap();
        let stale_recovered = CollectorLock::acquire(&path).unwrap();
        drop(stale_recovered);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn durable_source_cursor_survives_close_and_reopen() {
        let database_path = std::env::temp_dir().join(format!(
            "codex-nexus-reopen-{}-{}.db",
            std::process::id(),
            unique_suffix()
        ));
        let path = temp_path("reopen");
        rollout_file(&path, 123);
        {
            let connection = Connection::open(&database_path).unwrap();
            initialize_schema(&connection).unwrap();
            catch_up_path(&connection, &path, None, false, 10).unwrap();
        }
        {
            let connection = Connection::open(&database_path).unwrap();
            initialize_schema(&connection).unwrap();
            assert!(!catch_up_path(&connection, &path, None, false, 20).unwrap());
            assert_eq!(
                connection
                    .query_row("SELECT COUNT(*) FROM turn_token_samples", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        let _ = fs::remove_file(database_path);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn partial_line_is_not_bound_until_completed_and_parsed() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = temp_path("partial");
        rollout_file(&path, 88);
        let content = fs::read_to_string(&path).unwrap();
        fs::write(&path, content.trim_end_matches('\n')).unwrap();
        assert!(!catch_up_path(&connection, &path, Some("account:a"), true, 10).unwrap());
        let source: String = connection
            .query_row("SELECT source_id FROM rollout_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            BINDING_UNRESOLVED
        );
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\n").unwrap();
        assert!(catch_up_path(&connection, &path, Some("account:a"), true, 11).unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            BINDING_VERIFIED
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_start_heartbeat_and_drop_flushes_a_graceful_stop() {
        let database_path = std::env::temp_dir().join(format!(
            "codex-nexus-session-{}-{}.db",
            std::process::id(),
            unique_suffix()
        ));
        let lock_path = database_path.with_extension("lock");
        let connection = Connection::open(&database_path).unwrap();
        initialize_schema(&connection).unwrap();
        let session_id;
        {
            let guard =
                CollectorSessionGuard::start(&connection, &database_path, &lock_path, 100).unwrap();
            session_id = guard.session_id.clone();
            touch_session(&connection, &guard.session_id, 101, &guard.instance_id).unwrap();
        }
        // The guard uses the same durable DB connection on drop; verify the
        // session is closed and no open session remains after a clean stop.
        let status: String = connection
            .query_row(
                "SELECT status FROM collector_sessions WHERE session_id = ?1",
                [&session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "stopped");
        let _ = fs::remove_file(database_path);
        let _ = fs::remove_file(lock_path);
    }
}
