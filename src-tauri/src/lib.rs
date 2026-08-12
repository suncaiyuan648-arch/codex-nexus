mod codex;

use codex::{
    CodexRpcClient,
    ConnectionStatus,
};

use serde_json::{json, Value};

use std::{
    sync::Arc,
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
fn get_codex_snapshot(
    state: State<'_, CodexState>,
) -> Result<Value, String> {
    let client = &state.client;

    let (
        account,
        account_error,
    ) = match client.request(
        "account/read",
        Some(json!({
            "refreshToken": false
        })),
    ) {
        Ok(value) => (
            Some(value),
            None,
        ),

        Err(error) => (
            None,
            Some(error),
        ),
    };

    let rate_limits = client.request(
        "account/rateLimits/read",
        None,
    )?;

    let (
        usage,
        usage_error,
    ) = match client.request(
        "account/usage/read",
        None,
    ) {
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

        "rateLimits":
            rate_limits,

        "usage":
            usage,

        "usageError":
            usage_error
    }))
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
                        "Codex Usage Monitor"
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
                get_codex_connection_status
            ],
        )

        .run(
            tauri::generate_context!()
        )

        .expect(
            "error while running tauri application"
        );
}
