mod codex;

use chrono::{
    Datelike,
    Local,
    TimeZone,
    Timelike,
};

use codex::{
    CodexRpcClient,
    ConnectionStatus,
};

use serde_json::{json, Value};

use std::{
    sync::Arc,
    thread,
    time::{
        SystemTime,
        UNIX_EPOCH,
    },
};

use tauri::{
    image::Image,
    menu::{
        Menu,
        MenuItem,
        PredefinedMenuItem,
    },
    tray::{
        MouseButton,
        MouseButtonState,
        TrayIcon,
        TrayIconBuilder,
        TrayIconEvent,
    },
    Manager,
    Listener,
    State,
    WindowEvent,
    Wry,
};

#[cfg(target_os = "macos")]
const TRAY_ICON_BYTES: &[u8] =
    include_bytes!("../icons/tray/macos/tray@2x.png");

#[cfg(target_os = "windows")]
const TRAY_ICON_BYTES: &[u8] =
    include_bytes!("../icons/tray/windows/tray.ico");

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const TRAY_ICON_BYTES: &[u8] =
    include_bytes!("../icons/tray/macos/tray@2x.png");

const TRAY_ICON_IS_TEMPLATE: bool = cfg!(target_os = "macos");

pub struct CodexState {
    client: Arc<CodexRpcClient>,
}

struct TrayDisplay {
    tray: TrayIcon<Wry>,
    status: MenuItem<Wry>,
    weekly_title: MenuItem<Wry>,
    weekly_progress: MenuItem<Wry>,
    weekly_reset: MenuItem<Wry>,
    today: MenuItem<Wry>,
}

#[derive(Clone, Debug)]
struct TrayQuotaSummary {
    label: String,
    used_percent: f64,
    remaining_percent: f64,
    resets_at: Option<i64>,
    today_tokens: Option<f64>,
}

#[tauri::command]
fn get_codex_connection_status(
    state: State<'_, CodexState>,
) -> ConnectionStatus {
    state.client.status()
}

#[tauri::command]
fn reconnect_codex(
    state: State<'_, CodexState>,
) -> Result<(), String> {
    state.client.reconnect()
}

#[tauri::command]
fn get_codex_snapshot(
    state: State<'_, CodexState>,
) -> Result<Value, String> {
    fetch_codex_snapshot(&state.client)
}

fn fetch_codex_snapshot(
    client: &Arc<CodexRpcClient>,
) -> Result<Value, String> {

    // These RPCs are independent, so send all three before waiting for any
    // response. The client's request IDs and pending map route responses back
    // to the matching worker.
    let (
        account_result,
        rate_limits_result,
        usage_result,
    ) = thread::scope(|scope| {
        let account = scope.spawn(|| {
            client.request(
                "account/read",
                Some(json!({
                    "refreshToken": false
                })),
            )
        });

        let rate_limits = scope.spawn(|| {
            client.request(
                "account/rateLimits/read",
                None,
            )
        });

        let usage = scope.spawn(|| {
            client.request(
                "account/usage/read",
                None,
            )
        });

        let account_result = account
            .join()
            .map_err(|_| "Account RPC worker panicked".to_string())?;

        let rate_limits_result = rate_limits
            .join()
            .map_err(|_| "Rate limits RPC worker panicked".to_string())?;

        let usage_result = usage
            .join()
            .map_err(|_| "Usage RPC worker panicked".to_string())?;

        Ok::<_, String>(
            (
                account_result,
                rate_limits_result,
                usage_result,
            )
        )
    })?;

    let (
        account,
        account_error,
        account_state,
    ) = match account_result {
        Ok(value) => {
            let signed_in = value
                .get("account")
                .map(|account| !account.is_null())
                .unwrap_or(false);

            if signed_in {
                (
                    Some(value),
                    None,
                    "signedIn",
                )
            } else {
                (
                    Some(value),
                    None,
                    "signedOut",
                )
            }
        }

        Err(error) => (
            None,
            Some(error.clone()),
            classify_account_error(&error),
        ),
    };

    let rate_limits = rate_limits_result?;

    let (
        usage,
        usage_error,
    ) = match usage_result {
        Ok(value) => (
            Some(value),
            None,
        ),

        Err(error) => (
            None,
            Some(error),
        ),
    };

    let fetched_at =
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis();

    Ok(json!({
        "codexPath":
            client.display_path()
                .unwrap_or_default(),

        "fetchedAt":
            fetched_at,

        "account":
            account,

        "accountError":
            account_error,

        "accountState":
            account_state,

        "rateLimits":
            rate_limits,

        "usage":
            usage,

        "usageError":
            usage_error
    }))
}

