use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Listener, Manager, Wry};

use crate::{codex::CodexRpcClient, monitor};

use super::{quota, recorder, rollout};

const LOCAL_USAGE_DEBOUNCE: Duration = Duration::from_secs(3);
const SETTLEMENT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(12), Duration::from_secs(30)];
const LONG_IDLE_AFTER: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RefreshPolicy {
    Adaptive,
    FifteenSeconds,
    ThirtySeconds,
    OneMinute,
    ThreeMinutes,
    FiveMinutes,
    FiveSeconds,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self::Adaptive
    }
}

impl RefreshPolicy {
    pub fn parse(value: &str) -> Self {
        match value {
            "15s" => Self::FifteenSeconds,
            "30s" => Self::ThirtySeconds,
            "1m" => Self::OneMinute,
            "3m" => Self::ThreeMinutes,
            "5m" => Self::FiveMinutes,
            "5s" => Self::FiveSeconds,
            _ => Self::Adaptive,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Adaptive => "adaptive",
            Self::FifteenSeconds => "15s",
            Self::ThirtySeconds => "30s",
            Self::OneMinute => "1m",
            Self::ThreeMinutes => "3m",
            Self::FiveMinutes => "5m",
            Self::FiveSeconds => "5s",
        }
    }

    fn fallback_duration(&self, foreground: bool, recently_active: bool) -> Duration {
        match self {
            Self::Adaptive if recently_active && foreground => Duration::from_secs(60),
            Self::Adaptive if recently_active => Duration::from_secs(3 * 60),
            Self::Adaptive => Duration::from_secs(5 * 60),
            Self::FifteenSeconds => Duration::from_secs(15),
            Self::ThirtySeconds => Duration::from_secs(30),
            Self::OneMinute => Duration::from_secs(60),
            Self::ThreeMinutes => Duration::from_secs(3 * 60),
            Self::FiveMinutes => Duration::from_secs(5 * 60),
            Self::FiveSeconds => Duration::from_secs(5),
        }
    }
}

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
}

#[derive(Clone, Debug)]
enum SchedulerCommand {
    ConnectionReady,
    AccountUpdated,
    LocalFileChanged(PathBuf),
    RateLimitUpdated,
    PolicyChanged(String),
    RefreshNow(Option<mpsc::Sender<Result<(), String>>>),
}

pub struct UsageRefreshScheduler {
    sender: mpsc::Sender<SchedulerCommand>,
    latest_snapshot: Arc<Mutex<Option<Value>>>,
    status: Arc<Mutex<UsageSchedulerStatus>>,
}

pub struct UsageSchedulerState {
    pub scheduler: Arc<UsageRefreshScheduler>,
}

#[derive(Clone, Debug)]
struct QuotaMarker {
    limit_id: String,
    window: String,
    window_duration_mins: i64,
    used_percent: f64,
    resets_at: Option<i64>,
}

#[derive(Clone, Debug)]
struct SettlementState {
    stage: usize,
    baseline: Option<QuotaMarker>,
    next_at: Instant,
}

impl UsageRefreshScheduler {
    pub fn start(app: AppHandle<Wry>, client: Arc<CodexRpcClient>) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel();
        let latest_snapshot = Arc::new(Mutex::new(None));
        let initial_policy = monitor::load_settings(&app)
            .map(|settings| RefreshPolicy::parse(&settings.usage_refresh_policy))
            .unwrap_or_default();
        let status = Arc::new(Mutex::new(UsageSchedulerStatus {
            policy: initial_policy.as_str().into(),
            mode: "starting".into(),
            watcher_active: false,
            pending_reconciliation: false,
            fallback_seconds: 0,
            last_refresh_at: None,
            last_local_activity_at: None,
        }));
        let scheduler = Arc::new(Self {
            sender: sender.clone(),
            latest_snapshot: Arc::clone(&latest_snapshot),
            status: Arc::clone(&status),
        });

        install_app_event_bridges(&app, &sender);
        let watcher_active = install_rollout_watcher(sender.clone());
        if let Ok(mut current) = status.lock() {
            current.watcher_active = watcher_active;
        }

