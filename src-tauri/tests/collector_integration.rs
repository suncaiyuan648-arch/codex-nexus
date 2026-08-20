use std::{
    path::PathBuf,
    process::{Child, Command},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

use codex_nexus_lib::usage::collector_ipc;
use serde_json::Value;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temp_database(_label: &str) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "cn-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        sequence
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join("usage.db")
}

fn wait_for_status(endpoint: &PathBuf) -> Value {
    for _ in 0..80 {
        if let Ok(value) = collector_ipc::request_path(endpoint, "GET_STATUS", Value::Null) {
            return value;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("standalone collector did not publish an IPC status");
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
#[test]
fn standalone_collector_process_ipc_is_secure_bounded_and_restartable() {
    use std::{
        fs,
        io::{Read, Write},
        os::unix::fs::PermissionsExt,
        os::unix::net::UnixStream,
    };

    let database = temp_database("integration");
    let endpoint = database.with_file_name(collector_ipc::IPC_SOCKET_FILE);
    let binary = std::env::var("CARGO_BIN_EXE_nexus-collector").expect("cargo binary path");
    let mut child = Command::new(binary)
        .args(["--database", database.to_str().unwrap()])
        .spawn()
        .unwrap();
    let status = wait_for_status(&endpoint);
    assert_eq!(status["collector"]["status"], "running");
    assert!(status["codex"]["phase"].is_string());
    assert!(
        collector_ipc::request_path(&endpoint, "REFRESH_NOW", Value::Null)
            .unwrap()
            .as_u64()
            .is_some()
    );
    assert!(collector_ipc::request_path(&endpoint, "GET_DATA_HEALTH", Value::Null).is_ok());
    assert!(collector_ipc::request_path(&endpoint, "GET_CODEX_STATUS", Value::Null).is_ok());
    assert!(collector_ipc::request_path(&endpoint, "GET_ACCOUNT", Value::Null).is_ok());
    assert!(collector_ipc::request_path(
        &endpoint,
        "GET_CATEGORY_USAGE",
        serde_json::json!({"period": "day"}),
    )
    .is_ok());
    assert!(collector_ipc::request_path(&endpoint, "GET_SNAPSHOT", Value::Null).is_ok());
    assert!(collector_ipc::request_path(
        &endpoint,
        "REBUILD_ACCOUNT",
        serde_json::json!({"accountKey": "unresolved:test"}),
    )
    .is_err());
    assert!(collector_ipc::request_path(&endpoint, "NOT_A_METHOD", Value::Null).is_err());
    let mode = fs::symlink_metadata(&endpoint)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    // A client that never sends a newline cannot monopolize the accept loop.
    let mut stalled = UnixStream::connect(&endpoint).unwrap();
    stalled
        .set_read_timeout(Some(collector_ipc::IO_TIMEOUT + Duration::from_secs(1)))
        .unwrap();
    thread::sleep(collector_ipc::IO_TIMEOUT + Duration::from_millis(100));
    let mut stalled_response = Vec::new();
    stalled.read_to_end(&mut stalled_response).unwrap();
    assert!(!stalled_response.is_empty());
    assert_eq!(wait_for_status(&endpoint)["collector"]["status"], "running");
    drop(stalled);

    // The UI proxy refuses endpoints that are not owned/locked down, even if
    // the socket is otherwise connectable.
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(collector_ipc::request_path(&endpoint, "GET_STATUS", Value::Null).is_err());
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(wait_for_status(&endpoint)["collector"]["status"], "running");

    // Oversized requests are rejected without affecting later clients.
    let mut oversized = UnixStream::connect(&endpoint).unwrap();
    oversized
        .write_all(&vec![b'x'; collector_ipc::MAX_REQUEST_BYTES + 1])
        .unwrap();
    oversized.write_all(b"\n").unwrap();
    drop(oversized);
    assert_eq!(wait_for_status(&endpoint)["collector"]["status"], "running");
    stop(&mut child);

    // The OS lock and endpoint can be recovered after a process restart.
    let mut restarted = Command::new(std::env::var("CARGO_BIN_EXE_nexus-collector").unwrap())
        .args(["--database", database.to_str().unwrap()])
        .spawn()
        .unwrap();
    assert_eq!(wait_for_status(&endpoint)["collector"]["status"], "running");
    stop(&mut restarted);
    let _ = fs::remove_file(endpoint);
    let _ = fs::remove_file(&database);
    let _ = fs::remove_file(database.with_extension("collector.lock"));
    let _ = fs::remove_dir_all(database.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn standalone_collector_retroactively_binds_only_live_tail_with_account_evidence() {
    use codex_nexus_lib::usage::{collector_core, db, recorder};
    use std::{fs::OpenOptions, io::Write};

    let database = temp_database("retroactive");
    let rollout = temp_database("retroactive-rollout").with_extension("jsonl");
    {
        let mut file = std::fs::File::create(&rollout).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{{"session_id":"s"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10,"total_tokens":10}}}}}}}}"#).unwrap();
    }
    let connection = db::open_standalone_database(&database).unwrap();
    let source = collector_core::register_source(&connection, &rollout, 10).unwrap();
    collector_core::catch_up_path(&connection, &rollout, None, false, 11).unwrap();
    recorder::ensure_account(&connection, "account:a", 12).unwrap();
    collector_core::record_account_presence(
        &connection,
        "account:a",
        12,
        "account_read",
        "high",
        None,
    )
    .unwrap();
    drop(connection);

    let seeded = db::open_standalone_readonly(&database).unwrap();
    assert_eq!(
        recorder::current_account_key(&seeded).unwrap(),
        Some("account:a".into())
    );
    let seeded_row = seeded
        .query_row(
            "SELECT binding_status, last_offset FROM rollout_sources WHERE source_id = ?1",
            [&source],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(seeded_row.0, collector_core::BINDING_UNRESOLVED);
    drop(seeded);

    let mut file = OpenOptions::new().append(true).open(&rollout).unwrap();
    writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":20,"total_tokens":20}}}}}}}}"#).unwrap();
    drop(file);

    let endpoint = database.with_file_name(collector_ipc::IPC_SOCKET_FILE);
    let binary = std::env::var("CARGO_BIN_EXE_nexus-collector").unwrap();
    let mut child = Command::new(binary)
        .args(["--database", database.to_str().unwrap()])
        .spawn()
        .unwrap();
    wait_for_status(&endpoint);
    let mut bound = false;
    for _ in 0..160 {
        if let Ok(read) = db::open_standalone_readonly(&database) {
            bound = read
                .query_row(
                    "SELECT binding_status FROM rollout_sources WHERE source_id = ?1",
                    [&source],
                    |row| row.get::<_, String>(0),
                )
                .map(|status| status == collector_core::BINDING_VERIFIED)
                .unwrap_or(false);
            if bound {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    stop(&mut child);
    assert!(bound, "collector did not retroactively bind the live tail");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_file(&database);
    let _ = std::fs::remove_file(database.with_extension("collector.lock"));
    let _ = std::fs::remove_file(rollout);
    let _ = std::fs::remove_dir_all(database.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn standalone_collector_preserves_unresolved_tail_across_account_switch_without_evidence() {
    use codex_nexus_lib::usage::{collector_core, db, recorder};
    use std::{fs, io::Write, os::unix::fs::PermissionsExt};

    let database = temp_database("account-switch");
    let rollout = temp_database("account-switch-rollout").with_extension("jsonl");
    {
        let mut file = fs::File::create(&rollout).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{{"session_id":"switch"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{{"turn_id":"t","model":"gpt-5.6-sol"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10,"total_tokens":10}}}}}}}}"#).unwrap();
    }
    let connection = db::open_standalone_database(&database).unwrap();
    let source = collector_core::register_source(&connection, &rollout, 10).unwrap();
    collector_core::catch_up_path(&connection, &rollout, None, false, 11).unwrap();
    recorder::ensure_account(&connection, "account:a", 12).unwrap();
    recorder::ensure_account(&connection, "account:b", 13).unwrap();
    collector_core::record_account_presence(
        &connection,
        "account:a",
        12,
        "account_read",
        "high",
        None,
    )
    .unwrap();
    recorder::record_rate_limit_update_connection(
        &connection,
        &serde_json::json!({
            "fetchedAt": 13_000,
            "rateLimits": {
                "rateLimitsByLimitId": {
                    "codex": {
                        "primary": {"windowDurationMins": 10080, "usedPercent": 7.0, "resetsAt": 99}
                    }
                }
            }
        }),
    )
    .unwrap();
    drop(connection);

    let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
    writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":20,"total_tokens":20}}}}}}}}"#).unwrap();

    let fake_codex = temp_database("account-switch-codex").with_extension("sh");
    fs::write(
        &fake_codex,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
      case "$line" in
        *'"method":"account/read"'*) printf '{"id":%s,"error":{"code":-32001,"message":"account transport unavailable"}}\n' "$id"; continue ;;
        *'"method":"account/rateLimits/read"'*) printf '{"id":%s,"error":{"code":-32000,"message":"offline"}}\n' "$id"; continue ;;
        *) result='{}' ;;
      esac
      printf '{"id":%s,"result":%s}\n' "$id" "$result"
      ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();

    let endpoint = database.with_file_name(collector_ipc::IPC_SOCKET_FILE);
    let binary = std::env::var("CARGO_BIN_EXE_nexus-collector").unwrap();
    let mut child = Command::new(binary)
        .args(["--database", database.to_str().unwrap()])
        .env("CODEX_BIN", &fake_codex)
        .env("HOME", database.parent().unwrap())
        .spawn()
        .unwrap();
    wait_for_status(&endpoint);
    let mut refresh_failed = false;
    for _ in 0..40 {
        if let Ok(status) = collector_ipc::request_path(&endpoint, "GET_STATUS", Value::Null) {
            refresh_failed = status["scheduler"]["refreshing"] == false
                && status["scheduler"]["refreshError"].is_string();
            if refresh_failed {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        refresh_failed,
        "refresh failure was not exposed through GET_STATUS"
    );
    let read = db::open_standalone_readonly(&database).unwrap();
    let (status, account): (String, Option<String>) = read
        .query_row(
            "SELECT binding_status, account_key FROM rollout_sources WHERE source_id = ?1",
            [&source],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, collector_core::BINDING_UNRESOLVED);
    assert_eq!(account, None);
    assert_eq!(recorder::current_account_key(&read).unwrap(), None);
    let old_presence_ended: Option<i64> = read
        .query_row(
            "SELECT ended_at FROM account_presence_intervals
             WHERE account_key = 'account:a' ORDER BY started_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        old_presence_ended.is_some(),
        "account/read failure left old presence open"
    );
    let unresolved_presence: Option<String> = read
        .query_row(
            "SELECT account_key FROM account_presence_intervals
             WHERE ended_at IS NULL ORDER BY started_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();
    assert!(
        unresolved_presence
            .as_deref()
            .is_some_and(|key| key.starts_with("unresolved:official:")),
        "account/read failure did not establish an unresolved identity fence: {unresolved_presence:?}"
    );
    let old_rate_limit_samples: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM rate_limit_samples WHERE account_key = 'account:a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_rate_limit_samples, 1);
    stop(&mut child);
    let _ = fs::remove_file(endpoint);
    let _ = fs::remove_file(&database);
    let _ = fs::remove_file(database.with_extension("collector.lock"));
    let _ = fs::remove_file(rollout);
    let _ = fs::remove_file(fake_codex);
    let _ = fs::remove_dir_all(database.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn standalone_collector_rechecks_account_for_each_source_during_a_switch() {
    use codex_nexus_lib::usage::{collector_core, db};
    use std::{fs, io::Write, os::unix::fs::PermissionsExt};

    let database = temp_database("source-switch");
    let source_a_path = temp_database("source-a").with_extension("jsonl");
    let source_b_path = temp_database("source-b").with_extension("jsonl");
    for (path, session) in [(&source_a_path, "source-a"), (&source_b_path, "source-b")] {
        let mut file = fs::File::create(path).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{{"session_id":"{session}"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{{"turn_id":"t","model":"gpt-5.6-sol"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10,"total_tokens":10}}}}}}}}"#).unwrap();
    }
    let connection = db::open_standalone_database(&database).unwrap();
    let source_a = collector_core::register_source(&connection, &source_a_path, 10).unwrap();
    let source_b = collector_core::register_source(&connection, &source_b_path, 10).unwrap();
    let canonical_a = source_a_path.canonicalize().unwrap();
    let canonical_b = source_b_path.canonicalize().unwrap();
    let registered_paths = collector_core::registered_source_paths(&connection).unwrap();
    assert_eq!(
        registered_paths,
        vec![canonical_a.clone(), canonical_b.clone()],
        "registered source traversal must follow canonical path order"
    );
    collector_core::catch_up_path(&connection, &source_a_path, None, false, 11).unwrap();
    collector_core::catch_up_path(&connection, &source_b_path, None, false, 11).unwrap();
    drop(connection);

    // The initial history is fully consumed first. Only these later appends
    // are ownership evidence eligible for retroactive binding.
    for path in [&source_a_path, &source_b_path] {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-14T00:00:03Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":20,"total_tokens":20}}}}}}}}"#).unwrap();
    }

    let fake_codex = temp_database("source-switch-codex").with_extension("sh");
    fs::write(&fake_codex, r#"#!/bin/sh
account_reads=0
while IFS= read -r line; do
  case "$line" in
    *'"id":'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
      case "$line" in
        *'"method":"account/read"'*)
          account_reads=$((account_reads + 1))
          if [ "$account_reads" -eq 1 ]; then result='{"account":{"id":"a"}}'; else sleep 1; result='{"account":{"id":"b"}}'; fi
          ;;
        *) result='{}' ;;
      esac
      printf '{"id":%s,"result":%s}\n' "$id" "$result"
      ;;
  esac
