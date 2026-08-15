use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_notification::NotificationExt;

const HISTORY_FILE: &str = "usage-history.json";
const SETTINGS_FILE: &str = "settings.json";
const SAMPLE_INTERVAL_SECONDS: i64 = 5 * 60;
const MAX_HISTORY_ENTRIES: usize = 120_000;

static MONITOR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorSettings {
    pub notify_thresholds: Vec<u8>,
    pub notify_quota_reset: bool,
    pub launch_at_startup: bool,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    #[serde(default = "default_usage_refresh_policy")]
    pub usage_refresh_policy: String,

    // These maps are intentionally persisted with settings. They make the
    // notification behavior survive app restarts and are keyed by
    // `${limitId}:${primary|secondary}`.
    #[serde(default)]
    pub last_notified_threshold: BTreeMap<String, u8>,
    #[serde(default)]
    pub last_seen_reset_at: BTreeMap<String, i64>,
    #[serde(default)]
    pub last_reset_notified_at: BTreeMap<String, i64>,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            notify_thresholds: vec![80, 90, 95, 100],
            notify_quota_reset: true,
            launch_at_startup: false,
            start_minimized: false,
            close_to_tray: true,
            usage_refresh_policy: default_usage_refresh_policy(),
            last_notified_threshold: BTreeMap::new(),
            last_seen_reset_at: BTreeMap::new(),
            last_reset_notified_at: BTreeMap::new(),
        }
    }
}

fn default_usage_refresh_policy() -> String {
    "adaptive".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryLimit {
    pub limit_id: String,
    pub limit_name: String,
    pub window: String,
    pub window_duration_mins: u64,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistoryEntry {
    pub timestamp: i64,
    pub limits: BTreeMap<String, HistoryLimit>,
    pub lifetime_tokens: Option<i64>,
}

#[derive(Clone, Debug)]
struct QuotaWindow {
    key: String,
    limit_id: String,
    limit_name: String,
    window: String,
    window_duration_mins: u64,
    used_percent: f64,
    resets_at: Option<i64>,
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn storage_dir(app: &AppHandle<Wry>) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("Codex Usage Monitor");

    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir)
}

fn read_json<T>(path: PathBuf, default: T) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(default);
    }

    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    if text.trim().is_empty() {
        return Ok(default);
    }

    serde_json::from_str(&text).map_err(|error| error.to_string())
}

fn write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    fs::write(&temp, text).map_err(|error| error.to_string())?;
    fs::rename(&temp, path).map_err(|error| error.to_string())
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    MONITOR_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn load_settings(app: &AppHandle<Wry>) -> Result<MonitorSettings, String> {
    let _guard = lock();
    let dir = storage_dir(app)?;
    let mut settings = read_json(dir.join(SETTINGS_FILE), MonitorSettings::default())?;
    sanitize_settings(&mut settings);
    Ok(settings)
}

pub fn save_settings(
    app: &AppHandle<Wry>,
    mut settings: MonitorSettings,
) -> Result<MonitorSettings, String> {
    let _guard = lock();
    sanitize_settings(&mut settings);
    let dir = storage_dir(app)?;
    write_json(dir.join(SETTINGS_FILE), &settings)?;
    Ok(settings)
}

pub fn sanitize_settings(settings: &mut MonitorSettings) {
    settings
        .notify_thresholds
        .retain(|value| *value > 0 && *value <= 100);
    settings.notify_thresholds.sort_unstable();
    settings.notify_thresholds.dedup();

    if !matches!(
        settings.usage_refresh_policy.as_str(),
        "adaptive" | "15s" | "30s" | "1m" | "3m" | "5m" | "5s"
    ) {
        settings.usage_refresh_policy = default_usage_refresh_policy();
    }
}

pub fn load_history(app: &AppHandle<Wry>) -> Result<Vec<UsageHistoryEntry>, String> {
    let _guard = lock();
    let dir = storage_dir(app)?;
    read_json(dir.join(HISTORY_FILE), Vec::new())
}

fn number_as_i64(value: Option<&Value>) -> Option<i64> {
    value
        .and_then(Value::as_i64)
        .or_else(|| value.and_then(Value::as_u64).map(|value| value as i64))
        .or_else(|| value.and_then(Value::as_f64).map(|value| value as i64))
}

fn number_as_f64(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .or_else(|| value.and_then(Value::as_i64).map(|value| value as f64))
        .or_else(|| value.and_then(Value::as_u64).map(|value| value as f64))
}

