//! Tauri-free Collector service runtime.
//!
//! This is the process boundary used by `nexus-collector`. It owns the
//! durable SQLite writer, singleton lock, heartbeat, JSONL catch-up and file
//! watcher. The desktop app only connects to its endpoint.

use notify::{RecursiveMode, Watcher};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex, Weak,
    },
    thread,
    time::Duration,
};

use super::{
    category_usage,
    collector_core::{self, CollectorLock, CollectorSessionGuard},
    collector_ipc, db, quota, recorder, rollout,
};
use crate::codex::CodexRpcClient;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

fn read_account_snapshot(rpc: &Arc<CodexRpcClient>) -> Result<Value, String> {
    rpc.request("account/read", Some(json!({"refreshToken": false})))
        .map(|account| {
            json!({
                "codexPath": rpc.display_path().unwrap_or_default(),
                "fetchedAt": collector_core::now_seconds() * 1000,
                "account": account,
            })
        })
}

struct Runtime {
    database_path: PathBuf,
    endpoint: PathBuf,
    connection: Mutex<rusqlite::Connection>,
    rpc: Arc<CodexRpcClient>,
    _session: CollectorSessionGuard,
    refresh: Arc<Mutex<RefreshState>>,
}

#[derive(Default)]
struct RefreshState {
    generation: u64,
    refreshing: bool,
    queued_refresh: bool,
    error: Option<String>,
    started_at: Option<i64>,
    latest_snapshot: Option<Value>,
}

impl RefreshState {
    fn begin_refresh(&mut self, now: i64) -> Option<u64> {
        if self.refreshing {
            return None;
        }
        self.generation = self.generation.saturating_add(1);
        self.refreshing = true;
        self.queued_refresh = false;
        self.error = None;
        self.started_at = Some(now);
        self.latest_snapshot = None;
        Some(self.generation)
    }

    fn invalidate_account(&mut self, now: i64) -> Option<u64> {
        self.generation = self.generation.saturating_add(1);
        self.latest_snapshot = None;
        self.error = None;
        self.started_at = Some(now);
        if self.refreshing {
            self.queued_refresh = true;
            None
        } else {
            self.refreshing = true;
            self.queued_refresh = false;
            Some(self.generation)
        }
    }

    fn renderable_snapshot(&self) -> Value {
        if self.refreshing {
            Value::Null
        } else {
            self.latest_snapshot.clone().unwrap_or(Value::Null)
        }
    }
}

pub struct StandaloneCollector {
    runtime: Arc<Runtime>,
}

impl StandaloneCollector {
    pub fn start(database_path: PathBuf) -> Result<Self, String> {
        let lock_path = collector_core::lock_path_for_database(&database_path);
        let lock = CollectorLock::acquire(&lock_path)?;
        let connection = db::open_standalone_database(&database_path)?;
        let session = CollectorSessionGuard::start_with_lock(
            &connection,
            database_path.clone(),
            lock,
            collector_core::now_seconds(),
        )?;
        let endpoint = database_path.with_file_name(collector_ipc::IPC_SOCKET_FILE);
        let refresh = Arc::new(Mutex::new(RefreshState::default()));
        let (account_refresh_tx, account_refresh_rx) = mpsc::channel::<()>();
        let callback_refresh = Arc::clone(&refresh);
        let rpc = CodexRpcClient::start_headless_with_account_updates(move || {
            let should_start = callback_refresh
                .lock()
                .ok()
                .and_then(|mut state| state.invalidate_account(collector_core::now_seconds()))
                .is_some();
            if should_start {
                let _ = account_refresh_tx.send(());
            }
        });
        let runtime = Arc::new(Runtime {
            database_path,
            endpoint,
            connection: Mutex::new(connection),
            rpc,
            _session: session,
            refresh,
        });
        Self::spawn_account_refresh_dispatcher(&runtime, account_refresh_rx)?;
        let service = Self { runtime };
        Ok(service)
    }