done
"#).unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();

    let endpoint = database.with_file_name(collector_ipc::IPC_SOCKET_FILE);
    let binary = std::env::var("CARGO_BIN_EXE_nexus-collector").unwrap();
    let mut child = Command::new(binary)
        .args(["--database", database.to_str().unwrap()])
        .env("CODEX_BIN", &fake_codex)
        .env("HOME", database.parent().unwrap())
        .spawn()
        .unwrap();
    wait_for_status(&endpoint);
    let mut switched = false;
    for _ in 0..80 {
        if let Ok(read) = db::open_standalone_readonly(&database) {
            let account_for_source = |source: &str| {
                read.query_row(
                    "SELECT account_key FROM rollout_sources WHERE source_id = ?1 AND binding_status = 'verified'",
                    [source],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            };
            let account_a = account_for_source(&source_a);
            let account_b = account_for_source(&source_b);
            switched = account_a.as_deref() == Some("account:a")
                && account_b.as_deref() == Some("account:b");
            if switched {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    stop(&mut child);
    let read = db::open_standalone_readonly(&database).unwrap();
    let source_a_account: Option<String> = read
        .query_row(
            "SELECT account_key FROM rollout_sources WHERE source_id = ?1 AND binding_status = 'verified'",
            [&source_a],
            |row| row.get(0),
        )
        .ok();
    let source_b_account: Option<String> = read
        .query_row(
            "SELECT account_key FROM rollout_sources WHERE source_id = ?1 AND binding_status = 'verified'",
            [&source_b],
            |row| row.get(0),
        )
        .ok();
    assert!(
        switched,
        "source catch-up reused one account identity: source_a={source_a_account:?}, source_b={source_b_account:?}"
    );
    let _ = fs::remove_file(endpoint);
    let _ = fs::remove_file(&database);
    let _ = fs::remove_file(database.with_extension("collector.lock"));
    let _ = fs::remove_file(source_a_path);
    let _ = fs::remove_file(source_b_path);
    let _ = fs::remove_file(fake_codex);
    let _ = fs::remove_dir_all(database.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn account_updated_notification_immediately_invalidates_and_refreshes_snapshot() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let database = temp_database("account-notification");
    let fake_codex = temp_database("account-notification-codex").with_extension("sh");
    let event_file = database.parent().unwrap().join("account-updated-fired");
    fs::write(
        &fake_codex,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
      case "$line" in
        *'"method":"account/read"'*)
          if [ -e "$EVENT_FILE" ]; then sleep 2; result='{"account":{"id":"b","email":"b@example.com"}}'; else result='{"account":{"id":"a","email":"a@example.com"}}'; fi
          ;;
        *'"method":"account/rateLimits/read"'*) result='{"rateLimitsByLimitId":{}}' ;;
        *'"method":"account/usage/read"'*)
          result='{"dailyUsageBuckets":[],"summary":null}'
          if [ ! -e "$EVENT_FILE.scheduled" ]; then
            : > "$EVENT_FILE.scheduled"
            (sleep 1; : > "$EVENT_FILE"; printf '%s\n' '{"method":"account/updated","params":{"account":{"id":"b"}}}') &
          fi
          ;;
        *) result='{}' ;;
      esac
      printf '{"id":%s,"result":%s}\n' "$id" "$result"
      ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();

    let endpoint = database.with_file_name(collector_ipc::IPC_SOCKET_FILE);
    let binary = std::env::var("CARGO_BIN_EXE_nexus-collector").unwrap();
    let mut child = Command::new(binary)
        .args(["--database", database.to_str().unwrap()])
        .env("CODEX_BIN", &fake_codex)
        .env("EVENT_FILE", &event_file)
        .env("HOME", database.parent().unwrap())
        .spawn()
        .unwrap();
    wait_for_status(&endpoint);

    let mut initial_generation = None;
    for _ in 0..60 {
        let status = collector_ipc::request_path(&endpoint, "GET_STATUS", Value::Null).unwrap();
        let snapshot = collector_ipc::request_path(&endpoint, "GET_SNAPSHOT", Value::Null).unwrap();
        if status["scheduler"]["refreshing"] == false && !snapshot.is_null() {
            initial_generation = status["scheduler"]["refreshGeneration"].as_u64();
            assert_eq!(snapshot["account"]["account"]["id"], "a");
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let initial_generation = initial_generation.expect("initial snapshot never became ready");

    let mut invalidated_immediately = false;
    for _ in 0..40 {
        let status = collector_ipc::request_path(&endpoint, "GET_STATUS", Value::Null).unwrap();
        let snapshot = collector_ipc::request_path(&endpoint, "GET_SNAPSHOT", Value::Null).unwrap();
        invalidated_immediately = status["scheduler"]["refreshing"] == true
            && status["scheduler"]["refreshGeneration"]
                .as_u64()
                .is_some_and(|generation| generation > initial_generation)
            && snapshot.is_null();
        if invalidated_immediately {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        invalidated_immediately,
        "account/updated did not immediately invalidate the old snapshot"
    );

    let mut refreshed_account = None;
    for _ in 0..80 {
        let status = collector_ipc::request_path(&endpoint, "GET_STATUS", Value::Null).unwrap();
        let snapshot = collector_ipc::request_path(&endpoint, "GET_SNAPSHOT", Value::Null).unwrap();
        if status["scheduler"]["refreshing"] == false && !snapshot.is_null() {
            refreshed_account = snapshot["account"]["account"]["id"]
                .as_str()
                .map(str::to_owned);
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(refreshed_account.as_deref(), Some("b"));

    stop(&mut child);
    let _ = fs::remove_file(endpoint);
    let _ = fs::remove_file(&database);
    let _ = fs::remove_file(database.with_extension("collector.lock"));
    let _ = fs::remove_file(fake_codex);
    let _ = fs::remove_file(&event_file);
    let _ = fs::remove_file(event_file.with_extension("scheduled"));
    let _ = fs::remove_dir_all(database.parent().unwrap());
}

#[test]
fn ui_proxy_returns_a_clear_error_when_collector_is_unavailable() {
    let endpoint = temp_database("missing").with_file_name(collector_ipc::IPC_SOCKET_FILE);
    let error = collector_ipc::request_path(&endpoint, "GET_STATUS", Value::Null).unwrap_err();
    assert!(
        error.contains("metadata") || error.contains("unavailable") || error.contains("Named Pipe")
    );
    let _ = std::fs::remove_dir_all(endpoint.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn standalone_collector_partial_rpc_failures_are_fail_closed_independently() {
    use codex_nexus_lib::usage::{collector_core, db, quota, recorder};
    use std::{fs, os::unix::fs::PermissionsExt};

    for failed_method in ["rate", "usage", "account", "signedout"] {
        let database = temp_database(&format!("partial-{failed_method}"));
        let fake_codex =
            temp_database(&format!("partial-codex-{failed_method}")).with_extension("sh");
        let history_dir = database.parent().unwrap().join("Codex Usage Monitor");
        fs::create_dir_all(&history_dir).unwrap();
        let history_path = history_dir.join("usage-history.json");
        let settings_path = history_dir.join("settings.json");
        fs::write(&history_path, b"history-sentinel").unwrap();
        fs::write(&settings_path, b"settings-sentinel").unwrap();
        let before_counts: (i64, i64, i64, i64, i64, i64, i64, i64);
        {
            let connection = db::open_standalone_database(&database).unwrap();
            recorder::ensure_account(&connection, "account:partial", 10).unwrap();
            connection
                .execute(
                    "INSERT INTO account_usage_data_versions
                     (account_key, rollout_parser_version, status, timeline_status,
                      missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
                      parse_error_count, source_incomplete_count, source_lag_seconds, updated_at)
                     VALUES ('account:partial', 9, 'verified', 'complete', 0, 0, 0, 0, 0, 0, 10)",
                    [],
                )
                .unwrap();
            collector_core::record_account_presence(
                &connection,
                "account:partial",
                10,
                "account_read",
                "high",
                None,
            )
            .unwrap();
            connection
                .execute(
                    "INSERT INTO account_daily_usage
                     (account_key, date, official_tokens, fetched_at, source, confidence)
                     VALUES ('account:partial', '2026-08-20', 10, 10, 'official', 'high')",
                    [],
                )
                .unwrap();
            connection
                .execute_batch(
                    "INSERT INTO rate_limit_samples
                     (account_key, sampled_at, limit_id, window, window_duration_mins,
                      used_percent, resets_at, source, confidence)
                     VALUES ('account:partial', 10, 'codex', 'primary', 10080, 10, 99, 'official', 'high'),
                            ('account:partial', 20, 'codex', 'primary', 10080, 20, 99, 'official', 'high'),
                            ('account:partial', 30, 'codex', 'primary', 10080, 30, 99, 'official', 'high');",
                )
                .unwrap();
            quota::refresh_intervals(&connection, "account:partial", "codex", "primary").unwrap();
            before_counts = connection
                .query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM account_daily_usage),
                       (SELECT COUNT(*) FROM rate_limit_samples),
                       (SELECT COUNT(*) FROM quota_intervals),
                       (SELECT COUNT(*) FROM turn_token_samples),
                       (SELECT COUNT(*) FROM thread_usage_group_samples),
                       (SELECT COUNT(*) FROM turn_timeline_audits),
                       (SELECT COUNT(*) FROM rollout_parse_errors),
                       (SELECT COUNT(*) FROM account_usage_data_versions)",
                    [],
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
                        ))
                    },
                )
                .unwrap();
        }
        fs::write(
            &fake_codex,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
      case "$line" in
        *'"method":"account/read"'*)
          if [ "$FAIL_METHOD" = "account" ]; then printf '{"id":%s,"error":{"code":-32009,"message":"account unavailable"}}\n' "$id"; continue; fi
          if [ "$FAIL_METHOD" = "signedout" ]; then result='{"account":null}'; else result='{"account":{"id":"partial"}}'; fi ;;
        *'"method":"account/rateLimits/read"'*)
          if [ "$FAIL_METHOD" = "rate" ]; then printf '{"id":%s,"error":{"code":-32010,"message":"rate limits unavailable"}}\n' "$id"; continue; else result='{"rateLimitsByLimitId":{}}'; fi ;;
        *'"method":"account/usage/read"'*)
          if [ "$FAIL_METHOD" = "usage" ]; then printf '{"id":%s,"error":{"code":-32011,"message":"usage unavailable"}}\n' "$id"; continue; else result='{"dailyUsageBuckets":[],"summary":null}'; fi ;;
        *) result='{}' ;;
      esac
      printf '{"id":%s,"result":%s}\n' "$id" "$result"
      ;;
  esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();

        let endpoint = database.with_file_name(collector_ipc::IPC_SOCKET_FILE);
        let binary = std::env::var("CARGO_BIN_EXE_nexus-collector").unwrap();
        let mut child = Command::new(binary)
            .args(["--database", database.to_str().unwrap()])
            .env("CODEX_BIN", &fake_codex)
            .env("FAIL_METHOD", failed_method)
            .env("HOME", database.parent().unwrap())
            .spawn()
            .unwrap();
        wait_for_status(&endpoint);

        let mut refresh_failed = false;
        for _ in 0..60 {
            if let Ok(status) = collector_ipc::request_path(&endpoint, "GET_STATUS", Value::Null) {
                refresh_failed = status["scheduler"]["refreshing"] == false
                    && if failed_method == "signedout" {
                        status["scheduler"]["refreshGeneration"]
                            .as_u64()
                            .unwrap_or_default()
                            >= 1
                    } else {
                        status["scheduler"]["refreshError"].is_string()
                    };
                if refresh_failed {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            refresh_failed,
            "{failed_method} RPC failure was not surfaced"
        );
        let snapshot = collector_ipc::request_path(&endpoint, "GET_SNAPSHOT", Value::Null).unwrap();
        assert!(
            snapshot.is_null(),
            "{failed_method} RPC failure exposed a renderable snapshot: {snapshot}"
        );
        let read = db::open_standalone_readonly(&database).unwrap();
        let after_counts: (i64, i64, i64, i64, i64, i64, i64, i64) = read
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM account_daily_usage),
                   (SELECT COUNT(*) FROM rate_limit_samples),
                   (SELECT COUNT(*) FROM quota_intervals),
                   (SELECT COUNT(*) FROM turn_token_samples),
                   (SELECT COUNT(*) FROM thread_usage_group_samples),
                   (SELECT COUNT(*) FROM turn_timeline_audits),
                   (SELECT COUNT(*) FROM rollout_parse_errors),
                   (SELECT COUNT(*) FROM account_usage_data_versions)",
                [],
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
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            after_counts, before_counts,
            "{failed_method} changed durable derived data"
        );
        assert_eq!(fs::read(&history_path).unwrap(), b"history-sentinel");
        assert_eq!(fs::read(&settings_path).unwrap(), b"settings-sentinel");
        let current_account = recorder::current_account_key(&read).unwrap();
        if matches!(failed_method, "account" | "signedout") {
            assert_eq!(
                current_account, None,
                "{failed_method} kept the old current account"
            );
            let unresolved: Option<String> = read
                .query_row(
                    "SELECT account_key FROM account_presence_intervals
                     WHERE ended_at IS NULL ORDER BY started_at DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .ok();
            assert!(
                unresolved
                    .as_deref()
                    .is_some_and(|key| key.starts_with("unresolved:official:")),
                "{failed_method} did not establish an identity fence: {unresolved:?}"
            );
        } else {
            assert_eq!(current_account.as_deref(), Some("account:partial"));
        }

        stop(&mut child);
        let _ = fs::remove_file(endpoint);
        let _ = fs::remove_file(&database);
        let _ = fs::remove_file(database.with_extension("collector.lock"));
        let _ = fs::remove_file(fake_codex);
        let _ = fs::remove_dir_all(database.parent().unwrap());
    }
}