fn extract_windows(snapshot: &Value) -> Vec<QuotaWindow> {
    let Some(rate_limits) = snapshot.get("rateLimits") else {
        return Vec::new();
    };

    let mut buckets: Vec<(String, &Value)> = Vec::new();
    if let Some(by_id) = rate_limits
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        for (limit_id, bucket) in by_id {
            buckets.push((limit_id.clone(), bucket));
        }
    }

    if buckets.is_empty() {
        if let Some(bucket) = rate_limits.get("rateLimits") {
            if let Some(limit_id) = bucket.get("limitId").and_then(Value::as_str) {
                buckets.push((limit_id.to_owned(), bucket));
            }
        }
    }

    buckets
        .into_iter()
        .flat_map(|(limit_id, bucket)| {
            ["primary", "secondary"]
                .into_iter()
                .filter_map(move |window| {
                    let data = bucket.get(window)?;
                    let duration = data.get("windowDurationMins").and_then(Value::as_u64)?;
                    let used_percent = number_as_f64(data.get("usedPercent"))?.clamp(0.0, 100.0);
                    let limit_name = bucket
                        .get("limitName")
                        .and_then(Value::as_str)
                        .unwrap_or(&limit_id)
                        .to_owned();
                    let key = format!("{}:{}", limit_id, window);

                    Some(QuotaWindow {
                        key,
                        limit_id: limit_id.clone(),
                        limit_name,
                        window: window.to_owned(),
                        window_duration_mins: duration,
                        used_percent,
                        resets_at: number_as_i64(data.get("resetsAt")),
                    })
                })
        })
        .collect()
}

fn window_label(window: &QuotaWindow) -> String {
    match window.window_duration_mins {
        300 => "5 Hour".into(),
        1440 => "Daily".into(),
        10080 => "Weekly".into(),
        value if value % 10080 == 0 => format!("{} Week", value / 10080),
        value if value % 1440 == 0 => format!("{} Day", value / 1440),
        value if value % 60 == 0 => format!("{} Hour", value / 60),
        value => format!("{} Minute", value),
    }
}

fn reset_relative(resets_at: Option<i64>) -> String {
    let Some(resets_at) = resets_at else {
        return "Reset time unavailable.".into();
    };

    let total_minutes = (resets_at - now_seconds()).max(0) / 60;
    let days = total_minutes / 1440;
    let hours = (total_minutes % 1440) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        return format!("Resets in {}d {}h.", days, hours);
    }
    if hours > 0 {
        return format!("Resets in {}h {}m.", hours, minutes);
    }
    format!("Resets in {}m.", minutes)
}

fn next_threshold(last_notified: u8, used_percent: f64, thresholds: &[u8]) -> Option<u8> {
    thresholds
        .iter()
        .copied()
        .filter(|threshold| *threshold > last_notified && used_percent >= *threshold as f64)
        .max()
}

fn quota_reset_occurred(previous: Option<i64>, current: Option<i64>, now: i64) -> bool {
    matches!((previous, current), (Some(previous), Some(current)) if current > previous && current > now)
}

fn send_notification(app: &AppHandle<Wry>, title: String, body: String) {
    if let Err(error) = app.notification().builder().title(title).body(body).show() {
        eprintln!("[Monitor] notification failed: {}", error);
    }
}

pub fn process_snapshot(app: &AppHandle<Wry>, snapshot: &Value) {
    let _guard = lock();
    let mut settings = match load_settings_unlocked(app) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("[Monitor] settings read failed: {}", error);
            return;
        }
    };

    let windows = extract_windows(snapshot);
    let mut changed = false;
    let mut should_force_history = false;
    let mut notifications = Vec::<(String, String)>::new();
    let now = now_seconds();

    for window in &windows {
        let key = &window.key;
        let previous_reset = settings.last_seen_reset_at.get(key).copied();
        let reset_occurred = quota_reset_occurred(previous_reset, window.resets_at, now);

        if reset_occurred {
            settings.last_notified_threshold.remove(key);
            settings
                .last_reset_notified_at
                .insert(key.clone(), window.resets_at.unwrap());
            changed = true;
            should_force_history = true;

            if settings.notify_quota_reset {
                notifications.push((
                    "Codex quota reset".into(),
                    format!("Your {} quota is available again.", window_label(window)),
                ));
            }
        }

        if let Some(resets_at) = window.resets_at {
            if settings.last_seen_reset_at.insert(key.clone(), resets_at) != Some(resets_at) {
                changed = true;
            }
        }

        if let Some(threshold) = next_threshold(
            settings
                .last_notified_threshold
                .get(key)
                .copied()
                .unwrap_or(0),
            window.used_percent,
            &settings.notify_thresholds,
        ) {
            settings
                .last_notified_threshold
                .insert(key.clone(), threshold);
            changed = true;
            should_force_history = true;

            notifications.push((
                format!("Codex usage reached {}%", threshold),
                format!(
                    "{} quota has {}% remaining.\n{}",
                    window_label(window),
                    (100.0 - window.used_percent).round() as i64,
                    reset_relative(window.resets_at),
                ),
            ));
        }
    }

    for (title, body) in notifications {
        send_notification(app, title, body);
    }

    if let Err(error) = record_history_unlocked(app, snapshot, should_force_history) {
        eprintln!("[Monitor] history write failed: {}", error);
    }

    if changed {
        if let Err(error) = save_settings_unlocked(app, &settings) {
            eprintln!("[Monitor] settings write failed: {}", error);
        }
    }
}

