mod codex;

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
    menu::{
        Menu,
        MenuItem,
    },
    tray::{
        MouseButton,
        MouseButtonState,
        TrayIconBuilder,
        TrayIconEvent,
    },
    Manager,
    State,
};

pub struct CodexState {
    client: Arc<CodexRpcClient>,
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
    let client = &state.client;

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
        .setup(|app| {
            let client =
                CodexRpcClient::start(
                    app.handle().clone(),
                );

            app.manage(
                CodexState {
                    client,
                }
            );

            let show =
                MenuItem::with_id(
                    app,
                    "show",
                    "Show Codex Monitor",
                    true,
                    None::<&str>,
                )?;

            let quit =
                MenuItem::with_id(
                    app,
                    "quit",
                    "Quit",
                    true,
                    None::<&str>,
                )?;

            let menu =
                Menu::with_items(
                    app,
                    &[
                        &show,
                        &quit,
                    ],
                )?;

            let mut builder =
                TrayIconBuilder::new()

                    .tooltip(
                        "Codex Nexus"
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

            if let Some(icon) =
                app.default_window_icon()
            {
                builder =
                    builder.icon(
                        icon.clone()
                    );
            }

            builder.build(app)?;

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
