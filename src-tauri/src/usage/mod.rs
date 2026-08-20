use serde::Serialize;
use tauri::{AppHandle, Emitter, Wry};

pub mod analytics;
pub mod category_usage;
pub mod collector_core;
pub mod collector_ipc;
pub mod collector_service;
pub mod db;
pub mod models;
pub mod quota;
pub mod rate_card;
pub mod recorder;
pub mod repository;
pub mod rollout;
pub mod scheduler;

pub use models::{AccountScope, CategoryUsage, UsageAnalytics, UsageAnalyticsQuery};
pub use scheduler::UsageSchedulerStatus;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDataInvalidatedPayload {
    pub reason: String,
    pub invalidated_at: i64,
}

pub fn emit_usage_data_invalidated(app: &AppHandle<Wry>, reason: &str) {
    let invalidated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let _ = app.emit(
        "codex://usage-data-invalidated",
        UsageDataInvalidatedPayload {
            reason: reason.into(),
            invalidated_at,
        },
    );
    collector_ipc::emit_event(
        app,
        collector_ipc::EVENT_USAGE_INVALIDATED,
        serde_json::json!({"reason": reason, "invalidatedAt": invalidated_at}),
    );
}

pub fn record_official_snapshot(
    app: &tauri::AppHandle<tauri::Wry>,
    snapshot: &serde_json::Value,
) -> Result<(), String> {
    recorder::record_official_snapshot(app, snapshot)
}

pub fn record_rate_limit_update(
    app: &tauri::AppHandle<tauri::Wry>,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let result = recorder::record_rate_limit_update(app, payload);
    if result.is_ok() {
        emit_usage_data_invalidated(app, "rate_limits");
    }
    result
}

pub fn analytics(
    app: &tauri::AppHandle<tauri::Wry>,
    range: &str,
    breakdown: &str,
) -> Result<UsageAnalytics, String> {
    analytics::app_legacy_query(app, range, breakdown)
}