    fn spawn_account_refresh_dispatcher(
        runtime: &Arc<Runtime>,
        receiver: mpsc::Receiver<()>,
    ) -> Result<(), String> {
        let runtime = Arc::downgrade(runtime);
        thread::Builder::new()
            .name("nexus-collector-account-events".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    let Some(runtime) = Weak::upgrade(&runtime) else {
                        break;
                    };
                    if let Err(error) = StandaloneCollector::spawn_reserved_refresh(runtime.clone())
                    {
                        StandaloneCollector::fail_reserved_refresh(&runtime, error);
                    }
                }
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn endpoint(&self) -> &Path {
        &self.runtime.endpoint
    }

    fn refresh_sources(&self, refresh_generation: u64) -> Result<u64, String> {
        // The RPC child belongs to this OS-managed process, not to the UI.
        // A disconnected Codex server must not prevent JSONL catch-up and
        // heartbeat, so the local durable plane still refreshes below.
        // Resume durable registry entries before broad filesystem discovery;
        // this is important for a known source's live tail and avoids a large
        // offline history scan delaying its binding evidence.
        let connection = self
            .runtime
            .connection
            .lock()
            .map_err(|_| "collector DB lock poisoned".to_string())?;
        let mut paths = collector_core::registered_source_paths(&connection)?;
        for path in rollout::discover_rollout_files_for_collector() {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        drop(connection);

        let mut identity_unavailable = false;
        for path in paths.into_iter().filter(|path| path.is_file()) {
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            // Re-read account ownership for every source. A single refresh
            // may span an A -> B transition; reusing the previous source's
            // identity would incorrectly bind B's tail to A.
            let account_snapshot = read_account_snapshot(&self.runtime.rpc);
            let active_account = account_snapshot
                .as_ref()
                .ok()
                .and_then(recorder::explicit_snapshot_account_key);
            let evidence_at = collector_core::now_seconds();
            let allow_retroactive_binding = active_account.is_some();
            let connection = self
                .runtime
                .connection
                .lock()
                .map_err(|_| "collector DB lock poisoned".to_string())?;
            match account_snapshot.as_ref() {
                Ok(snapshot) => {
                    // Persist and verify the ownership interval before
                    // consuming this source's new bytes. A successful
                    // signed-out read is immediately fenced as unresolved.
                    if let Some(account_key) = active_account.as_ref() {
                        recorder::record_account_snapshot(
                            &connection,
                            snapshot,
                            account_key,
                            evidence_at,
                        )?;
                    } else {
                        identity_unavailable = true;
                        recorder::record_official_snapshot_connection(&connection, snapshot)?;
                    }
                }
                Err(_) => {
                    // account/read is the identity authority. Fence the old
                    // account even when rate-limit/usage RPCs might succeed.
                    identity_unavailable = true;
                    recorder::record_account_identity_unavailable_connection(
                        &connection,
                        self.runtime.rpc.display_path().as_deref(),
                        evidence_at,
                    )?;
                }
            }
            let ownership_verified = active_account.as_deref().is_some_and(|account| {
                recorder::account_presence_covers(&connection, account, evidence_at)
                    .unwrap_or(false)
            });
            let allow_retroactive_binding = allow_retroactive_binding && ownership_verified;
            let source_id =
                collector_core::register_source(&connection, &path, collector_core::now_seconds())?;
            let before_offset: i64 = connection
                .query_row(
                    "SELECT last_offset FROM rollout_sources WHERE source_id = ?1",
                    [&source_id],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let current_size = std::fs::metadata(&path)
                .map(|metadata| metadata.len() as i64)
                .unwrap_or_default();
            let changed = collector_core::catch_up_path(
                &connection,
                &path,
                active_account.as_deref(),
                allow_retroactive_binding,
                collector_core::now_seconds(),
            )?;
            // Keep the service-side evidence explicit as a defense in depth:
            // only a non-zero consumed cursor plus an actual growing tail can
            // trigger retroactive binding. First discovery remains unresolved.
            let after_offset: i64 = connection
                .query_row(
                    "SELECT last_offset FROM rollout_sources WHERE source_id = ?1",
                    [&source_id],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if (changed || after_offset > before_offset)
                && before_offset > 0
                && current_size > before_offset
                && allow_retroactive_binding
            {
                let status: String = connection
                    .query_row(
                        "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                        [&source_id],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if status == collector_core::BINDING_UNRESOLVED {
                    if let Some(account) = active_account.as_deref() {
                        collector_core::retroactively_bind_source(
                            &connection,
                            &source_id,
                            account,
                            "standalone collector live-tail evidence",
                            collector_core::now_seconds(),
                        )?;
                    }
                }
            }
            drop(connection);
        }
        // The full snapshot is collected after local catch-up so a slow or
        // offline official endpoint cannot prevent local ingestion. Its
        // identity is independently validated by recorder and can never
        // fall back to the previous current account.
        let official_snapshot =
            crate::fetch_codex_snapshot(&self.runtime.rpc)
                .ok()
                .map(|mut snapshot| {
                    if let Some(object) = snapshot.as_object_mut() {
                        object.insert("refreshGeneration".into(), json!(refresh_generation));
                    }
                    snapshot
                });
        let connection = self
            .runtime
            .connection
            .lock()
            .map_err(|_| "collector DB lock poisoned".to_string())?;
        let complete_snapshot = official_snapshot
            .as_ref()
            .is_some_and(crate::snapshot_is_complete_signed_in);
        if let Some(snapshot) = official_snapshot.as_ref() {
            if snapshot
                .get("accountError")
                .and_then(Value::as_str)
                .is_some()
            {
                identity_unavailable = true;
            } else if recorder::explicit_snapshot_account_key(snapshot).is_none() {
                identity_unavailable = true;
            }
            recorder::record_official_snapshot_connection(&connection, snapshot)?;
        }
        // With no fresh account identity, leave old account-derived quota
        // intervals untouched. Durable raw samples and unresolved fences are
        // still recorded, but estimator/rebuild output cannot advance under
        // an unknown owner.
        if !identity_unavailable && complete_snapshot {
            quota::rebuild_all_intervals(&connection)?;
        }
        if complete_snapshot {
            let mut accounts = connection
                .prepare(
                    "SELECT account_key FROM accounts WHERE account_key NOT LIKE 'unresolved:%'",
                )
                .map_err(|error| error.to_string())?
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            accounts.sort();
            accounts.dedup();
            for account in accounts {
                let _ = rollout::refresh_account_data_health(&connection, &account);
            }
        }
        if let Some(snapshot) = official_snapshot.as_ref() {
            let failed_part = ["accountError", "rateLimitsError", "usageError"]
                .into_iter()
                .find_map(|field| snapshot.get(field).and_then(Value::as_str));
            if let Some(error) = failed_part {
                return Err(format!("Codex snapshot refresh failed: {error}"));
            }
        } else {
            return Err("Codex snapshot refresh failed or timed out".into());
        }
        let mut state = self
            .runtime
            .refresh
            .lock()
            .map_err(|_| "collector refresh lock poisoned".to_string())?;
        // Account notifications may invalidate this worker while its RPCs are
        // in flight. Never publish a snapshot produced by a stale generation.
        if state.generation == refresh_generation && state.refreshing {
            state.latest_snapshot = complete_snapshot.then(|| official_snapshot.unwrap());
        }
        Ok(refresh_generation)
    }

    fn schedule_refresh(&self) -> Result<u64, String> {
        let generation = {
            let mut state = self
                .runtime
                .refresh
                .lock()
                .map_err(|_| "collector refresh lock poisoned".to_string())?;
            match state.begin_refresh(collector_core::now_seconds()) {
                Some(generation) => generation,
                None => return Ok(state.generation),
            }
        };
        if let Err(error) = Self::spawn_reserved_refresh(Arc::clone(&self.runtime)) {
            Self::fail_reserved_refresh(&self.runtime, error.clone());
            return Err(error);
        }
        Ok(generation)
    }

    fn spawn_reserved_refresh(runtime: Arc<Runtime>) -> Result<(), String> {
        let generation = {
            let mut state = runtime
                .refresh
                .lock()
                .map_err(|_| "collector refresh lock poisoned".to_string())?;
            if !state.refreshing {
                return Ok(());
            }
            // An account event can arrive after a refresh was reserved but
            // before its thread starts. Launch only the newest generation.
            state.queued_refresh = false;
            state.generation
        };
        thread::Builder::new()
            .name("nexus-collector-refresh".into())
            .spawn(move || {
                let service = StandaloneCollector {
                    runtime: Arc::clone(&runtime),
                };
                let result = service.refresh_sources(generation);
                let should_refresh_again = match runtime.refresh.lock() {
                    Ok(mut state) => {
                        if state.generation == generation {
                            if let Err(error) = result {
                                state.latest_snapshot = None;
                                state.error = Some(error);
                            }
                        }
                        if state.queued_refresh || state.generation != generation {
                            state.queued_refresh = false;
                            true
                        } else {
                            state.refreshing = false;
                            false
                        }
                    }
                    Err(_) => false,
                };
                if should_refresh_again {
                    if let Err(error) = Self::spawn_reserved_refresh(Arc::clone(&runtime)) {
                        Self::fail_reserved_refresh(&runtime, error);
                    }
                }
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn fail_reserved_refresh(runtime: &Runtime, error: String) {
        if let Ok(mut state) = runtime.refresh.lock() {
            state.latest_snapshot = None;
            state.refreshing = false;
            state.queued_refresh = false;
            state.error = Some(error);
        }
    }

    fn run_once_refresh(&self) -> Result<(), String> {
        let mut generation = {
            let mut state = self
                .runtime
                .refresh
                .lock()
                .map_err(|_| "collector refresh lock poisoned".to_string())?;
            state
                .begin_refresh(collector_core::now_seconds())
                .unwrap_or(state.generation)
        };
        loop {
            let result = self.refresh_sources(generation);
            let mut state = self
                .runtime
                .refresh
                .lock()
                .map_err(|_| "collector refresh lock poisoned".to_string())?;
            if state.generation == generation && !state.queued_refresh {
                state.refreshing = false;
                if let Err(error) = result {
                    state.latest_snapshot = None;
                    state.error = Some(error.clone());
                    return Err(error);
                }
                return Ok(());
            }
            // Coalesce account notifications received during a one-shot run
            // and validate the newest identity before the process exits.
            state.queued_refresh = false;
            generation = state.generation;
            drop(state);
        }
    }

    fn heartbeat(&self) -> Result<(), String> {
        let connection = self
            .runtime
            .connection
            .lock()
            .map_err(|_| "collector DB lock poisoned".to_string())?;
        collector_core::touch_session(
            &connection,
            &self.runtime._session.session_id,
            collector_core::now_seconds(),
            &self.runtime._session.instance_id,
        )
    }

    fn handle(&self, request: collector_ipc::IpcRequest) -> collector_ipc::IpcResponse {
        let result = (|| -> Result<Value, String> {
            let method = collector_ipc::CollectorMethod::parse(&request.method)?;
            match method {
                collector_ipc::CollectorMethod::GetStatus => {
                    let connection = db::open_standalone_readonly(&self.runtime.database_path)?;
                    let collector = collector_ipc::status_from_connection(
                        Some(&connection),
                        &self.runtime.endpoint.to_string_lossy(),
                    );
                    let (
                        refreshing,
                        refresh_error,
                        refresh_started_at,
                        refresh_generation,
                        queued_refresh,
                    ) = {
                        let state = self
                            .runtime
                            .refresh
                            .lock()
                            .map_err(|_| "collector refresh lock poisoned".to_string())?;
                        (
                            state.refreshing,
                            state.error.clone(),
                            state.started_at,
                            state.generation,
                            state.queued_refresh,
                        )
                    };
                    Ok(json!({
                        "collector": collector,
                        "scheduler": {
                            "policy": "standalone",
                            "mode": "standalone",
                            "watcherActive": true,
                            "pendingReconciliation": false,
                            "fallbackSeconds": 5,
                            "lastRefreshAt": null,
                            "lastLocalActivityAt": null,
                            "refreshing": refreshing,
                            "refreshReason": null,
                            "refreshError": refresh_error,
                            "refreshStartedAt": refresh_started_at,
                            "refreshGeneration": refresh_generation,
                            "queuedRefresh": queued_refresh
                        },
                        "codex": self.runtime.rpc.status()
                    }))
                }
                collector_ipc::CollectorMethod::RefreshNow => Ok(json!(self.schedule_refresh()?)),
                collector_ipc::CollectorMethod::GetSnapshot => self
                    .runtime
                    .refresh
                    .lock()
                    .map_err(|_| "collector refresh lock poisoned".to_string())
                    .map(|state| state.renderable_snapshot()),
                collector_ipc::CollectorMethod::GetCodexStatus => {
                    serde_json::to_value(self.runtime.rpc.status())
                        .map_err(|error| error.to_string())
                }
                collector_ipc::CollectorMethod::ReconnectCodex => {
                    self.runtime.rpc.reconnect()?;
                    Ok(Value::Bool(true))
                }
                collector_ipc::CollectorMethod::GetAccount => {
                    let connection = db::open_standalone_readonly(&self.runtime.database_path)?;
                    let account = recorder::current_account_key(&connection)?;
                    if let Some(account_key) = account {
                        let row: Option<(Option<String>, Option<String>, Option<String>)> = connection.query_row("SELECT email, plan_type, display_name FROM accounts WHERE account_key = ?1", [&account_key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional().map_err(|error| error.to_string())?;
                        Ok(
                            json!({"account": {"accountKey": account_key, "email": row.as_ref().and_then(|value| value.0.clone()), "planType": row.as_ref().and_then(|value| value.1.clone()), "displayName": row.and_then(|value| value.2)}}),
                        )
                    } else {
                        Ok(Value::Null)
                    }
                }
                collector_ipc::CollectorMethod::GetCategoryUsage => {
                    let period = request
                        .params
                        .get("period")
                        .and_then(Value::as_str)
                        .unwrap_or("day");
                    let connection = db::open_standalone_readonly(&self.runtime.database_path)?;
                    serde_json::to_value(category_usage::category_usage(&connection, period)?)
                        .map_err(|error| error.to_string())
                }
                collector_ipc::CollectorMethod::GetDataHealth => {
                    let connection = db::open_standalone_readonly(&self.runtime.database_path)?;
                    serde_json::to_value(collector_ipc::data_health_for_connection(
                        &connection,
                        &self.runtime.endpoint.to_string_lossy(),
                    )?)
                    .map_err(|error| error.to_string())
                }
                collector_ipc::CollectorMethod::RebuildAccount => {
                    let account_key = request
                        .params
                        .get("accountKey")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "REBUILD_ACCOUNT requires accountKey".to_string())?;
                    let connection = self
                        .runtime
                        .connection
                        .lock()
                        .map_err(|_| "collector DB lock poisoned".to_string())?;
                    rollout::rebuild_account_connection(&connection, account_key)?;
                    Ok(json!(true))
                }
            }
        })();
        match result {
            Ok(result) => collector_ipc::IpcResponse {
                id: request.id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => collector_ipc::IpcResponse {
                id: request.id,
                ok: false,
                result: None,
                error: Some(error),
            },
        }
    }

    #[cfg(unix)]
    fn serve_ipc(&self) -> Result<(), String> {
        let listener = collector_ipc::bind_secure_listener(&self.runtime.endpoint)?;
        let service = Arc::clone(&self.runtime);
        let active = Arc::new(AtomicUsize::new(0));
        thread::Builder::new()
            .name("nexus-collector-ipc".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    if active.fetch_add(1, Ordering::AcqRel) >= collector_ipc::MAX_CONNECTIONS {
                        active.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    let active = Arc::clone(&active);
                    let service = Arc::clone(&service);
                    let _ = thread::Builder::new()
                        .name("nexus-collector-ipc-request".into())
                        .spawn(move || {
                            let _guard = ActiveConnection(active);
                            let _ = stream.set_read_timeout(Some(collector_ipc::IO_TIMEOUT));
                            let _ = stream.set_write_timeout(Some(collector_ipc::IO_TIMEOUT));
                            let response = match collector_ipc::read_limited_line(&mut stream)
                                .and_then(|line| {
                                    serde_json::from_slice::<collector_ipc::IpcRequest>(&line)
                                        .map_err(|error| error.to_string())
                                }) {
                                Ok(request) => {
                                    let runtime = StandaloneCollector { runtime: service };
                                    runtime.handle(request)
                                }
                                Err(error) => collector_ipc::IpcResponse {
                                    id: "unknown".into(),
                                    ok: false,
                                    result: None,
                                    error: Some(error),
                                },
                            };
                            let _ = collector_ipc::write_response(&mut stream, &response);
                        });
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn serve_ipc(&self) -> Result<(), String> {
        Err("nexus-collector Named Pipe transport is not implemented on Windows yet".into())
    }

    pub fn run(self, once: bool) -> Result<(), String> {
        if once {
            return self.run_once_refresh();
        }
        self.serve_ipc()?;
        // Publish the endpoint before the first potentially slow app-server
        // refresh. A UI must be able to observe Running/Reconnecting while
        // Codex itself is offline or still initializing.
        let initial_refresh = Arc::clone(&self.runtime);
        thread::Builder::new()
            .name("nexus-collector-initial-refresh".into())
            .spawn(move || {
                let service = StandaloneCollector {
                    runtime: initial_refresh,
                };
                let _ = service.schedule_refresh();
            })
            .map_err(|error| error.to_string())?;
        let (events_tx, events_rx) = mpsc::channel::<PathBuf>();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else { return };
                for path in event.paths {
                    if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                        let _ = events_tx.send(path);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        for root in rollout::rollout_watch_roots() {
            watcher
                .watch(&root, RecursiveMode::Recursive)
                .map_err(|error| error.to_string())?;
        }
        let mut next_refresh = std::time::Instant::now() + REFRESH_INTERVAL;
        loop {
            let wait = next_refresh
                .saturating_duration_since(std::time::Instant::now())
                .min(HEARTBEAT_INTERVAL);
            if let Ok(path) = events_rx.recv_timeout(wait) {
                if path.is_file() {
                    let _ = self.schedule_refresh();
                }
            }
            let _ = self.heartbeat();
            if next_refresh <= std::time::Instant::now() {
                let _ = self.schedule_refresh();
                next_refresh = std::time::Instant::now() + REFRESH_INTERVAL;
            }
        }
    }
}

struct ActiveConnection(Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn default_database_path() -> PathBuf {
    if let Some(path) = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0] == "--database")
        .map(|pair| PathBuf::from(&pair[1]))
    {
        return path;
    }
    db::default_database_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_read_waiting_on_state_lock_cannot_escape_invalidation() {
        let refresh = Arc::new(Mutex::new(RefreshState {
            generation: 7,
            latest_snapshot: Some(json!({"refreshGeneration": 7, "quota": "old"})),
            ..RefreshState::default()
        }));
        let mut writer = refresh.lock().unwrap();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let reader_refresh = Arc::clone(&refresh);
        let reader = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            reader_refresh.lock().unwrap().renderable_snapshot()
        });

        // The reader is now waiting on the same lock that atomically owns the
        // refresh flag, generation and snapshot. Invalidate before releasing
        // it to reproduce the old GET_SNAPSHOT interleaving deterministically.
        attempted_rx.recv().unwrap();
        assert_eq!(writer.invalidate_account(42), Some(8));
        assert!(writer.refreshing);
        assert_eq!(writer.generation, 8);
        assert!(writer.latest_snapshot.is_none());
        drop(writer);

        assert!(reader.join().unwrap().is_null());
    }

    #[test]
    fn account_invalidation_queues_new_generation_during_active_refresh() {
        let mut state = RefreshState::default();
        assert_eq!(state.begin_refresh(10), Some(1));
        state.latest_snapshot = Some(json!({"refreshGeneration": 1}));

        assert_eq!(state.invalidate_account(11), None);
        assert_eq!(state.generation, 2);
        assert!(state.refreshing);
        assert!(state.queued_refresh);
        assert!(state.renderable_snapshot().is_null());
    }
}