        let thread_app = app.clone();
        let thread_latest = Arc::clone(&latest_snapshot);
        let thread_status = Arc::clone(&status);
        thread::Builder::new()
            .name("usage-refresh-scheduler".into())
            .spawn(move || {
                run_scheduler(
                    thread_app,
                    client,
                    receiver,
                    initial_policy,
                    thread_latest,
                    thread_status,
                    watcher_active,
                );
            })
            .expect("failed to start UsageRefreshScheduler");

        if scheduler_status_ready(&scheduler) {
            let _ = sender.send(SchedulerCommand::ConnectionReady);
        }
        scheduler
    }

    pub fn set_policy(&self, policy: String) {
        let _ = self.sender.send(SchedulerCommand::PolicyChanged(policy));
    }

    pub fn refresh_now_blocking(&self) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        self.sender
            .send(SchedulerCommand::RefreshNow(Some(sender)))
            .map_err(|error| error.to_string())?;
        receiver
            .recv_timeout(Duration::from_secs(60))
            .map_err(|error| error.to_string())?
    }

    pub fn cached_snapshot(&self) -> Option<Value> {
        self.latest_snapshot
            .lock()
            .ok()
            .and_then(|snapshot| snapshot.clone())
    }

    pub fn status(&self) -> UsageSchedulerStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or(UsageSchedulerStatus {
                policy: RefreshPolicy::Adaptive.as_str().into(),
                mode: "unavailable".into(),
                watcher_active: false,
                pending_reconciliation: false,
                fallback_seconds: 0,
                last_refresh_at: None,
                last_local_activity_at: None,
            })
    }
}

fn scheduler_status_ready(scheduler: &UsageRefreshScheduler) -> bool {
    scheduler
        .status
        .lock()
        .map(|status| status.mode == "starting")
        .unwrap_or(false)
}

fn install_app_event_bridges(app: &AppHandle<Wry>, sender: &mpsc::Sender<SchedulerCommand>) {
    let account_sender = sender.clone();
    app.listen_any("codex://account-updated", move |_| {
        let _ = account_sender.send(SchedulerCommand::AccountUpdated);
    });

    let connection_sender = sender.clone();
    app.listen_any("codex://connection-state", move |event| {
        let ready = serde_json::from_str::<Value>(event.payload())
            .ok()
            .and_then(|payload| {
                payload
                    .get("phase")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|phase| phase == "ready");
        if ready {
            let _ = connection_sender.send(SchedulerCommand::ConnectionReady);
        }
    });

    // rateLimits/updated is persisted synchronously in the Codex notification
    // path. The scheduler receives the event only to publish a fresh status;
    // it must never turn this event into a full account/usage poll.
    let rate_limit_sender = sender.clone();
    app.listen_any("codex://rate-limits-updated", move |_| {
        let _ = rate_limit_sender.send(SchedulerCommand::RateLimitUpdated);
    });
}

fn install_rollout_watcher(sender: mpsc::Sender<SchedulerCommand>) -> bool {
    let roots = rollout::rollout_watch_roots();
    if roots.is_empty() {
        return false;
    }
    thread::Builder::new()
        .name("usage-rollout-watcher".into())
        .spawn(move || {
            let callback_sender = sender.clone();
            let mut watcher =
                match notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                    let Ok(event) = result else { return };
                    if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                        return;
                    }
                    for path in event.paths {
                        if path.extension().and_then(|extension| extension.to_str())
                            == Some("jsonl")
                        {
                            let _ = callback_sender.send(SchedulerCommand::LocalFileChanged(path));
                        }
                    }
                }) {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        eprintln!("[Usage] rollout watcher failed to start: {error}");
                        return;
                    }
                };
            for root in roots {
                if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
                    eprintln!(
                        "[Usage] rollout watcher failed for {}: {error}",
                        root.display()
                    );
                }
            }
            loop {
                thread::park_timeout(Duration::from_secs(60 * 60));
            }
        })
        .is_ok()
}