fn tray_quota_summary(
    snapshot: &Value,
) -> TrayQuotaSummary {
    let rate_limits = snapshot
        .get("rateLimits")
        .and_then(Value::as_object);

    let mut windows = Vec::new();

    if let Some(rate_limits) = rate_limits {
        if let Some(by_id) = rate_limits
            .get("rateLimitsByLimitId")
            .and_then(Value::as_object)
        {
            for bucket in by_id.values() {
                collect_tray_windows(
                    bucket,
                    &mut windows,
                );
            }
        }

        if windows.is_empty() {
            if let Some(bucket) = rate_limits
                .get("rateLimits")
            {
                collect_tray_windows(
                    bucket,
                    &mut windows,
                );
            }
        }
    }

    windows.sort_by_key(|window| window.0);

    let selected = windows
        .iter()
        .find(|window| window.0 == 10080)
        .or_else(|| windows.last());

    let today_tokens = snapshot
        .get("usage")
        .and_then(|usage| usage.get("dailyUsageBuckets"))
        .and_then(Value::as_array)
        .and_then(|buckets| {
            let today = Local::now().date_naive().to_string();

            buckets
                .iter()
                .find(|bucket| {
                    bucket
                        .get("startDate")
                        .and_then(Value::as_str)
                        == Some(today.as_str())
                })
                .and_then(|bucket| {
                    bucket
                        .get("tokens")
                        .and_then(Value::as_f64)
                })
        });

    match selected {
        Some((duration, used_percent, resets_at)) => {
            let used_percent = clamp_percent(*used_percent);

            TrayQuotaSummary {
                label: tray_window_label(*duration),
                used_percent,
                remaining_percent:
                    (100.0 - used_percent).max(0.0),
                resets_at: *resets_at,
                today_tokens,
            }
        }

        None => TrayQuotaSummary {
            label: "每周".into(),
            used_percent: 0.0,
            remaining_percent: 100.0,
            resets_at: None,
            today_tokens,
        },
    }
}

fn collect_tray_windows(
    bucket: &Value,
    windows: &mut Vec<(u64, f64, Option<i64>)>,
) {
    for key in ["primary", "secondary"] {
        let Some(window) = bucket.get(key) else {
            continue;
        };

        let Some(duration) = window
            .get("windowDurationMins")
            .and_then(Value::as_u64)
        else {
            continue;
        };

        let used_percent = window
            .get("usedPercent")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);

        let resets_at = window
            .get("resetsAt")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_u64().map(|value| value as i64))
            });

        windows.push((duration, used_percent, resets_at));
    }
}

fn clamp_percent(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn tray_window_label(minutes: u64) -> String {
    match minutes {
        300 => "5 小时".into(),
        1440 => "每日".into(),
        10080 => "每周".into(),
        value if value % 10080 == 0 =>
            format!("{} 周", value / 10080),
        value if value % 1440 == 0 =>
            format!("{} 天", value / 1440),
        value if value % 60 == 0 =>
            format!("{} 小时", value / 60),
        value => format!("{} 分钟", value),
    }
}

fn format_tray_tokens(tokens: Option<f64>) -> String {
    let Some(tokens) = tokens else {
        return "暂无数据".into();
    };

    if tokens >= 1_000_000.0 {
        return format!("{:.1}M token", tokens / 1_000_000.0);
    }

    if tokens >= 1_000.0 {
        return format!("{:.1}K token", tokens / 1_000.0);
    }

    format!("{:.0} token", tokens)
}

fn tray_progress_bar(used_percent: f64) -> String {
    let filled = ((clamp_percent(used_percent) / 100.0) * 20.0)
        .round() as usize;

    format!(
        "{}{} {:.0}%",
        "█".repeat(filled.min(20)),
        "░".repeat(20usize.saturating_sub(filled)),
        clamp_percent(used_percent),
    )
}

fn format_tray_reset_relative(resets_at: Option<i64>) -> String {
    let Some(resets_at) = resets_at else {
        return "重置时间不可用".into();
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();

    let total_minutes = (resets_at - now).max(0) / 60;
    let days = total_minutes / 1440;
    let hours = (total_minutes % 1440) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        return format!("重置：{} 天 {} 小时后", days, hours);
    }

    if hours > 0 {
        return format!("重置：{} 小时 {} 分钟后", hours, minutes);
    }

    format!("重置：{} 分钟后", minutes)
}

fn format_tray_reset_absolute(resets_at: Option<i64>) -> String {
    let Some(resets_at) = resets_at else {
        return "重置时间不可用".into();
    };

    Local
        .timestamp_opt(resets_at, 0)
        .single()
        .map(|date| {
            format!(
                "重置：{}月{}日 {:02}:{:02}",
                date.month(),
                date.day(),
                date.hour(),
                date.minute(),
            )
        })
        .unwrap_or_else(|| "重置时间不可用".into())
}

fn update_tray_display(
    client: &Arc<CodexRpcClient>,
    display: &TrayDisplay,
) {
    let snapshot = match fetch_codex_snapshot(client) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let connected = client.status().phase == "ready";
            let _ = display.status.set_text(
                if connected {
                    "Codex 已连接 · 额度不可用"
                } else {
                    "Codex 未连接"
                },
            );
            let _ = display.weekly_progress.set_text("等待连接…");
            let _ = display.weekly_reset.set_text("重置时间不可用");
            let _ = display.today.set_text("今日 · 暂无数据");
            let _ = display.tray.set_tooltip(Some(format!(
                "Codex 用量\n{}\n{}",
                if connected { "额度暂不可用" } else { "未连接" },
                error,
            )));
            let _ = display.tray.set_title(Some(
                if connected { "Codex —" } else { "Codex" },
            ));
            return;
        }
    };

    let summary = tray_quota_summary(&snapshot);
    let used = summary.used_percent.round();
    let remaining = summary.remaining_percent.round();
    let reset_relative = format_tray_reset_relative(summary.resets_at);
    let reset_absolute = format_tray_reset_absolute(summary.resets_at);
    let account_available = snapshot
        .get("accountState")
        .and_then(Value::as_str)
        == Some("signedIn");

    let _ = display.status.set_text(
        if account_available {
            "Codex 已连接"
        } else {
            "Codex 已连接 · 账号不可用"
        },
    );
    let _ = display.weekly_title.set_text(&summary.label);
    let _ = display.weekly_progress.set_text(tray_progress_bar(
        summary.used_percent,
    ));
    let _ = display.weekly_reset.set_text(reset_absolute);
    let _ = display.today.set_text(format!(
        "今日 · {}",
        format_tray_tokens(summary.today_tokens),
    ));

    let tooltip = if account_available {
        format!(
            "Codex 用量\n{} {}%\n剩余 {}%\n{}",
            summary.label,
            used,
            remaining,
            reset_relative,
        )
    } else {
        "Codex 用量\n账号不可用\n请登录 Codex".into()
    };

    let _ = display.tray.set_tooltip(Some(tooltip));
    let _ = display.tray.set_title(Some(format!(
        "Codex {}",
        if account_available {
            format!("{:.0}%", used)
        } else {
            "—".into()
        },
    )));
}

