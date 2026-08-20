//! Compatibility DTO for the former in-process scheduler.
//!
//! Collector scheduling, Codex app-server RPC, file watching, heartbeat and
//! durable writes now live in `collector_service` and are only started by the
//! `nexus-collector` binary. This module intentionally contains no Tauri
//! scheduler implementation; the status shape remains here so existing
//! command/UI payloads keep their names during the migration.

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSchedulerStatus {
    pub policy: String,
    pub mode: String,
    pub watcher_active: bool,
    pub pending_reconciliation: bool,
    pub fallback_seconds: u64,
    pub last_refresh_at: Option<i64>,
    pub last_local_activity_at: Option<i64>,
    pub refreshing: bool,
    pub refresh_reason: Option<String>,
    pub refresh_error: Option<String>,
    pub refresh_started_at: Option<i64>,
    pub refresh_generation: Option<u64>,
    pub queued_refresh: bool,
}