fn run_scheduler(
    app: AppHandle<Wry>,
    client: Arc<CodexRpcClient>,
    receiver: mpsc::Receiver<SchedulerCommand>,
    mut policy: RefreshPolicy,
    latest_snapshot: Arc<Mutex<Option<Value>>>,
    status: Arc<Mutex<UsageSchedulerStatus>>,
    watcher_active: bool,
) {
    let _ = rollout::collect_rollouts(&app);
    let mut next_fallback = Instant::now() + policy.fallback_duration(is_foreground(&app), false);
    let mut next_thread_usage: Option<Instant> = None;
    let mut debounce_deadline: Option<Instant> = None;
    let mut settlement: Option<SettlementState> = None;
    let mut last_local_activity: Option<Instant> = None;
    let mut last_local_activity_at: Option<i64> = None;
    let mut last_refresh_at: Option<i64> = None;

    loop {
        let foreground = is_foreground(&app);
        let recently_active = last_local_activity
            .map(|instant| instant.elapsed() < LONG_IDLE_AFTER)
            .unwrap_or(false);
        let fallback = policy.fallback_duration(foreground, recently_active);
        let now = Instant::now();
        if next_fallback <= now {
            next_fallback = now + fallback;
        }
        let wait = [
            Some(next_fallback.saturating_duration_since(now)),
            debounce_deadline.map(|deadline| deadline.saturating_duration_since(now)),
            settlement
                .as_ref()
                .map(|state| state.next_at.saturating_duration_since(now)),
            next_thread_usage.map(|deadline| deadline.saturating_duration_since(now)),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(fallback);
        publish_status(
            &app,
            &status,
            &policy,
            foreground,
            fallback,
            debounce_deadline.is_some() || settlement.is_some(),
            watcher_active,
            last_refresh_at,
            last_local_activity_at,
        );

        match receiver.recv_timeout(wait) {
            Ok(SchedulerCommand::ConnectionReady) | Ok(SchedulerCommand::AccountUpdated) => {
                debounce_deadline = None;
                settlement = None;
                if let Err(error) =
                    perform_full_refresh(&app, &client, &latest_snapshot, &mut last_refresh_at)
                {
                    eprintln!("[Usage] scheduler full refresh failed: {error}");
                }
                next_thread_usage = thread_usage_deadline(&app);
                next_fallback = Instant::now() + fallback;
            }
            Ok(SchedulerCommand::RefreshNow(response)) => {
                let result =
                    perform_full_refresh(&app, &client, &latest_snapshot, &mut last_refresh_at);
                if let Err(error) = &result {
                    eprintln!("[Usage] scheduler manual refresh failed: {error}");
                }
                next_thread_usage = thread_usage_deadline(&app);
                if let Some(response) = response {
                    let _ = response.send(result);
                }
                next_fallback = Instant::now() + fallback;
            }
            Ok(SchedulerCommand::PolicyChanged(value)) => {
                policy = RefreshPolicy::parse(&value);
                next_fallback = Instant::now();
            }
            Ok(SchedulerCommand::RateLimitUpdated) => {
                // The notification path has already persisted the sample.
                // It also gives the short-lived settlement loop a chance to
                // stop early when the server has settled the Turn.
                if let Some(state) = settlement.as_ref() {
                    if quota_marker_changed(&state.baseline, &latest_weekly_quota_marker(&app)) {
                        settlement = None;
                    }
                }
            }
            Ok(SchedulerCommand::LocalFileChanged(path)) => {
                if !path.as_os_str().is_empty() {
                    match rollout::collect_rollout_file(&app, &path) {
                        Ok(true) => {
                            last_local_activity = Some(Instant::now());
                            last_local_activity_at = Some(now_seconds());
                            debounce_deadline = Some(Instant::now() + LOCAL_USAGE_DEBOUNCE);
                            settlement = Some(SettlementState {
                                stage: 0,
                                baseline: latest_weekly_quota_marker(&app),
                                next_at: Instant::now() + LOCAL_USAGE_DEBOUNCE,
                            });
                            next_thread_usage = None;
                        }
                        Ok(false) => {}
                        Err(error) => eprintln!("[Usage] rollout event processing failed: {error}"),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut reconciled = false;
                let mut thread_usage_refreshed = false;
                if debounce_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                    if let Err(error) = perform_thread_usage_refresh(&app, &client) {
                        eprintln!("[Usage] thread usage refresh failed: {error}");
                    }
                    next_thread_usage = thread_usage_deadline(&app);
                    thread_usage_refreshed = true;
                    if let Err(error) = perform_reconciliation(
                        &app,
                        &client,
                        &latest_snapshot,
                        &mut last_refresh_at,
                    ) {
                        eprintln!("[Usage] scheduler reconciliation failed: {error}");
                    }
                    debounce_deadline = None;
                    reconciled = true;
                    settle_after_reconciliation(&app, &mut settlement);
                } else if settlement
                    .as_ref()
                    .is_some_and(|state| state.next_at <= Instant::now())
                {
                    if let Err(error) = perform_reconciliation(
                        &app,
                        &client,
                        &latest_snapshot,
                        &mut last_refresh_at,
                    ) {
                        eprintln!("[Usage] settlement reconciliation failed: {error}");
                    }
                    settle_after_reconciliation(&app, &mut settlement);
                    reconciled = true;
                }
                if next_thread_usage.is_some_and(|deadline| deadline <= Instant::now())
                    && !thread_usage_refreshed
                {
                    if let Err(error) = perform_thread_usage_refresh(&app, &client) {
                        eprintln!("[Usage] scheduled thread usage refresh failed: {error}");
                    }
                    next_thread_usage = thread_usage_deadline(&app);
                }
                if next_fallback <= Instant::now() && !reconciled {
                    if let Err(error) = perform_reconciliation(
                        &app,
                        &client,
                        &latest_snapshot,
                        &mut last_refresh_at,
                    ) {
                        eprintln!("[Usage] scheduler fallback refresh failed: {error}");
                    }
                    next_fallback = Instant::now()
                        + policy.fallback_duration(
                            is_foreground(&app),
                            last_local_activity
                                .map(|instant| instant.elapsed() < LONG_IDLE_AFTER)
                                .unwrap_or(false),
                        );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn latest_weekly_quota_marker(app: &AppHandle<Wry>) -> Option<QuotaMarker> {
    let connection = super::db::open_database(app).ok()?;
    let account_key = recorder::current_account_key(&connection).ok()??;
    connection
        .query_row(
            "SELECT limit_id, window, window_duration_mins, used_percent, resets_at
             FROM rate_limit_samples
             WHERE account_key = ?1 AND window_duration_mins = 10080
             ORDER BY sampled_at DESC, id DESC LIMIT 1",
            [&account_key],
            |row| {
                let limit_id: String = row.get(0)?;
                let window: String = row.get(1)?;
                let window_duration_mins: i64 = row.get(2)?;
                let used_percent: f64 = row.get(3)?;
                let resets_at: Option<i64> = row.get(4)?;
                Ok(QuotaMarker {
                    limit_id,
                    window,
                    window_duration_mins,
                    used_percent,
                    resets_at,
                })
            },
        )
        .ok()
}

fn quota_marker_changed(before: &Option<QuotaMarker>, after: &Option<QuotaMarker>) -> bool {
    match (before, after) {
        (None, None) => false,
        (None, Some(_)) | (Some(_), None) => true,
        (Some(before), Some(after)) => {
            before.limit_id != after.limit_id
                || before.window != after.window
                || before.window_duration_mins != after.window_duration_mins
                || (before.used_percent - after.used_percent).abs() > f64::EPSILON
                || !quota::same_reset_at(before.resets_at, after.resets_at)
        }
    }
}

fn settle_after_reconciliation(app: &AppHandle<Wry>, settlement: &mut Option<SettlementState>) {
    let Some(state) = settlement.as_mut() else {
        return;
    };
    if quota_marker_changed(&state.baseline, &latest_weekly_quota_marker(app))
        || state.stage >= SETTLEMENT_RETRY_DELAYS.len()
    {
        *settlement = None;
        return;
    }
    state.next_at = Instant::now() + SETTLEMENT_RETRY_DELAYS[state.stage];
    state.stage += 1;
}

fn perform_thread_usage_refresh(
    app: &AppHandle<Wry>,
    client: &Arc<CodexRpcClient>,
) -> Result<(), String> {
    if client.status().phase != "ready" {
        return Ok(());
    }
    let connection = super::db::open_database(app)?;
    let thread_ids = recorder::pending_thread_usage_threads(&connection, now_seconds())?;
    drop(connection);

    for thread_id in thread_ids {
        let result = client.request("account/usage/read", Some(json!({ "threadId": thread_id })));
        match result {
            Ok(response) => {
                recorder::record_thread_usage_snapshot(app, &thread_id, &response)?;
            }
            Err(error) => {
                recorder::record_thread_usage_failure(app, &thread_id, &error)?;
            }
        }
    }
    Ok(())
}

fn thread_usage_deadline(app: &AppHandle<Wry>) -> Option<Instant> {
    let connection = super::db::open_database(app).ok()?;
    let due_at = recorder::next_thread_usage_at(&connection, now_seconds()).ok()??;
    let delay = due_at.saturating_sub(now_seconds()) as u64;
    Some(Instant::now() + Duration::from_secs(delay.max(1)))
}

fn is_foreground(app: &AppHandle<Wry>) -> bool {
    app.get_webview_window("main")
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn now_seconds() -> i64 {
    now_millis() / 1000
}

fn perform_full_refresh(
    app: &AppHandle<Wry>,
    client: &Arc<CodexRpcClient>,
    latest_snapshot: &Arc<Mutex<Option<Value>>>,
    last_refresh_at: &mut Option<i64>,
) -> Result<(), String> {
    if client.status().phase != "ready" {
        return Ok(());
    }
    let snapshot = crate::fetch_codex_snapshot(client)?;
    if let Err(error) = perform_thread_usage_refresh(app, client) {
        eprintln!("[Usage] initial thread usage refresh failed: {error}");
    }
    apply_snapshot(app, snapshot, latest_snapshot, last_refresh_at)
}

fn perform_reconciliation(
    app: &AppHandle<Wry>,
    client: &Arc<CodexRpcClient>,
    latest_snapshot: &Arc<Mutex<Option<Value>>>,
    last_refresh_at: &mut Option<i64>,
) -> Result<(), String> {
    if client.status().phase != "ready" {
        return Ok(());
    }
    let (usage, rate_limits) = std::thread::scope(|scope| {
        let usage = scope.spawn(|| client.request("account/usage/read", None));
        let rate_limits = scope.spawn(|| client.request("account/rateLimits/read", None));
        let usage = usage
            .join()
            .map_err(|_| "Usage RPC worker panicked".to_string())??;
        let rate_limits = rate_limits
            .join()
            .map_err(|_| "Rate-limit RPC worker panicked".to_string())??;
        Ok::<_, String>((usage, rate_limits))
    })?;
    let mut snapshot = latest_snapshot
        .lock()
        .ok()
        .and_then(|snapshot| snapshot.clone())
        .unwrap_or_else(|| json!({"codexPath": client.display_path().unwrap_or_default()}));
    if let Some(object) = snapshot.as_object_mut() {
        object.insert("fetchedAt".into(), json!(now_millis()));
        object.insert("usage".into(), usage);
        object.insert("rateLimits".into(), rate_limits);
    }
    apply_snapshot(app, snapshot, latest_snapshot, last_refresh_at)
}

fn apply_snapshot(
    app: &AppHandle<Wry>,
    snapshot: Value,
    latest_snapshot: &Arc<Mutex<Option<Value>>>,
    last_refresh_at: &mut Option<i64>,
) -> Result<(), String> {
    if let Err(error) = recorder::record_official_snapshot(app, &snapshot) {
        eprintln!("[Usage] scheduler failed to record snapshot: {error}");
    }
    monitor::process_snapshot(app, &snapshot);
    if let Ok(mut current) = latest_snapshot.lock() {
        *current = Some(snapshot.clone());
    }
    *last_refresh_at = Some(now_seconds());
    app.emit("codex://usage-snapshot", snapshot)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn publish_status(
    app: &AppHandle<Wry>,
    status: &Arc<Mutex<UsageSchedulerStatus>>,
    policy: &RefreshPolicy,
    foreground: bool,
    fallback: Duration,
    pending_reconciliation: bool,
    watcher_active: bool,
    last_refresh_at: Option<i64>,
    last_local_activity_at: Option<i64>,
) {
    let next = UsageSchedulerStatus {
        policy: policy.as_str().into(),
        mode: if foreground {
            "foreground"
        } else {
            "background"
        }
        .into(),
        watcher_active,
        pending_reconciliation,
        fallback_seconds: fallback.as_secs(),
        last_refresh_at,
        last_local_activity_at,
    };
    if let Ok(mut current) = status.lock() {
        *current = next.clone();
    }
    let _ = app.emit("codex://usage-refresh-state", next);
}