fn update_tray_connection_state(
    display: &TrayDisplay,
    phase: &str,
) {
    let (status, message) = match phase {
        "connecting" | "initializing" =>
            ("Codex 正在连接…", "正在连接 Codex…"),
        "reconnecting" =>
            ("Codex 重连中…", "正在重新连接 Codex…"),
        "disconnected" =>
            ("Codex 未连接", "Codex 连接已断开"),
        _ => return,
    };

    let _ = display.status.set_text(status);
    let _ = display.weekly_progress.set_text(message);
    let _ = display.tray.set_tooltip(Some(format!(
        "Codex 用量\n{}",
        message,
    )));
    let _ = display.tray.set_title(Some("Codex"));
}

fn spawn_tray_refresh(
    client: Arc<CodexRpcClient>,
    display: Arc<TrayDisplay>,
) {
    thread::spawn(move || {
        update_tray_display(&client, &display);
    });
}

fn start_tray_updater(
    app: &tauri::App<Wry>,
    client: Arc<CodexRpcClient>,
    display: Arc<TrayDisplay>,
) {
    let periodic_client = Arc::clone(&client);
    let periodic_display = Arc::clone(&display);

    thread::spawn(move || loop {
        update_tray_display(
            &periodic_client,
            &periodic_display,
        );
        thread::sleep(std::time::Duration::from_secs(5 * 60));
    });

    let event_client = Arc::clone(&client);
    let event_display = Arc::clone(&display);
    app.listen_any(
        "codex://rate-limits-updated",
        move |_| {
            spawn_tray_refresh(
                Arc::clone(&event_client),
                Arc::clone(&event_display),
            );
        },
    );

    let event_client = Arc::clone(&client);
    let event_display = Arc::clone(&display);
    app.listen_any(
        "codex://account-updated",
        move |_| {
            spawn_tray_refresh(
                Arc::clone(&event_client),
                Arc::clone(&event_display),
            );
        },
    );

    let event_client = Arc::clone(&client);
    let event_display = Arc::clone(&display);
    app.listen_any(
        "codex://connection-state",
        move |event| {
            let phase = serde_json::from_str::<Value>(
                event.payload(),
            )
            .ok()
            .and_then(|payload| payload.get("phase").cloned())
            .and_then(|phase| phase.as_str().map(str::to_owned));

            if let Some(phase) = phase {
                if phase == "ready" {
                    spawn_tray_refresh(
                        Arc::clone(&event_client),
                        Arc::clone(&event_display),
                    );
                } else {
                    update_tray_connection_state(
                        &event_display,
                        &phase,
                    );
                }
            }
        },
    );
}

