pub mod analytics;
pub mod db;
pub mod models;
pub mod quota;
pub mod rate_card;
pub mod recorder;
pub mod repository;
pub mod rollout;
pub mod scheduler;

pub use models::{AccountScope, DailyModelUsage, UsageAnalytics, UsageAnalyticsQuery};
pub use scheduler::{UsageRefreshScheduler, UsageSchedulerState, UsageSchedulerStatus};

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
    recorder::record_rate_limit_update(app, payload)
}

pub fn analytics(
    app: &tauri::AppHandle<tauri::Wry>,
    range: &str,
    breakdown: &str,
) -> Result<UsageAnalytics, String> {
    analytics::app_legacy_query(app, range, breakdown)
}
