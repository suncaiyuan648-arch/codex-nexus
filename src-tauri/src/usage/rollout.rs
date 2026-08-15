use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use super::models::{Confidence, TokenUsage, TurnUsageRecord, SOURCE_ROLLOUT};
use super::{db, rate_card::RateCard, recorder};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorState {
    session_id: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    speed_mode: Option<String>,
    turn_started_at: Option<i64>,
    turn_start_totals: TokenUsage,
    last_totals: TokenUsage,
}

#[derive(Clone, Debug)]
struct RolloutCursor {
    byte_offset: u64,
    state: CursorState,
}

fn number_i64(value: Option<&Value>) -> Option<i64> {
    value
        .and_then(Value::as_i64)
        .or_else(|| value.and_then(Value::as_u64).map(|value| value as i64))
        .or_else(|| value.and_then(Value::as_f64).map(|value| value as i64))
}

fn string_value(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn parse_timestamp(value: Option<&Value>) -> Option<i64> {
    if let Some(number) = number_i64(value) {
        return Some(if number > 10_000_000_000 {
            number / 1000
        } else {
            number
        });
    }
    let text = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|date| date.timestamp())
}

fn parse_usage(value: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: number_i64(value.get("input_tokens")).unwrap_or_default(),
        cached_input_tokens: number_i64(value.get("cached_input_tokens")).unwrap_or_default(),
        output_tokens: number_i64(value.get("output_tokens")).unwrap_or_default(),
        reasoning_output_tokens: number_i64(value.get("reasoning_output_tokens"))
            .unwrap_or_default(),
        raw_total_tokens: number_i64(value.get("total_tokens")).unwrap_or_default(),
    }
    .normalized()
}

fn subtract(current: &TokenUsage, start: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: (current.input_tokens - start.input_tokens).max(0),
        cached_input_tokens: (current.cached_input_tokens - start.cached_input_tokens).max(0),
        output_tokens: (current.output_tokens - start.output_tokens).max(0),
        reasoning_output_tokens: (current.reasoning_output_tokens - start.reasoning_output_tokens)
            .max(0),
        raw_total_tokens: (current.raw_total_tokens - start.raw_total_tokens).max(0),
    }
    .normalized()
}

fn normalize_effort(value: Option<String>) -> String {
    match value.map(|value| value.to_ascii_lowercase()) {
        Some(value) if ["low", "medium", "high", "xhigh", "ultra"].contains(&value.as_str()) => {
            value
        }
        _ => "unknown".into(),
    }
}

fn normalize_speed(value: Option<String>) -> String {
    match value.map(|value| value.to_ascii_lowercase()).as_deref() {
        Some("standard") | Some("default") => "standard".into(),
        Some("fast") | Some("priority") | Some("fast_requested") => "fast_requested".into(),
        _ => "unknown".into(),
    }
}

fn confidence(model: Option<&str>, effort: &str, speed: &str) -> Confidence {
    if model.is_some() && effort != "unknown" && speed != "unknown" {
        Confidence::High
    } else if model.is_some() {
        Confidence::Medium
    } else {
        Confidence::Low
    }
}

fn upsert_turn(connection: &Connection, turn: &TurnUsageRecord) -> Result<(), String> {
    let now = recorder::now_seconds();
    let confidence = recorder::confidence_string(&turn.confidence);
    connection.execute(
        "INSERT INTO turn_usage
         (account_key, thread_id, turn_id, started_at, completed_at, model,
          reasoning_effort, speed_mode, input_tokens, cached_input_tokens,
          output_tokens, reasoning_output_tokens, raw_total_tokens, estimated_credits,
          rate_card_version, source, confidence, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?18)
         ON CONFLICT(account_key, thread_id, turn_id) DO UPDATE SET
           started_at = excluded.started_at,
           completed_at = excluded.completed_at,
           model = excluded.model,
           reasoning_effort = excluded.reasoning_effort,
           speed_mode = excluded.speed_mode,
           input_tokens = excluded.input_tokens,
           cached_input_tokens = excluded.cached_input_tokens,
           output_tokens = excluded.output_tokens,
           reasoning_output_tokens = excluded.reasoning_output_tokens,
           raw_total_tokens = excluded.raw_total_tokens,
           estimated_credits = excluded.estimated_credits,
           rate_card_version = excluded.rate_card_version,
           source = excluded.source,
           confidence = excluded.confidence,
           updated_at = excluded.updated_at",
        params![
            turn.account_key, turn.thread_id, turn.turn_id, turn.started_at,
            turn.completed_at, turn.model, turn.reasoning_effort, turn.speed_mode,
            turn.usage.input_tokens, turn.usage.cached_input_tokens, turn.usage.output_tokens,
            turn.usage.reasoning_output_tokens, turn.usage.raw_total_tokens,
            turn.estimated_credits, turn.rate_card_version, turn.source, confidence, now,
        ],
    ).map_err(|error| error.to_string())?;
    Ok(())
}