fn classify_account_error(error: &str) -> &'static str {
    let normalized = error.to_ascii_lowercase();

    if normalized.contains("not logged")
        || normalized.contains("not authenticated")
        || normalized.contains("unauthenticated")
        || normalized.contains("unauthorized")
        || normalized.contains("auth_required")
        || normalized.contains("auth required")
        || normalized.contains("not_logged")
        || normalized.contains("signed out")
        || normalized.contains("login required")
    {
        "signedOut"
    } else {
        "error"
    }
}

#[cfg_attr(
    mobile,
    tauri::mobile_entry_point
)]
pub fn run() {
    tauri::Builder::default()
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Closing the dashboard enters background mode. The tray icon
                // remains alive so Codex monitoring and quota refresh continue.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            let client =
                CodexRpcClient::start(
                    app.handle().clone(),
                );

            app.manage(
                CodexState {
                    client: Arc::clone(&client),
                }
            );

            let status =
                MenuItem::with_id(
                    app,
                    "status",
                    "Codex 正在连接…",
                    false,
                    None::<&str>,
                )?;

            let weekly_title =
                MenuItem::with_id(
                    app,
                    "weekly-title",
                    "每周",
                    false,
                    None::<&str>,
                )?;

            let weekly_progress =
                MenuItem::with_id(
                    app,
                    "weekly-progress",
                    "等待数据…",
                    false,
                    None::<&str>,
                )?;

            let weekly_reset =
                MenuItem::with_id(
                    app,
                    "weekly-reset",
                    "重置时间不可用",
                    false,
                    None::<&str>,
                )?;

            let today =
                MenuItem::with_id(
                    app,
                    "today",
                    "今日 · 暂无数据",
                    false,
                    None::<&str>,
                )?;

            let show =
                MenuItem::with_id(
                    app,
                    "show",
                    "打开监控面板",
                    true,
                    None::<&str>,
                )?;

            let quit =
                MenuItem::with_id(
                    app,
                    "quit",
                    "退出",
                    true,
                    None::<&str>,
                )?;

            let separator =
                PredefinedMenuItem::separator(app)?;

            let menu =
                Menu::with_items(
                    app,
                    &[
                        &status,
                        &weekly_title,
                        &weekly_progress,
                        &weekly_reset,
                        &today,
                        &separator,
                        &show,
                        &quit,
                    ],
                )?;

            let mut builder =
                TrayIconBuilder::with_id("codex-usage")

                    .tooltip(
                        "Codex 用量\n等待数据…"
                    )

                    .menu(
                        &menu
                    )

                    .show_menu_on_left_click(
                        false
                    )

                    .on_menu_event(
                        |app, event| {
                            match event.id.as_ref() {
                                "show" => {
                                    if let Some(window) =
                                        app.get_webview_window(
                                            "main"
                                        )
                                    {
                                        let _ =
                                            window.show();

                                        let _ =
                                            window.unminimize();

                                        let _ =
                                            window.set_focus();
                                    }
                                }

                                "quit" => {
                                    app.exit(0);
                                }

                                _ => {}
                            }
                        },
                    )

                    .on_tray_icon_event(
                        |tray, event| {
                            if let
                                TrayIconEvent::Click {
                                    button:
                                        MouseButton::Left,

                                    button_state:
                                        MouseButtonState::Up,

                                    ..
                                } = event
                            {
                                let app =
                                    tray.app_handle();

                                if let Some(window) =
                                    app.get_webview_window(
                                        "main"
                                    )
                                {
                                    let _ =
                                        window.show();

                                    let _ =
                                        window.unminimize();

                                    let _ =
                                        window.set_focus();
                                }
                            }
                        },
                    );

            // The menu bar uses a dedicated macOS template asset instead of
            // the application icon. Template images must be black/transparent
            // so macOS can adapt them to light and dark menu bars.
            let tray_icon = Image::from_bytes(TRAY_ICON_BYTES)?;

            builder = builder
                .icon(tray_icon)
                .icon_as_template(TRAY_ICON_IS_TEMPLATE);

            let tray = builder.build(app)?;

            let display = Arc::new(
                TrayDisplay {
                    tray,
                    status,
                    weekly_title,
                    weekly_progress,
                    weekly_reset,
                    today,
                },
            );

            start_tray_updater(
                app,
                Arc::clone(&client),
                display,
            );

            Ok(())
        })

        .invoke_handler(
            tauri::generate_handler![
                get_codex_snapshot,
                get_codex_connection_status,
                reconnect_codex
            ],
        )

        .run(
            tauri::generate_context!()
        )

        .expect(
            "error while running tauri application"
        );
}