fn load_settings_unlocked(app: &AppHandle<Wry>) -> Result<MonitorSettings, String> {
    let dir = storage_dir(app)?;
    let mut settings = read_json(dir.join(SETTINGS_FILE), MonitorSettings::default())?;
    sanitize_settings(&mut settings);
    Ok(settings)
}

fn save_settings_unlocked(app: &AppHandle<Wry>, settings: &MonitorSettings) -> Result<(), String> {
    let dir = storage_dir(app)?;
    write_json(dir.join(SETTINGS_FILE), settings)
}

fn record_history_unlocked(
    app: &AppHandle<Wry>,
    snapshot: &Value,
    force: bool,
) -> Result<(), String> {
    let dir = storage_dir(app)?;
    let path = dir.join(HISTORY_FILE);
    let mut history: Vec<UsageHistoryEntry> = read_json(path.clone(), Vec::new())?;
    let now = now_seconds();

    let should_record = force
        || history
            .last()
            .map(|entry| now - entry.timestamp >= SAMPLE_INTERVAL_SECONDS)
            .unwrap_or(true);

    if !should_record {
        return Ok(());
    }

    let limits = extract_windows(snapshot)
        .into_iter()
        .map(|window| {
            let key = window.key.clone();
            (
                key,
                HistoryLimit {
                    limit_id: window.limit_id,
                    limit_name: window.limit_name,
                    window: window.window,
                    window_duration_mins: window.window_duration_mins,
                    used_percent: window.used_percent,
                    resets_at: window.resets_at,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let lifetime_tokens = snapshot
        .get("usage")
        .and_then(|usage| usage.get("summary"))
        .and_then(|summary| number_as_i64(summary.get("lifetimeTokens")));

    let entry = UsageHistoryEntry {
        timestamp: now,
        limits,
        lifetime_tokens,
    };

    if history.last().map(|item| item.timestamp) == Some(now) {
        history.pop();
    }
    history.push(entry);
    if history.len() > MAX_HISTORY_ENTRIES {
        let trim = history.len() - MAX_HISTORY_ENTRIES;
        history.drain(0..trim);
    }

    write_json(path, &history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_sorted_and_deduplicated() {
        let mut settings = MonitorSettings {
            notify_thresholds: vec![100, 95, 95, 0, 101, 80],
            ..MonitorSettings::default()
        };
        sanitize_settings(&mut settings);
        assert_eq!(settings.notify_thresholds, vec![80, 95, 100]);
    }

    #[test]
    fn history_limit_keys_are_stable_for_multiple_buckets() {
        let mut keys = std::collections::BTreeSet::new();
        keys.insert("codex:primary");
        keys.insert("codex_other:secondary");
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn threshold_notifications_are_deduplicated_and_advance() {
        let thresholds = [80, 90, 95, 100];
        assert_eq!(next_threshold(0, 89.0, &thresholds), Some(80));
        assert_eq!(next_threshold(80, 90.0, &thresholds), Some(90));
        assert_eq!(next_threshold(90, 98.0, &thresholds), Some(95));
        assert_eq!(next_threshold(95, 98.0, &thresholds), None);
    }

    #[test]
    fn reset_detection_requires_a_new_future_reset_timestamp() {
        assert!(quota_reset_occurred(Some(100), Some(200), 150));
        assert!(!quota_reset_occurred(Some(100), Some(100), 150));
        assert!(!quota_reset_occurred(Some(200), Some(100), 150));
        assert!(!quota_reset_occurred(None, Some(200), 150));
    }
}