fn process_line(
    connection: &Connection,
    line: &str,
    state: &mut CursorState,
    account_key: &str,
) -> Result<bool, String> {
    let value: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
    let timestamp = parse_timestamp(value.get("timestamp")).unwrap_or_else(recorder::now_seconds);
    match value.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            state.session_id = value
                .get("payload")
                .and_then(|payload| payload.get("session_id").or_else(|| payload.get("id")))
                .and_then(Value::as_str)
                .map(str::to_owned);
            Ok(false)
        }
        Some("turn_context") => {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            state.turn_start_totals = state.last_totals.clone();
            state.turn_id = string_value(payload.get("turn_id").or_else(|| payload.get("turnId")));
            state.thread_id =
                string_value(payload.get("thread_id").or_else(|| payload.get("threadId")))
                    .or_else(|| state.session_id.clone());
            state.model = string_value(payload.get("model"));
            state.reasoning_effort = string_value(payload.get("effort"))
                .or_else(|| string_value(payload.get("reasoning_effort")))
                .map(|value| normalize_effort(Some(value)));
            state.speed_mode = string_value(payload.get("service_tier"))
                .or_else(|| string_value(payload.get("serviceTier")))
                .map(|value| normalize_speed(Some(value)));
            state.turn_started_at = Some(timestamp);
            Ok(false)
        }
        Some("event_msg")
            if value
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                == Some("token_count") =>
        {
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let info = payload.get("info").unwrap_or(&Value::Null);
            let total = info
                .get("total_token_usage")
                .map(parse_usage)
                .unwrap_or_default();
            let last = info.get("last_token_usage").map(parse_usage);
            let usage = if let Some(last) = last.filter(|usage| usage.raw_total_tokens > 0) {
                last
            } else {
                subtract(&total, &state.turn_start_totals)
            };
            let reset_baseline = total.raw_total_tokens < state.last_totals.raw_total_tokens;
            if reset_baseline {
                state.turn_start_totals = TokenUsage::default();
            }
            state.last_totals = total;

            let thread_id = state
                .thread_id
                .clone()
                .or_else(|| state.session_id.clone())
                .unwrap_or_else(|| "unknown-thread".into());
            let turn_id = state
                .turn_id
                .clone()
                .unwrap_or_else(|| format!("unknown:{timestamp}"));
            let speed = state.speed_mode.clone().unwrap_or_else(|| "unknown".into());
            let model = state.model.clone();
            let card = RateCard::current();
            let estimated_credits = card.calculate(model.as_deref(), &speed, &usage);
            let turn = TurnUsageRecord {
                account_key: account_key.into(),
                thread_id,
                turn_id,
                started_at: state.turn_started_at.unwrap_or(timestamp),
                completed_at: Some(timestamp),
                model: model.clone(),
                reasoning_effort: state
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                speed_mode: speed.clone(),
                usage,
                estimated_credits,
                rate_card_version: estimated_credits
                    .map(|_| super::rate_card::CURRENT_RATE_CARD_VERSION.into()),
                source: SOURCE_ROLLOUT.into(),
                confidence: confidence(
                    model.as_deref(),
                    state.reasoning_effort.as_deref().unwrap_or("unknown"),
                    &speed,
                ),
            };
            recorder::ensure_account(connection, account_key, timestamp)?;
            upsert_turn(connection, &turn)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn load_cursor(connection: &Connection, path: &str) -> Result<Option<RolloutCursor>, String> {
    connection
        .query_row(
            "SELECT byte_offset, state_json FROM rollout_cursors WHERE file_path = ?1",
            [path],
            |row| {
                let state_json: String = row.get(1)?;
                Ok(RolloutCursor {
                    byte_offset: row.get::<_, i64>(0)? as u64,
                    state: serde_json::from_str(&state_json).unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn save_cursor(
    connection: &Connection,
    path: &str,
    byte_offset: u64,
    modified_at: i64,
    state: &CursorState,
) -> Result<(), String> {
    let state_json = serde_json::to_string(state).map_err(|error| error.to_string())?;
    connection.execute(
        "INSERT INTO rollout_cursors (file_path, byte_offset, last_modified_at, last_scanned_at, state_json)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(file_path) DO UPDATE SET byte_offset=excluded.byte_offset,
           last_modified_at=excluded.last_modified_at, last_scanned_at=excluded.last_scanned_at,
           state_json=excluded.state_json",
        params![path, byte_offset as i64, modified_at, recorder::now_seconds(), state_json],
    ).map_err(|error| error.to_string())?;
    Ok(())
}

fn collect_one(connection: &Connection, path: &Path, account_key: &str) -> Result<bool, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let path_string = path.to_string_lossy().into_owned();
    let mut cursor = load_cursor(connection, &path_string)?.unwrap_or(RolloutCursor {
        byte_offset: 0,
        state: CursorState::default(),
    });
    if metadata.len() < cursor.byte_offset {
        cursor = RolloutCursor {
            byte_offset: 0,
            state: CursorState::default(),
        };
    }
    if metadata.len() == cursor.byte_offset {
        return Ok(false);
    }

    let mut file = File::open(path).map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(cursor.byte_offset))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut offset = cursor.byte_offset;
    let mut changed = false;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    loop {
        line.clear();
        let line_start = offset;
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if bytes == 0 {
            break;
        }
        offset += bytes as u64;
        if !line.ends_with('\n') {
            offset = line_start;
            break;
        }
        match process_line(
            &transaction,
            line.trim_end(),
            &mut cursor.state,
            account_key,
        ) {
            Ok(line_changed) => changed |= line_changed,
            Err(error) => eprintln!(
                "[Usage] ignored rollout line in {}: {error}",
                path.display()
            ),
        }
    }
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64);
    save_cursor(
        &transaction,
        &path_string,
        offset,
        modified_at.unwrap_or_else(recorder::now_seconds),
        &cursor.state,
    )?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(changed)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn roots() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    [
        home.join(".codex/sessions"),
        home.join(".codex/archived_sessions"),
    ]
    .into_iter()
    .filter(|path| path.is_dir())
    .collect()
}

pub fn rollout_watch_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    let codex_dir = home.join(".codex");
    if codex_dir.is_dir() {
        vec![codex_dir]
    } else {
        Vec::new()
    }
}

fn discover(root: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            discover(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

pub fn collect_rollout_file(
    app: &tauri::AppHandle<tauri::Wry>,
    path: &Path,
) -> Result<bool, String> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return Ok(false);
    }
    let connection = db::open_database(app)?;
    let account_key =
        recorder::current_account_key(&connection)?.unwrap_or_else(|| "local:rollout".into());
    recorder::ensure_account(&connection, &account_key, recorder::now_seconds())?;
    collect_one(&connection, path, &account_key)
}

pub fn collect_rollouts(app: &tauri::AppHandle<tauri::Wry>) -> Result<bool, String> {
    let connection = db::open_database(app)?;
    let account_key =
        recorder::current_account_key(&connection)?.unwrap_or_else(|| "local:rollout".into());
    recorder::ensure_account(&connection, &account_key, recorder::now_seconds())?;
    let mut files = Vec::new();
    for root in roots() {
        discover(&root, &mut files);
    }
    files.sort();
    let mut changed = false;
    for path in files {
        match collect_one(&connection, &path, &account_key) {
            Ok(was_changed) => changed |= was_changed,
            Err(error) => {
                eprintln!(
                    "[Usage] rollout collection failed for {}: {error}",
                    path.display()
                );
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::db::initialize_schema;

    #[test]
    fn cumulative_token_count_is_stored_as_current_turn_total() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let mut state = CursorState::default();
        process_line(&connection, r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#, &mut state, "a").unwrap();
        process_line(&connection, r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#, &mut state, "a").unwrap();
        for total in [100, 135] {
            let line = serde_json::json!({
                "timestamp": "2026-08-14T00:00:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": { "total_token_usage": { "input_tokens": total, "total_tokens": total } }
                }
            }).to_string();
            process_line(&connection, &line, &mut state, "a").unwrap();
        }
        let total: i64 = connection
            .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(total, 135);
    }

    #[test]
    fn last_turn_usage_is_preferred_when_available() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let mut state = CursorState {
            session_id: Some("s".into()),
            thread_id: Some("th".into()),
            turn_id: Some("t".into()),
            model: Some("gpt-5.6-luna".into()),
            reasoning_effort: Some("medium".into()),
            speed_mode: Some("standard".into()),
            ..CursorState::default()
        };
        process_line(&connection, r#"{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100},"last_token_usage":{"input_tokens":7,"total_tokens":7}}}}"#, &mut state, "a").unwrap();
        let total: i64 = connection
            .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(total, 7);
    }
}
