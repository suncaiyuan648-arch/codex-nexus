use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use super::models::{
    is_unresolved_account_key, AccountDataHealth, Confidence, TokenUsage, TurnUsageRecord,
    SOURCE_ROLLOUT,
};
use super::{collector_core::BINDING_VERIFIED, db, rate_card::RateCard, recorder};

pub const ROLLOUT_PARSER_VERSION: i64 = 9;
pub const DATA_HEALTH_VERIFIED: &str = "verified";
pub const DATA_HEALTH_LEGACY_UNVERIFIED: &str = "legacy_unverified";
pub const DATA_HEALTH_ACCOUNTING_INCONSISTENT: &str = "accounting_inconsistent";
pub const DATA_HEALTH_REBUILDING: &str = "rebuilding";
pub const DATA_HEALTH_SOURCE_INCOMPLETE: &str = "source_incomplete";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
struct CursorState {
    parser_version: i64,
    session_id: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    speed_mode: Option<String>,
    turn_started_at: Option<i64>,
    turn_start_totals: TokenUsage,
    segment_no: i64,
    segment_base_usage: TokenUsage,
    turn_segment_start_usage: TokenUsage,
    turn_accumulated_usage: TokenUsage,
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

fn add(left: &TokenUsage, right: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: left.input_tokens.saturating_add(right.input_tokens),
        cached_input_tokens: left
            .cached_input_tokens
            .saturating_add(right.cached_input_tokens),
        output_tokens: left.output_tokens.saturating_add(right.output_tokens),
        reasoning_output_tokens: left
            .reasoning_output_tokens
            .saturating_add(right.reasoning_output_tokens),
        raw_total_tokens: left.raw_total_tokens.saturating_add(right.raw_total_tokens),
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

fn insert_token_sample(
    connection: &Connection,
    account_key: &str,
    thread_id: &str,
    turn_id: &str,
    segment_no: i64,
    model: Option<&str>,
    reasoning_effort: &str,
    speed_mode: &str,
    sampled_at: i64,
    cumulative_tokens: i64,
    delta_tokens: i64,
    confidence: &Confidence,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT OR IGNORE INTO turn_token_samples
             (account_key, thread_id, turn_id, segment_no, model, reasoning_effort, speed_mode,
              sampled_at, cumulative_tokens, delta_tokens, source, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                account_key,
                thread_id,
                turn_id,
                segment_no,
                model,
                reasoning_effort,
                speed_mode,
                sampled_at,
                cumulative_tokens,
                delta_tokens,
                SOURCE_ROLLOUT,
                recorder::confidence_string(confidence),
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[allow(dead_code)]
fn process_line(
    connection: &Connection,
    line: &str,
    state: &mut CursorState,
    account_key: &str,
) -> Result<bool, String> {
    process_line_inner(connection, line, state, account_key, true, None)
}

fn process_line_inner(
    connection: &Connection,
    line: &str,
    state: &mut CursorState,
    account_key: &str,
    persist: bool,
    source_id: Option<&str>,
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
            let next_turn_id =
                string_value(payload.get("turn_id").or_else(|| payload.get("turnId")));
            let next_thread_id =
                string_value(payload.get("thread_id").or_else(|| payload.get("threadId")))
                    .or_else(|| state.session_id.clone());
            // `session_meta` is not guaranteed to precede the first
            // `turn_context`. Token samples already fall back to session_id
            // when thread_id is absent, so same-turn detection must use the
            // same resolved identity. Otherwise a later repeated context
            // resets the canonical total while the earlier timeline deltas
            // remain attached to the session thread.
            let current_thread_id = state.thread_id.clone().or_else(|| state.session_id.clone());
            let same_turn = state.turn_id == next_turn_id && current_thread_id == next_thread_id;
            state.turn_id = next_turn_id;
            state.thread_id = next_thread_id;
            state.model = string_value(payload.get("model"));
            state.reasoning_effort = string_value(payload.get("effort"))
                .or_else(|| string_value(payload.get("reasoning_effort")))
                .map(|value| normalize_effort(Some(value)));
            state.speed_mode = string_value(payload.get("service_tier"))
                .or_else(|| string_value(payload.get("serviceTier")))
                .map(|value| normalize_speed(Some(value)));
            if !same_turn {
                state.turn_start_totals = state.last_totals.clone();
                state.turn_started_at = Some(timestamp);
                state.turn_accumulated_usage = TokenUsage::default();
                state.turn_segment_start_usage = state.last_totals.clone();
            }
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
            let reset_baseline = total.raw_total_tokens < state.last_totals.raw_total_tokens;
            if reset_baseline {
                // `total_token_usage` is a cumulative counter within a
                // generation. A reset starts a new segment, but it does not
                // end the logical turn: the canonical turn usage remains the
                // sum of deltas across all segments.
                state.segment_no = state.segment_no.saturating_add(1);
                state.segment_base_usage = TokenUsage::default();
                state.turn_segment_start_usage = TokenUsage::default();
                state.last_totals = TokenUsage::default();
            }
            // `last_token_usage` is not a reliable delta: Codex can re-emit
            // the previous value when only rate-limit state changes. The
            // cumulative total is the source of truth. If the cumulative
            // total did not grow, keep the previous normalized total so a
            // repeated event cannot create a new usage observation.
            let effective_total = if total.raw_total_tokens > state.last_totals.raw_total_tokens {
                total
            } else {
                state.last_totals.clone()
            };
            let delta_usage = subtract(&effective_total, &state.last_totals);
            let cumulative_usage = subtract(&effective_total, &state.turn_segment_start_usage);
            state.turn_accumulated_usage = add(&state.turn_accumulated_usage, &delta_usage);
            state.last_totals = effective_total;

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
            let reasoning_effort = state
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "unknown".into());
            let card = RateCard::current();
            let estimated_credits =
                card.calculate(model.as_deref(), &speed, &state.turn_accumulated_usage);
            let sample_confidence = confidence(model.as_deref(), &reasoning_effort, &speed);
            let turn = TurnUsageRecord {
                account_key: account_key.into(),
                thread_id,
                turn_id,
                started_at: state.turn_started_at.unwrap_or(timestamp),
                completed_at: Some(timestamp),
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
                speed_mode: speed.clone(),
                usage: state.turn_accumulated_usage.clone(),
                estimated_credits,
                rate_card_version: estimated_credits
                    .map(|_| super::rate_card::CURRENT_RATE_CARD_VERSION.into()),
                source: SOURCE_ROLLOUT.into(),
                confidence: sample_confidence.clone(),
            };
            if persist {
                recorder::ensure_account(connection, account_key, timestamp)?;
                upsert_turn(connection, &turn)?;
                if delta_usage.raw_total_tokens > 0 {
                    insert_token_sample(
                        connection,
                        account_key,
                        &turn.thread_id,
                        &turn.turn_id,
                        state.segment_no,
                        model.as_deref(),
                        &reasoning_effort,
                        &speed,
                        timestamp,
                        cumulative_usage.raw_total_tokens,
                        delta_usage.raw_total_tokens,
                        &sample_confidence,
                    )?;
                }
                if let Some(source_id) = source_id {
                    connection
                        .execute(
                            "INSERT OR IGNORE INTO rollout_turn_sources
                             (source_id, account_key, thread_id, turn_id, first_seen_at)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                source_id,
                                account_key,
                                turn.thread_id,
                                turn.turn_id,
                                timestamp
                            ],
                        )
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(delta_usage.raw_total_tokens > 0)
        }
        _ => Ok(false),
    }
}

fn load_cursor(
    connection: &Connection,
    path: &str,
    source_id: Option<&str>,
) -> Result<Option<RolloutCursor>, String> {
    if let Some(source_id) = source_id {
        return connection
            .query_row(
                "SELECT last_offset, cursor_state_json FROM rollout_sources WHERE source_id = ?1",
                [source_id],
                |row| {
                    let state_json: String = row.get(1)?;
                    Ok(RolloutCursor {
                        byte_offset: row.get::<_, i64>(0)?.max(0) as u64,
                        state: serde_json::from_str(&state_json).unwrap_or_default(),
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string());
    }
    connection
        .query_row(
            "SELECT byte_offset, state_json FROM rollout_cursors WHERE file_path = ?1",
            [path],
            |row| {
                let state_json: String = row.get(1)?;
                Ok(RolloutCursor {
                    byte_offset: row.get::<_, i64>(0)?.max(0) as u64,
                    state: serde_json::from_str(&state_json).unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn fresh_cursor() -> RolloutCursor {
    RolloutCursor {
        byte_offset: 0,
        state: CursorState {
            parser_version: ROLLOUT_PARSER_VERSION,
            ..CursorState::default()
        },
    }
}

fn save_cursor(
    connection: &Connection,
    path: &str,
    byte_offset: u64,
    modified_at: i64,
    state: &CursorState,
    source_id: Option<&str>,
) -> Result<(), String> {
    let state_json = serde_json::to_string(state).map_err(|error| error.to_string())?;
    if let Some(source_id) = source_id {
        let size = fs::metadata(path).map_err(|error| error.to_string())?.len() as i64;
        let fingerprint = super::collector_core::file_fingerprint(Path::new(path))?;
        let prefix_fingerprint =
            super::collector_core::file_prefix_fingerprint(Path::new(path), byte_offset)?;
        connection
            .execute(
                "UPDATE rollout_sources SET last_offset = ?2, last_size = ?3,
                 last_mtime = ?4, cursor_state_json = ?5, file_fingerprint = ?6,
                 cursor_prefix_fingerprint = ?7, parser_version = ?8,
                 updated_at = ?9 WHERE source_id = ?1",
                params![
                    source_id,
                    byte_offset as i64,
                    size,
                    modified_at,
                    state_json,
                    fingerprint,
                    prefix_fingerprint,
                    ROLLOUT_PARSER_VERSION,
                    recorder::now_seconds()
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    // The legacy cursor is only maintained by the cfg(test) compatibility
    // parser and one-time historical migration. Durable production callers
    // always provide source_id and write rollout_sources above.
    if source_id.is_none() {
        connection
            .execute(
                "INSERT INTO rollout_cursors (file_path, byte_offset, last_modified_at, last_scanned_at, state_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(file_path) DO UPDATE SET byte_offset=excluded.byte_offset,
                   last_modified_at=excluded.last_modified_at, last_scanned_at=excluded.last_scanned_at,
                   state_json=excluded.state_json",
                params![path, byte_offset as i64, modified_at, recorder::now_seconds(), state_json],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[allow(dead_code)]
#[cfg(test)]
pub(crate) fn collect_one(
    connection: &Connection,
    path: &Path,
    account_key: &str,
    rebuild_batch_id: &str,
) -> Result<bool, String> {
    collect_one_internal(connection, path, account_key, rebuild_batch_id, None, true)
}

pub(crate) fn collect_one_for_source(
    connection: &Connection,
    path: &Path,
    account_key: &str,
    rebuild_batch_id: &str,
    source_id: Option<&str>,
) -> Result<bool, String> {
    collect_one_internal(
        connection,
        path,
        account_key,
        rebuild_batch_id,
        source_id,
        false,
    )
}

fn collect_one_for_rebuild(
    connection: &Connection,
    path: &Path,
    account_key: &str,
    rebuild_batch_id: &str,
) -> Result<bool, String> {
    // Historical rebuilds parse into a staging account and must not mutate
    // either cursor store. Durable production collection uses source_id.
    collect_one_internal(connection, path, account_key, rebuild_batch_id, None, false)
}

fn collect_one_internal(
    connection: &Connection,
    path: &Path,
    account_key: &str,
    rebuild_batch_id: &str,
    source_id: Option<&str>,
    persist_legacy_cursor: bool,
) -> Result<bool, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let path_string = path.to_string_lossy().into_owned();
    let mut cursor = if source_id.is_some() || persist_legacy_cursor {
        load_cursor(connection, &path_string, source_id)?.unwrap_or_else(fresh_cursor)
    } else {
        fresh_cursor()
    };
    if cursor.state.parser_version != ROLLOUT_PARSER_VERSION {
        cursor = fresh_cursor();
    }
    if metadata.len() < cursor.byte_offset {
        cursor = fresh_cursor();
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
        match process_line_inner(
            &transaction,
            line.trim_end(),
            &mut cursor.state,
            account_key,
            true,
            source_id,
        ) {
            Ok(line_changed) => changed |= line_changed,
            Err(error) => {
                record_parse_error(
                    &transaction,
                    &path_string,
                    line_start,
                    &error,
                    account_key,
                    source_id,
                    rebuild_batch_id,
                )?;
                eprintln!(
                    "[Usage] ignored rollout line in {}: {error}",
                    path.display()
                );
            }
        }
    }
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64);
    if source_id.is_some() || persist_legacy_cursor {
        save_cursor(
            &transaction,
            &path_string,
            offset,
            modified_at.unwrap_or_else(recorder::now_seconds),
            &cursor.state,
            source_id,
        )?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(changed)
}

fn rollout_rebuild_name(account_key: &str) -> String {
    format!("rollout_rebuild_v{ROLLOUT_PARSER_VERSION}:{account_key}")
}

fn rollout_batch_id(account_key: &str, kind: &str) -> String {
    format!(
        "{kind}:v{ROLLOUT_PARSER_VERSION}:{account_key}:{}",
        recorder::now_seconds()
    )
}

fn rollout_rebuild_completed(
    connection: &Connection,
    account_key: &str,
    files: &[PathBuf],
) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT status FROM account_usage_data_versions
             WHERE account_key = ?1
               AND rollout_parser_version = ?2",
            params![account_key, ROLLOUT_PARSER_VERSION],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map(|status| {
            status.is_some_and(|status| {
                status != DATA_HEALTH_REBUILDING
                    && !(status == DATA_HEALTH_LEGACY_UNVERIFIED && !files.is_empty())
            })
        })
        .map_err(|error| error.to_string())
}

fn mark_rollout_rebuild_completed(
    connection: &Connection,
    account_key: &str,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT OR REPLACE INTO usage_migrations (name, completed_at)
             VALUES (?1, ?2)",
            params![rollout_rebuild_name(account_key), recorder::now_seconds()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn account_data_health(
    connection: &Connection,
    account_key: &str,
) -> Result<Option<AccountDataHealth>, String> {
    connection
        .query_row(
            "SELECT account_key, rollout_parser_version, status, timeline_status,
                    missing_timeline_turns, orphan_timeline_samples,
                    mismatched_turns, parse_error_count, last_rebuild_batch_id,
                    source_incomplete_count, source_lag_seconds, verified_at
             FROM account_usage_data_versions
             WHERE account_key = ?1",
            [account_key],
            |row| {
                Ok(AccountDataHealth {
                    account_key: row.get(0)?,
                    data_version: row.get(1)?,
                    status: row.get(2)?,
                    timeline_status: row.get(3)?,
                    missing_timeline_turns: row.get(4)?,
                    orphan_timeline_samples: row.get(5)?,
                    mismatched_turns: row.get(6)?,
                    parse_error_count: row.get(7)?,
                    last_rebuild_batch_id: row.get(8)?,
                    source_incomplete_count: row.get(9)?,
                    source_lag_seconds: row.get(10)?,
                    verified_at: row.get(11)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn account_health_counts(
    connection: &Connection,
    account_key: &str,
) -> Result<(i64, i64, i64, i64, i64, i64), String> {
    let missing = connection
        .query_row(
            "SELECT COUNT(*) FROM (
               SELECT t.thread_id, t.turn_id
               FROM turn_usage t
               LEFT JOIN turn_token_samples s
                 ON s.account_key = t.account_key
                AND s.thread_id = t.thread_id
                AND s.turn_id = t.turn_id
               WHERE t.account_key = ?1 AND t.raw_total_tokens > 0
               GROUP BY t.thread_id, t.turn_id, t.raw_total_tokens
               HAVING COALESCE(SUM(s.delta_tokens), 0) = 0
             )",
            [account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let mismatched = connection
        .query_row(
            "SELECT COUNT(*) FROM (
               SELECT t.thread_id, t.turn_id
               FROM turn_usage t
               LEFT JOIN turn_token_samples s
                 ON s.account_key = t.account_key
                AND s.thread_id = t.thread_id
                AND s.turn_id = t.turn_id
               WHERE t.account_key = ?1
               GROUP BY t.thread_id, t.turn_id, t.raw_total_tokens
               HAVING t.raw_total_tokens < 0
                  OR COALESCE(SUM(s.delta_tokens), 0) < 0
                  OR (COALESCE(SUM(s.delta_tokens), 0) > 0
                      AND t.raw_total_tokens != COALESCE(SUM(s.delta_tokens), 0))
             )",
            [account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let orphan_samples = connection
        .query_row(
            "SELECT COUNT(*)
             FROM turn_token_samples s
             LEFT JOIN turn_usage t
               ON t.account_key = s.account_key
              AND t.thread_id = s.thread_id
              AND t.turn_id = s.turn_id
             WHERE s.account_key = ?1 AND t.id IS NULL",
            [account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let parse_errors = connection
        .query_row(
            "SELECT COUNT(*) FROM rollout_parse_errors
             WHERE account_key = ?1",
            [account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let (source_incomplete, source_lag_seconds) = source_completeness(connection, account_key)?;
    Ok((
        missing,
        orphan_samples,
        mismatched,
        parse_errors,
        source_incomplete,
        source_lag_seconds,
    ))
}

const SOURCE_LAG_TOLERANCE_SECONDS: i64 = 5 * 60;
fn file_modified_at(path: &str) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|value| value.as_secs() as i64)
}

fn source_completeness(connection: &Connection, account_key: &str) -> Result<(i64, i64), String> {
    let mut statement = connection
        .prepare(
            "SELECT canonical_path, binding_status, last_offset
             FROM rollout_sources
             WHERE account_key = ?1 OR binding_status = 'unresolved'",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([account_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);

    let quota_window_start: Option<i64> = connection
        .query_row(
            "SELECT resets_at - window_duration_mins * 60
             FROM rate_limit_samples
             WHERE account_key = ?1 AND resets_at IS NOT NULL
             ORDER BY sampled_at DESC, id DESC LIMIT 1",
            [account_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let mut incomplete_count = 0;
    for (path, status, cursor_offset) in rows {
        let Some(metadata) = fs::metadata(&path).ok() else {
            continue;
        };
        let modified_at = file_modified_at(&path);
        if status == "pending" {
            // Pending ownership cannot become complete merely because the
            // file stopped changing. If it overlaps the active quota window,
            // its Token rows can still affect the estimator for the entire
            // cycle and must remain a hard source-completeness blocker.
            if quota_window_start
                .map(|start| modified_at.is_some_and(|modified| modified >= start))
                .unwrap_or(true)
            {
                incomplete_count += 1;
            }
            continue;
        }
        let file_size = metadata.len();
        if file_size > cursor_offset.max(0) as u64 {
            incomplete_count += 1;
        }
    }

    // A later unchanged quota poll is not evidence of missing local Token.
    // Compare the timeline only with real in-cycle quota consumption
    // transitions, otherwise an idle account becomes source-incomplete after
    // five minutes simply because polling continued.
    let latest_quota: Option<i64> = connection
        .query_row(
            "SELECT MAX(sampled_at) FROM (
               SELECT sampled_at, used_percent, resets_at, window_duration_mins,
                      LAG(used_percent) OVER bucket AS previous_percent,
                      LAG(resets_at) OVER bucket AS previous_resets_at,
                      LAG(window_duration_mins) OVER bucket AS previous_duration
               FROM rate_limit_samples
               WHERE account_key = ?1
               WINDOW bucket AS (
                 PARTITION BY limit_id, window ORDER BY sampled_at, id
               )
             )
             WHERE previous_percent IS NOT NULL
               AND used_percent > previous_percent
               AND window_duration_mins = previous_duration
               AND ((resets_at IS NULL AND previous_resets_at IS NULL)
                    OR (resets_at IS NOT NULL AND previous_resets_at IS NOT NULL
                        AND ABS(resets_at - previous_resets_at) <= 5))",
            [account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let latest_timeline: Option<i64> = connection
        .query_row(
            "SELECT MAX(sampled_at) FROM turn_token_samples WHERE account_key = ?1",
            [account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let source_lag_seconds = match (latest_quota, latest_timeline) {
        (Some(quota), Some(timeline)) if quota > timeline + SOURCE_LAG_TOLERANCE_SECONDS => {
            quota - timeline
        }
        (Some(quota), None) if incomplete_count > 0 => quota,
        _ => 0,
    };
    if source_lag_seconds > 0 {
        incomplete_count += 1;
    }
    Ok((incomplete_count, source_lag_seconds))
}

pub(crate) fn set_account_data_health(
    connection: &Connection,
    account_key: &str,
    requested_status: &str,
    data_version: i64,
) -> Result<AccountDataHealth, String> {
    let (missing, orphan, mismatched, parse_errors, source_incomplete, source_lag_seconds) =
        account_health_counts(connection, account_key)?;
    let last_rebuild_batch_id: Option<String> = connection
        .query_row(
            "SELECT rebuild_batch_id FROM rollout_parse_errors
             WHERE account_key = ?1 AND rebuild_batch_id IS NOT NULL
             ORDER BY first_seen_at DESC LIMIT 1",
            [account_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let has_accounting_error = orphan > 0 || mismatched > 0 || parse_errors > 0;
    let status = match requested_status {
        DATA_HEALTH_REBUILDING => DATA_HEALTH_REBUILDING,
        DATA_HEALTH_VERIFIED if missing == 0 && !has_accounting_error && source_incomplete == 0 => {
            DATA_HEALTH_VERIFIED
        }
        DATA_HEALTH_LEGACY_UNVERIFIED => DATA_HEALTH_LEGACY_UNVERIFIED,
        _ if has_accounting_error || missing > 0 => DATA_HEALTH_ACCOUNTING_INCONSISTENT,
        _ if source_incomplete > 0 => DATA_HEALTH_SOURCE_INCOMPLETE,
        _ => requested_status,
    };
    let timeline_status = if status == DATA_HEALTH_REBUILDING {
        "rebuilding"
    } else if source_incomplete > 0 {
        "source_incomplete"
    } else if parse_errors > 0 {
        "parse_error"
    } else if has_accounting_error {
        "accounting_inconsistent"
    } else if missing > 0 {
        "legacy_pre_timeline"
    } else {
        "complete"
    };
    let now = recorder::now_seconds();
    let verified_at = (status == DATA_HEALTH_VERIFIED).then_some(now);
    connection
        .execute(
            "INSERT INTO account_usage_data_versions
             (account_key, rollout_parser_version, status, timeline_status,
              missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
              parse_error_count, last_rebuild_batch_id, source_incomplete_count,
              source_lag_seconds, verified_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(account_key) DO UPDATE SET
               rollout_parser_version = excluded.rollout_parser_version,
               status = excluded.status,
               timeline_status = excluded.timeline_status,
               missing_timeline_turns = excluded.missing_timeline_turns,
               orphan_timeline_samples = excluded.orphan_timeline_samples,
               mismatched_turns = excluded.mismatched_turns,
               parse_error_count = excluded.parse_error_count,
               last_rebuild_batch_id = excluded.last_rebuild_batch_id,
               source_incomplete_count = excluded.source_incomplete_count,
               source_lag_seconds = excluded.source_lag_seconds,
               verified_at = excluded.verified_at,
               updated_at = excluded.updated_at",
            params![
                account_key,
                data_version,
                status,
                timeline_status,
                missing,
                orphan,
                mismatched,
                parse_errors,
                last_rebuild_batch_id,
                source_incomplete,
                source_lag_seconds,
                verified_at,
                now
            ],
        )
        .map_err(|error| error.to_string())?;
    account_data_health(connection, account_key)?
        .ok_or_else(|| "account data health missing".into())
}

/// Re-evaluate source completeness against the latest quota transition.
/// Stored health is a cache, so quota ingestion and read paths must refresh it
/// even when no rollout file event arrived in between.
pub fn refresh_account_data_health(
    connection: &Connection,
    account_key: &str,
) -> Result<AccountDataHealth, String> {
    let previous = account_data_health(connection, account_key)?;
    let data_version = previous
        .as_ref()
        .map(|health| health.data_version)
        .unwrap_or_default();
    let requested_status = previous
        .as_ref()
        .map(|health| health.status.as_str())
        .map(|status| match status {
            DATA_HEALTH_REBUILDING => DATA_HEALTH_REBUILDING,
            DATA_HEALTH_LEGACY_UNVERIFIED => DATA_HEALTH_LEGACY_UNVERIFIED,
            DATA_HEALTH_ACCOUNTING_INCONSISTENT => DATA_HEALTH_ACCOUNTING_INCONSISTENT,
            _ => DATA_HEALTH_VERIFIED,
        })
        .unwrap_or(DATA_HEALTH_LEGACY_UNVERIFIED);
    set_account_data_health(connection, account_key, requested_status, data_version)
}

pub(crate) fn classify_legacy_accounts(connection: &Connection) -> Result<(), String> {
    audit_timeline_gaps(connection, None, "legacy_pre_timeline")?;
    let mut statement = connection
        .prepare(
            "SELECT account_key, rollout_parser_version, status
             FROM account_usage_data_versions",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (account_key, version, status) in rows {
        if status == DATA_HEALTH_LEGACY_UNVERIFIED {
            set_account_data_health(
                connection,
                &account_key,
                DATA_HEALTH_LEGACY_UNVERIFIED,
                version,
            )?;
        }
    }
    Ok(())
}

fn record_timeline_audit(
    connection: &Connection,
    account_key: &str,
    thread_id: &str,
    turn_id: &str,
    canonical_tokens: i64,
    timeline_tokens: i64,
    reason: &str,
) -> Result<(), String> {
    let now = recorder::now_seconds();
    connection
        .execute(
            "INSERT INTO turn_timeline_audits
             (account_key, thread_id, turn_id, canonical_tokens, timeline_tokens,
              reason, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(account_key, thread_id, turn_id) DO UPDATE SET
               canonical_tokens = excluded.canonical_tokens,
               timeline_tokens = excluded.timeline_tokens,
               reason = CASE
                 WHEN turn_timeline_audits.reason = 'legacy_pre_timeline'
                 THEN turn_timeline_audits.reason
                 ELSE excluded.reason
               END,
               last_seen_at = excluded.last_seen_at",
            params![
                account_key,
                thread_id,
                turn_id,
                canonical_tokens,
                timeline_tokens,
                reason,
                now
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) fn audit_timeline_gaps(
    connection: &Connection,
    account_key: Option<&str>,
    missing_reason: &str,
) -> Result<(), String> {
    connection
        .execute(
            "DELETE FROM turn_timeline_audits
             WHERE (?1 IS NULL OR account_key = ?1)
               AND reason != 'account_unresolved'",
            params![account_key],
        )
        .map_err(|error| error.to_string())?;
    let mut statement = connection
        .prepare(
            "SELECT t.account_key, t.thread_id, t.turn_id, t.raw_total_tokens,
                    COALESCE(SUM(s.delta_tokens), 0)
             FROM turn_usage t
             LEFT JOIN turn_token_samples s
               ON s.account_key = t.account_key
              AND s.thread_id = t.thread_id
              AND s.turn_id = t.turn_id
             WHERE (?1 IS NULL OR t.account_key = ?1)
             GROUP BY t.account_key, t.thread_id, t.turn_id, t.raw_total_tokens
             HAVING t.raw_total_tokens > 0
                AND t.raw_total_tokens != COALESCE(SUM(s.delta_tokens), 0)",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let (account, thread, turn, canonical, timeline) =
            row.map_err(|error| error.to_string())?;
        let reason = if timeline == 0 {
            missing_reason
        } else {
            "accounting_error"
        };
        record_timeline_audit(
            connection, &account, &thread, &turn, canonical, timeline, reason,
        )?;
    }
    drop(statement);

    let mut statement = connection
        .prepare(
            "SELECT s.account_key, s.thread_id, s.turn_id, SUM(s.delta_tokens)
             FROM turn_token_samples s
             LEFT JOIN turn_usage t
               ON t.account_key = s.account_key
              AND t.thread_id = s.thread_id
              AND t.turn_id = s.turn_id
             WHERE (?1 IS NULL OR s.account_key = ?1)
               AND t.id IS NULL
             GROUP BY s.account_key, s.thread_id, s.turn_id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let (account, thread, turn, timeline) = row.map_err(|error| error.to_string())?;
        record_timeline_audit(
            connection,
            &account,
            &thread,
            &turn,
            0,
            timeline,
            "orphan_timeline",
        )?;
    }
    Ok(())
}

fn record_parse_error(
    connection: &Connection,
    file_path: &str,
    byte_offset: u64,
    error: &str,
    account_key: &str,
    source_id: Option<&str>,
    rebuild_batch_id: &str,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO rollout_parse_errors
             (file_path, byte_offset, error, account_key, source_id, rebuild_batch_id, first_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(file_path, byte_offset) DO UPDATE SET
               error = excluded.error,
               account_key = excluded.account_key,
               source_id = excluded.source_id,
               rebuild_batch_id = excluded.rebuild_batch_id",
            params![
                file_path,
                byte_offset as i64,
                error,
                account_key,
                source_id,
                rebuild_batch_id,
                recorder::now_seconds()
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Verify the canonical turn total against the immutable token timeline.
/// This is intentionally an error, rather than a clamped ratio: callers must
/// stop estimation when the accounting ledger is not closed.
pub fn verify_token_accounting(
    connection: &Connection,
    account_key: Option<&str>,
) -> Result<(), String> {
    let mismatch: Option<(String, String, i64, i64)> = connection
        .query_row(
            "SELECT t.thread_id, t.turn_id, t.raw_total_tokens,
                    COALESCE(SUM(s.delta_tokens), 0)
             FROM turn_usage t
             LEFT JOIN turn_token_samples s
               ON s.account_key = t.account_key
              AND s.thread_id = t.thread_id
              AND s.turn_id = t.turn_id
             WHERE (?1 IS NULL OR t.account_key = ?1)
             GROUP BY t.account_key, t.thread_id, t.turn_id, t.raw_total_tokens
             HAVING t.raw_total_tokens < 0
                 OR COALESCE(SUM(s.delta_tokens), 0) < 0
                 OR t.raw_total_tokens != COALESCE(SUM(s.delta_tokens), 0)
             LIMIT 1",
            params![account_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((thread_id, turn_id, canonical, timeline)) = mismatch {
        return Err(format!(
            "token accounting inconsistent for turn {thread_id}/{turn_id}: canonical={canonical}, timeline={timeline}"
        ));
    }
    let orphan: Option<(String, String, i64)> = connection
        .query_row(
            "SELECT s.thread_id, s.turn_id, SUM(s.delta_tokens)
             FROM turn_token_samples s
             LEFT JOIN turn_usage t
               ON t.account_key = s.account_key
              AND t.thread_id = s.thread_id
              AND t.turn_id = s.turn_id
             WHERE (?1 IS NULL OR s.account_key = ?1)
               AND t.id IS NULL
             GROUP BY s.account_key, s.thread_id, s.turn_id
             LIMIT 1",
            params![account_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((thread_id, turn_id, timeline)) = orphan {
        return Err(format!(
            "orphan token timeline for turn {thread_id}/{turn_id}: canonical=missing, timeline={timeline}"
        ));
    }
    Ok(())
}

fn clear_rollout_derived_data(connection: &Connection, account_key: &str) -> Result<(), String> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_token_samples WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_usage WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_timeline_audits WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM rollout_parse_errors WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn replace_account_from_staging(
    connection: &Connection,
    account_key: &str,
    staging_key: &str,
) -> Result<(), String> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_token_samples WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_usage WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM turn_timeline_audits WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM rollout_parse_errors WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE turn_usage SET account_key = ?1 WHERE account_key = ?2",
            params![account_key, staging_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE turn_token_samples SET account_key = ?1 WHERE account_key = ?2",
            params![account_key, staging_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE rollout_parse_errors SET account_key = ?1 WHERE account_key = ?2",
            params![account_key, staging_key],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute("DELETE FROM accounts WHERE account_key = ?1", [staging_key])
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn clear_staging_health_records(connection: &Connection) -> Result<(), String> {
    for table in [
        "turn_token_samples",
        "turn_usage",
        "rollout_parse_errors",
        "turn_timeline_audits",
        "account_usage_data_versions",
        "accounts",
    ] {
        connection
            .execute(
                &format!("DELETE FROM {table} WHERE account_key LIKE '__rollout_rebuild_v%'"),
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
fn advance_cursor_without_persisting(
    connection: &Connection,
    path: &str,
    account_key: &str,
) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let mut cursor = load_cursor(connection, path, None)?.unwrap_or_else(fresh_cursor);
    if metadata.len() < cursor.byte_offset {
        cursor = fresh_cursor();
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(cursor.byte_offset))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut offset = cursor.byte_offset;
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
        if process_line_inner(
            connection,
            line.trim_end(),
            &mut cursor.state,
            account_key,
            false,
            None,
        )
        .is_err()
        {
            cursor.state = fresh_cursor().state;
        }
    }
    cursor.state.parser_version = ROLLOUT_PARSER_VERSION;
    save_cursor(
        connection,
        path,
        offset,
        metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_secs() as i64)
            .unwrap_or_else(recorder::now_seconds),
        &cursor.state,
        None,
    )
}

pub(crate) fn quarantine_unverified_account_data(
    connection: &Connection,
    account_key: &str,
) -> Result<(), String> {
    let now = recorder::now_seconds();
    connection
        .execute(
            "INSERT INTO turn_timeline_audits
             (account_key, thread_id, turn_id, canonical_tokens, timeline_tokens,
              reason, first_seen_at, last_seen_at)
             SELECT t.account_key, t.thread_id, t.turn_id, t.raw_total_tokens,
                    COALESCE(SUM(s.delta_tokens), 0), 'account_unresolved', ?2, ?2
             FROM turn_usage t
             LEFT JOIN turn_token_samples s
               ON s.account_key = t.account_key
              AND s.thread_id = t.thread_id
              AND s.turn_id = t.turn_id
             WHERE t.account_key = ?1
             GROUP BY t.account_key, t.thread_id, t.turn_id, t.raw_total_tokens
             ON CONFLICT(account_key, thread_id, turn_id) DO UPDATE SET
               canonical_tokens = excluded.canonical_tokens,
               timeline_tokens = excluded.timeline_tokens,
               reason = 'account_unresolved',
               last_seen_at = excluded.last_seen_at",
            params![account_key, now],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM turn_token_samples WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM turn_usage WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM quota_intervals WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "DELETE FROM rollout_parse_errors WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;
    for table in [
        "account_daily_usage",
        "thread_usage_group_samples",
        "thread_usage_capabilities",
        "rate_limit_samples",
        "account_presence_intervals",
        "rollout_turn_sources",
    ] {
        connection
            .execute(
                &format!("DELETE FROM {table} WHERE account_key = ?1"),
                [account_key],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn record_historical_reset_quarantine(
    connection: &Connection,
    source_id: &str,
    old_account_key: Option<&str>,
    old_status: &str,
    now: i64,
) -> Result<(), String> {
    if old_status == BINDING_VERIFIED {
        return Ok(());
    }
    connection
        .execute(
            "INSERT INTO source_binding_audits
             (source_id, old_account_key, new_account_key, old_status, new_status,
              reason, evidence, changed_at)
             SELECT ?1, ?2, NULL, ?3, 'quarantined', 'historical_reset_quarantine',
                    'legacy/non-verified source was rebased to its durable tail', ?4
             WHERE NOT EXISTS (
               SELECT 1 FROM source_binding_audits
               WHERE source_id = ?1 AND reason = 'historical_reset_quarantine'
             )",
            params![source_id, old_account_key, old_status, now],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn advance_source_cursor_without_persisting_in_transaction(
    connection: &Connection,
    source_id: &str,
    path: &Path,
    account_key: &str,
) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    let mut cursor = load_cursor(connection, &path.to_string_lossy(), Some(source_id))?
        .unwrap_or_else(fresh_cursor);
    if metadata.len() < cursor.byte_offset {
        cursor = fresh_cursor();
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(cursor.byte_offset))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut offset = cursor.byte_offset;
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
        if let Err(error) = process_line_inner(
            connection,
            line.trim_end(),
            &mut cursor.state,
            account_key,
            false,
            None,
        ) {
            eprintln!(
                "[Usage] skipped malformed historical rollout row in {}: {error}",
                path.display()
            );
            cursor.state = fresh_cursor().state;
        }
    }
    cursor.state.parser_version = ROLLOUT_PARSER_VERSION;
    cursor.state.turn_start_totals = cursor.state.last_totals.clone();
    cursor.state.turn_segment_start_usage = cursor.state.last_totals.clone();
    cursor.state.turn_accumulated_usage = TokenUsage::default();
    cursor.state.turn_started_at = Some(recorder::now_seconds());
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64)
        .unwrap_or_else(recorder::now_seconds);
    save_cursor(
        connection,
        &path.to_string_lossy(),
        offset,
        modified_at,
        &cursor.state,
        Some(source_id),
    )?;
    connection
        .execute(
            "UPDATE rollout_sources SET generation = generation + 1,
             health_status = 'historical_reset', last_error = NULL,
             last_error_at = NULL, updated_at = ?2 WHERE source_id = ?1",
            params![source_id, recorder::now_seconds()],
        )
        .map_err(|error| error.to_string())?;
    let old_binding: (Option<String>, String) = connection
        .query_row(
            "SELECT account_key, binding_status FROM rollout_sources WHERE source_id = ?1",
            [source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    record_historical_reset_quarantine(
        connection,
        source_id,
        old_binding.0.as_deref(),
        &old_binding.1,
        recorder::now_seconds(),
    )?;
    Ok(())
}

pub(crate) fn advance_source_cursor_without_persisting(
    connection: &Connection,
    source_id: &str,
    path: &Path,
    account_key: &str,
) -> Result<(), String> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    advance_source_cursor_without_persisting_in_transaction(
        &transaction,
        source_id,
        path,
        account_key,
    )?;
    transaction.commit().map_err(|error| error.to_string())
}

/// Parser v9 deliberately starts a new account ledger instead of trying to
/// infer ownership for historical files that predate durable bindings. Raw
/// JSONL and quota observations are retained, while rollout-derived rows are
/// cleared and every existing cursor is rebased to EOF. Unowned files remain
/// quarantined, but an explicit later watcher event may bind their already
/// rebased tail. Future bytes therefore start at zero without replaying dirty
/// history or permanently losing a rollout that is still growing.
fn reset_historical_rollout_data(connection: &Connection, account_key: &str) -> Result<(), String> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    // A legacy binding may have been written after the original schema open
    // (and older fixtures can model that state). Import it into the durable
    // registry before selecting reset targets; production reset decisions
    // remain source-owned after this one-time compatibility step.
    db::migrate_legacy_rollout_sources(&transaction)?;
    quarantine_unverified_account_data(&transaction, account_key)?;
    transaction
        .execute(
            "DELETE FROM turn_timeline_audits WHERE account_key = ?1",
            [account_key],
        )
        .map_err(|error| error.to_string())?;

    // Remove rollout-derived history for all already-unverified accounts too.
    // These rows cannot be reconciled to a trustworthy timeline and keeping
    // them would leave the database-wide accounting invariant broken.
    let mut legacy_statement = transaction
        .prepare(
            "SELECT account_key FROM account_usage_data_versions
             WHERE account_key != ?1 AND status = 'legacy_unverified'",
        )
        .map_err(|error| error.to_string())?;
    let legacy_accounts = legacy_statement
        .query_map([account_key], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(legacy_statement);
    for legacy_account in legacy_accounts {
        quarantine_unverified_account_data(&transaction, &legacy_account)?;
        transaction
            .execute(
                "DELETE FROM turn_timeline_audits WHERE account_key = ?1",
                [&legacy_account],
            )
            .map_err(|error| error.to_string())?;
        set_account_data_health(
            &transaction,
            &legacy_account,
            DATA_HEALTH_LEGACY_UNVERIFIED,
            0,
        )?;
    }

    let mut statement = transaction
        .prepare(
            "SELECT source_id, canonical_path, binding_status, account_key
             FROM rollout_sources
             WHERE (binding_status = 'verified' AND account_key = ?1)
                OR binding_status IN ('unresolved', 'quarantined')",
        )
        .map_err(|error| error.to_string())?;
    let sources = statement
        .query_map([account_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);

    for (source_id, path, binding_status, source_account) in sources {
        transaction
            .execute(
                "DELETE FROM rollout_turn_sources WHERE source_id = ?1",
                [&source_id],
            )
            .map_err(|error| error.to_string())?;
        let source_path = Path::new(&path);
        if !source_path.is_file() {
            transaction
                .execute(
                    "UPDATE rollout_sources SET account_key = NULL,
                     binding_status = 'quarantined', binding_source = 'historical_reset',
                     binding_confidence = 'unknown', last_offset = 0, last_size = 0,
                     last_mtime = NULL, cursor_state_json = '{}',
                     file_fingerprint = NULL, cursor_prefix_fingerprint = NULL,
                     generation = generation + 1, health_status = 'historical_reset',
                     updated_at = ?2 WHERE source_id = ?1",
                    params![source_id, recorder::now_seconds()],
                )
                .map_err(|error| error.to_string())?;
            record_historical_reset_quarantine(
                &transaction,
                &source_id,
                source_account.as_deref(),
                &binding_status,
                recorder::now_seconds(),
            )?;
            continue;
        }
        let ledger_account = source_account
            .clone()
            .or_else(|| Some(format!("unresolved:source:{source_id}")))
            .unwrap_or_else(|| account_key.to_owned());
        advance_source_cursor_without_persisting_in_transaction(
            &transaction,
            &source_id,
            source_path,
            &ledger_account,
        )?;
        if binding_status != BINDING_VERIFIED {
            transaction
                .execute(
                    "UPDATE rollout_sources SET account_key = NULL,
                     binding_status = 'quarantined', binding_source = 'historical_reset',
                     binding_confidence = 'unknown' WHERE source_id = ?1",
                    [&source_id],
                )
                .map_err(|error| error.to_string())?;
            record_historical_reset_quarantine(
                &transaction,
                &source_id,
                source_account.as_deref(),
                &binding_status,
                recorder::now_seconds(),
            )?;
        }
    }

    set_account_data_health(
        &transaction,
        account_key,
        DATA_HEALTH_VERIFIED,
        ROLLOUT_PARSER_VERSION,
    )?;
    mark_rollout_rebuild_completed(&transaction, account_key)?;
    transaction.commit().map_err(|error| error.to_string())
}

fn rebuild_rollout_derived_data(
    connection: &Connection,
    files: &[PathBuf],
    account_key: &str,
) -> Result<bool, String> {
    clear_staging_health_records(connection)?;
    let prior_health = account_data_health(connection, account_key)?;
    if prior_health.as_ref().is_some_and(|health| {
        health.data_version < ROLLOUT_PARSER_VERSION
            && (health.data_version > 0 || health.status != DATA_HEALTH_LEGACY_UNVERIFIED)
    }) {
        reset_historical_rollout_data(connection, account_key)?;
        return Ok(false);
    }
    if rollout_rebuild_completed(connection, account_key, files)? {
        return Ok(false);
    }
    audit_timeline_gaps(connection, Some(account_key), "legacy_pre_timeline")?;
    let existing_turns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM turn_usage WHERE account_key = ?1",
            [account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if files.is_empty() {
        set_account_data_health(
            connection,
            account_key,
            DATA_HEALTH_LEGACY_UNVERIFIED,
            ROLLOUT_PARSER_VERSION,
        )?;
        return Ok(false);
    }

    set_account_data_health(
        connection,
        account_key,
        DATA_HEALTH_REBUILDING,
        ROLLOUT_PARSER_VERSION,
    )?;
    let staging_key = format!("__rollout_rebuild_v{ROLLOUT_PARSER_VERSION}__:{account_key}");
    let rebuild_batch_id = rollout_batch_id(account_key, "rebuild");
    clear_rollout_derived_data(connection, &staging_key)?;
    for path in files {
        if let Err(error) =
            collect_one_for_rebuild(connection, path, &staging_key, &rebuild_batch_id)
        {
            clear_rollout_derived_data(connection, &staging_key)?;
            set_account_data_health(connection, account_key, DATA_HEALTH_LEGACY_UNVERIFIED, 0)?;
            return Err(error);
        }
    }
    let staged_parse_errors: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM rollout_parse_errors WHERE account_key = ?1",
            [&staging_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if staged_parse_errors > 0 {
        connection
            .execute(
                "UPDATE rollout_parse_errors SET account_key = ?1
                 WHERE account_key = ?2",
                params![account_key, staging_key],
            )
            .map_err(|error| error.to_string())?;
        clear_rollout_derived_data(connection, &staging_key)?;
        set_account_data_health(
            connection,
            account_key,
            DATA_HEALTH_ACCOUNTING_INCONSISTENT,
            ROLLOUT_PARSER_VERSION,
        )?;
        return Ok(false);
    }
    let staged_turns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM turn_usage WHERE account_key = ?1",
            [&staging_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if existing_turns > 0 && staged_turns == 0 {
        clear_rollout_derived_data(connection, &staging_key)?;
        set_account_data_health(
            connection,
            account_key,
            DATA_HEALTH_LEGACY_UNVERIFIED,
            ROLLOUT_PARSER_VERSION,
        )?;
        return Ok(false);
    }
    if let Err(error) = verify_token_accounting(connection, Some(&staging_key)) {
        clear_rollout_derived_data(connection, &staging_key)?;
        set_account_data_health(connection, account_key, DATA_HEALTH_LEGACY_UNVERIFIED, 0)?;
        return Err(error);
    }
    replace_account_from_staging(connection, account_key, &staging_key)?;
    super::quota::rebuild_account_intervals(connection, account_key)?;
    audit_timeline_gaps(connection, Some(account_key), "source_missing")?;
    verify_token_accounting(connection, Some(account_key))?;
    set_account_data_health(
        connection,
        account_key,
        DATA_HEALTH_VERIFIED,
        ROLLOUT_PARSER_VERSION,
    )?;
    mark_rollout_rebuild_completed(connection, account_key)?;
    Ok(true)
}

fn refresh_account_health_after_collection(
    connection: &Connection,
    account_key: &str,
) -> Result<(), String> {
    audit_timeline_gaps(connection, Some(account_key), "source_missing")?;
    let previous = account_data_health(connection, account_key)?;
    let previous_version = previous
        .as_ref()
        .map(|health| health.data_version)
        .unwrap_or_default();
    match verify_token_accounting(connection, Some(account_key)) {
        Ok(())
            if previous.as_ref().is_some_and(|health| {
                matches!(
                    health.status.as_str(),
                    DATA_HEALTH_VERIFIED | DATA_HEALTH_SOURCE_INCOMPLETE
                ) && health.data_version == ROLLOUT_PARSER_VERSION
            }) =>
        {
            set_account_data_health(
                connection,
                account_key,
                DATA_HEALTH_VERIFIED,
                ROLLOUT_PARSER_VERSION,
            )?;
        }
        Ok(()) => {
            let requested_status = previous
                .as_ref()
                .map(|health| health.status.as_str())
                .filter(|status| *status == DATA_HEALTH_ACCOUNTING_INCONSISTENT)
                .unwrap_or(DATA_HEALTH_LEGACY_UNVERIFIED);
            set_account_data_health(connection, account_key, requested_status, previous_version)?;
        }
        Err(_) => {
            set_account_data_health(
                connection,
                account_key,
                DATA_HEALTH_ACCOUNTING_INCONSISTENT,
                previous_version,
            )?;
        }
    }
    Ok(())
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

#[cfg(test)]
fn bind_or_quarantine_rollout_file(
    connection: &Connection,
    path: &Path,
    account_key: &str,
    allow_new_binding: bool,
) -> Result<bool, String> {
    let path_string = path.to_string_lossy().into_owned();
    let existing: Option<(Option<String>, String, Option<String>)> = connection
        .query_row(
            "SELECT account_key, status, reason FROM rollout_file_bindings
             WHERE file_path = ?1",
            [&path_string],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let now = recorder::now_seconds();
    let modified_at = file_modified_at(&path_string);
    match existing {
        Some((Some(bound_account), status, _)) if status == "bound" => {
            connection
                .execute(
                    "UPDATE rollout_file_bindings
                     SET last_seen_at = ?2, last_modified_at = ?3
                     WHERE file_path = ?1",
                    params![path_string, now, modified_at],
                )
                .map_err(|error| error.to_string())?;
            Ok(bound_account == account_key)
        }
        Some((None, status, reason)) => {
            let cursor_offset: Option<i64> = connection
                .query_row(
                    "SELECT byte_offset FROM rollout_cursors WHERE file_path = ?1",
                    [&path_string],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            let has_cursor = cursor_offset.is_some();
            let reset_quarantine = status == "quarantined"
                && matches!(
                    reason.as_deref(),
                    Some("historical_reset_v6")
                        | Some("historical_reset_v7")
                        | Some("historical_reset_v8")
                );
            let account_reset_reason = format!("historical_reset_v9:{account_key}");
            let account_reset_quarantine =
                status == "quarantined" && reason.as_deref() == Some(&account_reset_reason);
            let grew_after_reset = account_reset_quarantine
                && cursor_offset.is_some_and(|offset| {
                    fs::metadata(&path_string)
                        .ok()
                        .is_some_and(|metadata| metadata.len() > offset.max(0) as u64)
                });
            let returned_after_reset = account_reset_quarantine
                && cursor_offset.is_none()
                && Path::new(&path_string).is_file();
            if (allow_new_binding
                && (status == "pending" || reset_quarantine || account_reset_quarantine))
                || grew_after_reset
                || returned_after_reset
            {
                // Databases that already ran the flawed v6 reset can contain
                // an active quarantined file with no cursor. Rebase it at the
                // first later watcher observation; this intentionally drops
                // all bytes through that recovery point, then resumes safely.
                if (reset_quarantine || account_reset_quarantine) && !has_cursor {
                    advance_cursor_without_persisting(connection, &path_string, account_key)?;
                }
                connection
                    .execute(
                        "UPDATE rollout_file_bindings
                         SET account_key = ?2, status = 'bound',
                             reason = CASE WHEN ?6
                                      THEN 'watcher_tail_after_historical_reset'
                                      WHEN ?5 THEN 'watcher_tail_after_legacy_cursor'
                                      ELSE 'first_watcher_observation' END,
                             last_seen_at = ?3, last_modified_at = ?4
                         WHERE file_path = ?1",
                        params![
                            path_string,
                            account_key,
                            now,
                            modified_at,
                            has_cursor,
                            reset_quarantine || account_reset_quarantine
                        ],
                    )
                    .map_err(|error| error.to_string())?;
                Ok(true)
            } else {
                connection
                    .execute(
                        "UPDATE rollout_file_bindings
                         SET last_seen_at = ?2, last_modified_at = ?3
                         WHERE file_path = ?1",
                        params![path_string, now, modified_at],
                    )
                    .map_err(|error| error.to_string())?;
                Ok(false)
            }
        }
        Some(_) => Ok(false),
        None => {
            // A file is bound only at its first explicit collection event.
            // JSONL has no stable account identity, so an already-cursored
            // file is never silently rebound after an account switch.
            let has_cursor: bool = connection
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM rollout_cursors WHERE file_path = ?1
                     )",
                    [&path_string],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if has_cursor {
                connection
                    .execute(
                        "INSERT INTO rollout_file_bindings
                         (file_path, account_key, status, reason, first_seen_at, last_seen_at,
                          last_modified_at)
                         VALUES (?1, NULL, 'pending', 'unattributed_legacy_file', ?2, ?2, ?3)",
                        params![path_string, now, modified_at],
                    )
                    .map_err(|error| error.to_string())?;
                return Ok(false);
            }
            if !allow_new_binding {
                connection
                    .execute(
                        "INSERT INTO rollout_file_bindings
                         (file_path, account_key, status, reason, first_seen_at, last_seen_at,
                          last_modified_at)
                         VALUES (?1, NULL, 'pending', 'awaiting_watcher_binding', ?2, ?2, ?3)",
                        params![path_string, now, modified_at],
                    )
                    .map_err(|error| error.to_string())?;
                return Ok(false);
            }
            connection
                .execute(
                    "INSERT INTO rollout_file_bindings
                     (file_path, account_key, status, reason, first_seen_at, last_seen_at,
                      last_modified_at)
                     VALUES (?1, ?2, 'bound', 'first_observed_for_account', ?3, ?3, ?4)",
                    params![path_string, account_key, now, modified_at],
                )
                .map_err(|error| error.to_string())?;
            Ok(true)
        }
    }
}

fn rollout_file_belongs_to_account(
    connection: &Connection,
    path: &Path,
    account_key: &str,
) -> Result<bool, String> {
    let source_id =
        super::collector_core::register_source(connection, path, recorder::now_seconds())?;
    connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM rollout_sources
               WHERE source_id = ?1 AND account_key = ?2 AND binding_status = 'verified'
             )",
            params![source_id, account_key],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn discover_rollout_files_for_account(
    connection: &Connection,
    extra: Option<&Path>,
    account_key: &str,
) -> Result<Vec<PathBuf>, String> {
    let mut discovered = Vec::new();
    for root in roots() {
        discover(&root, &mut discovered);
    }
    if let Some(path) = extra.filter(|path| path.is_file()) {
        discovered.push(path.to_path_buf());
    }
    discovered.extend(
        super::collector_core::registered_source_paths(connection)?
            .into_iter()
            .filter(|path| path.is_file()),
    );
    discovered.sort();
    discovered.dedup();

    let mut files = Vec::new();
    for path in discovered {
        if rollout_file_belongs_to_account(connection, &path, account_key)? {
            files.push(path);
        }
    }
    Ok(files)
}

#[allow(dead_code)]
fn discover_rollout_files(extra: Option<&Path>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots() {
        discover(&root, &mut files);
    }
    if let Some(path) = extra.filter(|path| path.is_file()) {
        files.push(path.to_path_buf());
    }
    files.sort();
    files.dedup();
    files
}

/// Standalone collector discovery. This deliberately returns files without
/// assigning ownership; binding decisions remain in `collector_core`.
pub fn discover_rollout_files_for_collector() -> Vec<PathBuf> {
    discover_rollout_files(None)
}

pub fn collect_rollout_file(
    app: &tauri::AppHandle<tauri::Wry>,
    path: &Path,
) -> Result<bool, String> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
        return Ok(false);
    }
    let connection = db::open_database_for_collector(app)?;
    let observed_account = recorder::current_account_key(&connection)?;
    let source_id =
        super::collector_core::register_source(&connection, path, recorder::now_seconds())?;
    let collector_changed = super::collector_core::catch_up_path(
        &connection,
        path,
        observed_account.as_deref(),
        true,
        recorder::now_seconds(),
    )?;
    if observed_account.is_none() {
        return Ok(collector_changed);
    }
    let account_key = observed_account
        .clone()
        .expect("observed account checked above");
    let durable_binding: Option<(Option<String>, String)> = connection
        .query_row(
            "SELECT account_key, binding_status FROM rollout_sources WHERE source_id = ?1",
            [&source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if durable_binding
        .as_ref()
        .is_some_and(|(bound_account, status)| {
            status == super::collector_core::BINDING_VERIFIED
                && bound_account.as_deref() != observed_account.as_deref()
        })
    {
        // The compatibility binding table must never turn a verified source
        // into a different account merely because the active account changed.
        return Ok(collector_changed);
    }
    if durable_binding
        .as_ref()
        .is_some_and(|(_, status)| status == super::collector_core::BINDING_UNRESOLVED)
    {
        // An unresolved durable source is owned by the collector core. Do not
        // let the legacy compatibility path infer ownership from a watcher
        // event when the source had no new bytes/evidence.
        return Ok(collector_changed);
    }
    recorder::ensure_account(&connection, &account_key, recorder::now_seconds())?;
    classify_legacy_accounts(&connection)?;
    let files = discover_rollout_files_for_account(&connection, Some(path), &account_key)?;
    if !rollout_file_belongs_to_account(&connection, path, &account_key)? {
        return Ok(false);
    }
    let _ = rebuild_rollout_derived_data(&connection, &files, &account_key)?;
    if collector_changed {
        super::quota::rebuild_account_intervals(&connection, &account_key)?;
    }
    refresh_account_health_after_collection(&connection, &account_key)?;
    Ok(collector_changed)
}

pub fn collect_rollouts(app: &tauri::AppHandle<tauri::Wry>) -> Result<bool, String> {
    let connection = db::open_database_for_collector(app)?;
    let observed_account = recorder::current_account_key(&connection)?;
    classify_legacy_accounts(&connection)?;
    let mut collector_changed = false;
    let mut startup_paths = discover_rollout_files(None);
    for path in super::collector_core::registered_source_paths(&connection)? {
        if path.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        {
            startup_paths.push(path);
        }
    }
    startup_paths.sort();
    startup_paths.dedup();
    for path in startup_paths {
        let active_account = recorder::current_account_key(&connection)?;
        match super::collector_core::catch_up_path(
            &connection,
            &path,
            active_account.as_deref(),
            false,
            recorder::now_seconds(),
        ) {
            Ok(was_changed) => collector_changed |= was_changed,
            Err(error) => eprintln!(
                "[Usage] durable source catch-up failed for {}: {error}",
                path.display()
            ),
        }
    }
    let Some(account_key) = observed_account else {
        // The source cursor and unresolved ledger are still durable, but no
        // account-scoped derived data may be created without live identity.
        return Ok(collector_changed);
    };
    recorder::ensure_account(&connection, &account_key, recorder::now_seconds())?;
    let files = discover_rollout_files_for_account(&connection, None, &account_key)?;
    let changed =
        collector_changed || rebuild_rollout_derived_data(&connection, &files, &account_key)?;
    if changed {
        super::quota::rebuild_account_intervals(&connection, &account_key)?;
    }
    refresh_account_health_after_collection(&connection, &account_key)?;
    Ok(changed)
}

/// Rebuild one account through the collector writer boundary. The UI may
/// request this through IPC, but it never receives a writable SQLite handle.
pub fn rebuild_account(
    app: &tauri::AppHandle<tauri::Wry>,
    account_key: &str,
) -> Result<(), String> {
    if is_unresolved_account_key(account_key) {
        return Err("unresolved sources cannot be rebuilt as an account".into());
    }
    let connection = db::open_database_for_collector(app)?;
    rebuild_account_connection(&connection, account_key)
}

pub fn rebuild_account_connection(
    connection: &Connection,
    account_key: &str,
) -> Result<(), String> {
    if is_unresolved_account_key(account_key) {
        return Err("unresolved sources cannot be rebuilt as an account".into());
    }
    let files = discover_rollout_files_for_account(&connection, None, account_key)?;
    let _ = rebuild_rollout_derived_data(&connection, &files, account_key)?;
    super::quota::rebuild_account_intervals(&connection, account_key)?;
    refresh_account_health_after_collection(&connection, account_key)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::collector_core;
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
        let samples: Vec<(i64, i64)> = connection
            .prepare(
                "SELECT cumulative_tokens, delta_tokens
                 FROM turn_token_samples ORDER BY cumulative_tokens",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(samples, vec![(100, 100), (135, 35)]);
    }

    #[test]
    fn reset_starts_a_new_segment_without_losing_the_previous_turn_tokens() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let mut state = CursorState::default();
        process_line(
            &connection,
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            &mut state,
            "a",
        )
        .unwrap();
        process_line(
            &connection,
            r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#,
            &mut state,
            "a",
        )
        .unwrap();
        for (timestamp, total) in [("2026-08-14T00:00:02Z", 274), ("2026-08-14T00:00:03Z", 252)] {
            let line = serde_json::json!({
                "timestamp": timestamp,
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": { "total_token_usage": { "input_tokens": total, "total_tokens": total } }
                }
            })
            .to_string();
            process_line(&connection, &line, &mut state, "a").unwrap();
        }

        let total: i64 = connection
            .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(total, 526);
        let samples: Vec<(i64, i64, i64)> = connection
            .prepare(
                "SELECT segment_no, cumulative_tokens, delta_tokens
                 FROM turn_token_samples ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(samples, vec![(0, 274, 274), (1, 252, 252)]);
        verify_token_accounting(&connection, Some("a")).unwrap();
    }

    #[test]
    fn repeated_turn_context_preserves_the_current_turn_segment_baseline() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let mut state = CursorState::default();
        process_line(
            &connection,
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            &mut state,
            "a",
        )
        .unwrap();
        let context = r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#;
        process_line(&connection, context, &mut state, "a").unwrap();
        let first = serde_json::json!({
            "timestamp": "2026-08-14T00:00:02Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": { "total_token_usage": { "input_tokens": 100, "total_tokens": 100 } }
            }
        })
        .to_string();
        process_line(&connection, &first, &mut state, "a").unwrap();
        let baseline_before = state.turn_segment_start_usage.raw_total_tokens;
        let accumulated_before = state.turn_accumulated_usage.raw_total_tokens;

        process_line(&connection, context, &mut state, "a").unwrap();
        assert_eq!(
            state.turn_segment_start_usage.raw_total_tokens,
            baseline_before
        );
        assert_eq!(
            state.turn_accumulated_usage.raw_total_tokens,
            accumulated_before
        );

        let second = serde_json::json!({
            "timestamp": "2026-08-14T00:00:03Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": { "total_token_usage": { "input_tokens": 200, "total_tokens": 200 } }
            }
        })
        .to_string();
        process_line(&connection, &second, &mut state, "a").unwrap();

        let samples: Vec<(i64, i64)> = connection
            .prepare(
                "SELECT cumulative_tokens, delta_tokens
                 FROM turn_token_samples ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(samples, vec![(100, 100), (200, 100)]);
        verify_token_accounting(&connection, Some("a")).unwrap();
    }

    #[test]
    fn late_session_meta_does_not_reset_a_repeated_turn_context() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let mut state = CursorState::default();
        let first_context = r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#;
        process_line(&connection, first_context, &mut state, "a").unwrap();
        process_line(
            &connection,
            r#"{"timestamp":"2026-08-14T00:00:02Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            &mut state,
            "a",
        )
        .unwrap();
        process_line(
            &connection,
            r#"{"timestamp":"2026-08-14T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}}"#,
            &mut state,
            "a",
        )
        .unwrap();
        process_line(
            &connection,
            r#"{"timestamp":"2026-08-14T00:00:04Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#,
            &mut state,
            "a",
        )
        .unwrap();
        process_line(
            &connection,
            r#"{"timestamp":"2026-08-14T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"total_tokens":200}}}}"#,
            &mut state,
            "a",
        )
        .unwrap();

        let turn: (i64, i64) = connection
            .query_row(
                "SELECT started_at, raw_total_tokens FROM turn_usage
                 WHERE account_key = 'a' AND thread_id = 's' AND turn_id = 't'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(turn, (1786665601, 200));
        verify_token_accounting(&connection, Some("a")).unwrap();
    }

    #[test]
    fn repeated_cumulative_value_after_reset_is_not_deduplicated() {
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
        for total in [100, 200, 100] {
            let line = serde_json::json!({
                "timestamp": "2026-08-14T00:00:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": { "total_token_usage": { "input_tokens": total, "total_tokens": total } }
                }
            })
            .to_string();
            process_line(&connection, &line, &mut state, "a").unwrap();
        }
        let samples: Vec<(i64, i64)> = connection
            .prepare(
                "SELECT segment_no, cumulative_tokens
                 FROM turn_token_samples ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(samples, vec![(0, 100), (0, 200), (1, 100)]);
    }

    #[test]
    fn accounting_invariant_reports_a_mismatched_derived_total() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, reasoning_effort,
                  speed_mode, raw_total_tokens, source, confidence, created_at, updated_at)
                 VALUES ('a', 'thread', 'turn', 1, 'high', 'standard', 10,
                         'rollout', 'high', 1, 1)",
                [],
            )
            .unwrap();
        let error = verify_token_accounting(&connection, None).unwrap_err();
        assert!(error.contains("canonical=10"));
        assert!(error.contains("timeline=0"));
    }

    #[test]
    fn accounting_invariant_rejects_orphan_timeline_samples() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, model,
                  reasoning_effort, speed_mode, sampled_at, cumulative_tokens,
                  delta_tokens, source, confidence)
                 VALUES ('a', 'thread', 'orphan', 0, 'gpt-5.6-sol',
                         'high', 'standard', 2, 300, 300, 'rollout', 'high')",
                [],
            )
            .unwrap();

        let error = verify_token_accounting(&connection, Some("a")).unwrap_err();
        assert!(error.contains("orphan token timeline"));
        audit_timeline_gaps(&connection, Some("a"), "legacy_pre_timeline").unwrap();
        let health = set_account_data_health(
            &connection,
            "a",
            DATA_HEALTH_ACCOUNTING_INCONSISTENT,
            ROLLOUT_PARSER_VERSION,
        )
        .unwrap();
        assert_eq!(health.status, DATA_HEALTH_ACCOUNTING_INCONSISTENT);
        assert_eq!(health.orphan_timeline_samples, 1);
        assert_eq!(health.missing_timeline_turns, 0);
        assert_eq!(
            connection
                .query_row(
                    "SELECT reason FROM turn_timeline_audits
                     WHERE account_key = 'a' AND turn_id = 'orphan'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "orphan_timeline"
        );
    }

    #[test]
    fn timeline_audit_classifies_missing_and_partial_history() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for (turn_id, total) in [("missing", 10), ("partial", 20)] {
            connection
                .execute(
                    "INSERT INTO turn_usage
                     (account_key, thread_id, turn_id, started_at, reasoning_effort,
                      speed_mode, raw_total_tokens, source, confidence, created_at, updated_at)
                     VALUES ('history-account', 'thread', ?1, 1, 'high', 'standard',
                             ?2, 'rollout', 'high', 1, 1)",
                    params![turn_id, total],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, model,
                  reasoning_effort, speed_mode, sampled_at, cumulative_tokens,
                  delta_tokens, source, confidence)
                 VALUES ('history-account', 'thread', 'partial', 0, 'gpt-5.6-sol',
                         'high', 'standard', 2, 10, 10, 'rollout', 'high')",
                [],
            )
            .unwrap();

        audit_timeline_gaps(&connection, None, "legacy_pre_timeline").unwrap();
        let counts: std::collections::BTreeMap<String, i64> = connection
            .prepare("SELECT reason, COUNT(*) FROM turn_timeline_audits GROUP BY reason")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(counts.get("legacy_pre_timeline"), Some(&1));
        assert_eq!(counts.get("accounting_error"), Some(&1));
        assert_eq!(
            connection
                .query_row(
                    "SELECT reason FROM turn_timeline_audits
                     WHERE account_key = 'history-account' AND turn_id = 'missing'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "legacy_pre_timeline"
        );
    }

    #[test]
    fn rollout_files_are_immutable_bound_or_quarantined_per_account() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let suffix = format!("{}-{}", std::process::id(), recorder::now_seconds());
        let first = std::env::temp_dir().join(format!("codex-nexus-binding-a-{suffix}.jsonl"));
        let second = std::env::temp_dir().join(format!("codex-nexus-binding-b-{suffix}.jsonl"));
        let cursored = std::env::temp_dir().join(format!("codex-nexus-binding-c-{suffix}.jsonl"));
        fs::write(&first, "").unwrap();
        fs::write(&second, "").unwrap();
        fs::write(&cursored, "").unwrap();
        connection
            .execute(
                "INSERT INTO rollout_cursors
                 (file_path, byte_offset, last_scanned_at, state_json)
                 VALUES (?1, 0, 1, '{}')",
                [cursored.to_string_lossy().as_ref()],
            )
            .unwrap();

        assert!(!bind_or_quarantine_rollout_file(&connection, &first, "account-a", false).unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM rollout_file_bindings WHERE file_path = ?1",
                    [first.to_string_lossy().as_ref()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "pending"
        );
        assert!(bind_or_quarantine_rollout_file(&connection, &first, "account-a", true).unwrap());
        assert!(!bind_or_quarantine_rollout_file(&connection, &first, "account-b", true).unwrap());
        assert!(bind_or_quarantine_rollout_file(&connection, &second, "account-b", true).unwrap());
        assert!(
            !bind_or_quarantine_rollout_file(&connection, &cursored, "account-b", true).unwrap()
        );
        let bindings: Vec<(String, Option<String>, String)> = connection
            .prepare(
                "SELECT file_path, account_key, status
                 FROM rollout_file_bindings ORDER BY file_path",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].1.as_deref(), Some("account-a"));
        assert_eq!(bindings[0].2, "bound");
        assert_eq!(bindings[2].1, None);
        assert_eq!(bindings[2].2, "pending");
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
        fs::remove_file(cursored).unwrap();
    }

    #[test]
    fn parse_errors_are_account_scoped_hard_blockers_for_verified_health() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = std::env::temp_dir().join(format!(
            "codex-nexus-parse-error-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        fs::write(&path, "not-json\n").unwrap();
        collect_one(&connection, &path, "account-a", "rebuild-batch-a").unwrap();
        let health = set_account_data_health(
            &connection,
            "account-a",
            DATA_HEALTH_VERIFIED,
            ROLLOUT_PARSER_VERSION,
        )
        .unwrap();
        assert_eq!(health.status, DATA_HEALTH_ACCOUNTING_INCONSISTENT);
        assert_eq!(health.parse_error_count, 1);
        assert_eq!(
            health.last_rebuild_batch_id.as_deref(),
            Some("rebuild-batch-a")
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unconsumed_bound_rollout_bytes_make_verified_health_source_incomplete() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = std::env::temp_dir().join(format!(
            "codex-nexus-source-gap-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        fs::write(&path, "pending content\n").unwrap();
        assert!(bind_or_quarantine_rollout_file(&connection, &path, "account-a", true).unwrap());
        // Completeness is production-owned by the durable source registry;
        // the legacy fixture above is only the compatibility binding path.
        let source_id =
            collector_core::register_source(&connection, &path, recorder::now_seconds()).unwrap();
        assert!(collector_core::bind_source(
            &connection,
            &source_id,
            "account-a",
            "test",
            "high",
            "test_verified_source",
            None,
            recorder::now_seconds(),
        )
        .unwrap());
        connection
            .execute(
                "UPDATE rollout_sources
                 SET last_offset = 0, last_size = 0
                 WHERE source_id = ?1",
                [&source_id],
            )
            .unwrap();
        let health = set_account_data_health(
            &connection,
            "account-a",
            DATA_HEALTH_VERIFIED,
            ROLLOUT_PARSER_VERSION,
        )
        .unwrap();
        assert_eq!(health.status, DATA_HEALTH_SOURCE_INCOMPLETE);
        assert_eq!(health.source_incomplete_count, 1);

        connection
            .execute(
                "UPDATE rollout_sources SET last_offset = ?2 WHERE source_id = ?1",
                params![source_id, fs::metadata(&path).unwrap().len() as i64],
            )
            .unwrap();
        let health = set_account_data_health(
            &connection,
            "account-a",
            DATA_HEALTH_VERIFIED,
            ROLLOUT_PARSER_VERSION,
        )
        .unwrap();
        assert_eq!(health.status, DATA_HEALTH_VERIFIED);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn quota_transition_refreshes_stale_verified_health() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, reasoning_effort,
                  speed_mode, raw_total_tokens, source, confidence, created_at, updated_at)
                 VALUES ('a', 'thread', 'turn', 100, 'high', 'standard',
                         10, 'rollout', 'high', 100, 100)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, reasoning_effort,
                  speed_mode, sampled_at, cumulative_tokens, delta_tokens,
                  source, confidence)
                 VALUES ('a', 'thread', 'turn', 0, 'high', 'standard',
                         100, 10, 10, 'rollout', 'high')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO rate_limit_samples
                 (account_key, sampled_at, limit_id, window, window_duration_mins,
                  used_percent, resets_at, source, confidence)
                 VALUES ('a', 100, 'codex', 'primary', 10080, 1.0, 10000,
                         'rate_limits', 'high')",
                [],
            )
            .unwrap();
        let health = set_account_data_health(
            &connection,
            "a",
            DATA_HEALTH_VERIFIED,
            ROLLOUT_PARSER_VERSION,
        )
        .unwrap();
        assert_eq!(health.status, DATA_HEALTH_VERIFIED);

        connection
            .execute(
                "INSERT INTO rate_limit_samples
                 (account_key, sampled_at, limit_id, window, window_duration_mins,
                  used_percent, resets_at, source, confidence)
                 VALUES ('a', 501, 'codex', 'primary', 10080, 2.0, 10000,
                         'rate_limits', 'high')",
                [],
            )
            .unwrap();
        assert_eq!(
            account_data_health(&connection, "a")
                .unwrap()
                .unwrap()
                .status,
            DATA_HEALTH_VERIFIED
        );
        let refreshed = refresh_account_data_health(&connection, "a").unwrap();
        assert_eq!(refreshed.status, DATA_HEALTH_SOURCE_INCOMPLETE);
        assert_eq!(refreshed.source_lag_seconds, 401);
        assert_eq!(refreshed.source_incomplete_count, 1);
    }

    #[test]
    fn unchanged_quota_polls_do_not_create_source_lag() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for sampled_at in [100, 1_000] {
            connection
                .execute(
                    "INSERT INTO rate_limit_samples
                     (account_key, sampled_at, limit_id, window, window_duration_mins,
                      used_percent, resets_at, source, confidence)
                     VALUES ('a', ?1, 'codex', 'primary', 10080, 1.0, 10000,
                             'rate_limits', 'high')",
                    [sampled_at],
                )
                .unwrap();
        }
        let health = set_account_data_health(
            &connection,
            "a",
            DATA_HEALTH_VERIFIED,
            ROLLOUT_PARSER_VERSION,
        )
        .unwrap();
        assert_eq!(health.status, DATA_HEALTH_VERIFIED);
        assert_eq!(health.source_lag_seconds, 0);
    }

    #[test]
    fn rollout_rebuild_is_idempotent() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = std::env::temp_dir().join(format!(
            "codex-nexus-rollout-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        let content = [
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, content).unwrap();

        assert!(rebuild_rollout_derived_data(&connection, &[path.clone()], "a").unwrap());
        let first: (i64, i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM turn_usage),
                    (SELECT COUNT(*) FROM turn_token_samples),
                    (SELECT COALESCE(SUM(delta_tokens), 0) FROM turn_token_samples)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(!rebuild_rollout_derived_data(&connection, &[path.clone()], "a").unwrap());
        let second: (i64, i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM turn_usage),
                    (SELECT COUNT(*) FROM turn_token_samples),
                    (SELECT COALESCE(SUM(delta_tokens), 0) FROM turn_token_samples)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(first, (1, 1, 100));
        assert_eq!(second, first);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn account_scoped_migration_quarantines_legacy_history_without_rebuilding_verified_account() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO accounts (account_key, first_seen_at, last_seen_at)
                 VALUES ('legacy-a', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, reasoning_effort,
                  speed_mode, raw_total_tokens, source, confidence, created_at, updated_at)
                 VALUES ('legacy-a', 'old-thread', 'old-turn', 1, 'high',
                         'standard', 127, 'rollout', 'low', 1, 1)",
                [],
            )
            .unwrap();
        // Simulate opening an existing database after the account-scoped
        // version table was introduced.
        initialize_schema(&connection).unwrap();
        classify_legacy_accounts(&connection).unwrap();

        let path = std::env::temp_dir().join(format!(
            "codex-nexus-multi-account-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        let content = [
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"new-thread"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"new-turn","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"total_tokens":50}}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, content).unwrap();

        assert!(rebuild_rollout_derived_data(&connection, &[path.clone()], "verified-b").unwrap());
        let legacy = account_data_health(&connection, "legacy-a")
            .unwrap()
            .unwrap();
        let verified = account_data_health(&connection, "verified-b")
            .unwrap()
            .unwrap();
        assert_eq!(legacy.status, DATA_HEALTH_LEGACY_UNVERIFIED);
        assert_eq!(legacy.missing_timeline_turns, 1);
        assert_eq!(verified.status, DATA_HEALTH_VERIFIED);
        assert_eq!(verified.data_version, ROLLOUT_PARSER_VERSION);
        verify_token_accounting(&connection, Some("verified-b")).unwrap();
        assert!(verify_token_accounting(&connection, None).is_err());
        assert_eq!(
            connection
                .query_row(
                    "SELECT raw_total_tokens FROM turn_usage
                     WHERE account_key = 'legacy-a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            127
        );

        assert!(!rebuild_rollout_derived_data(&connection, &[path.clone()], "verified-b").unwrap());
        assert!(!rebuild_rollout_derived_data(&connection, &[], "legacy-a").unwrap());
        assert_eq!(
            account_data_health(&connection, "legacy-a")
                .unwrap()
                .unwrap()
                .status,
            DATA_HEALTH_LEGACY_UNVERIFIED
        );
        assert!(!rebuild_rollout_derived_data(&connection, &[], "legacy-a").unwrap());
        assert!(!rebuild_rollout_derived_data(&connection, &[path.clone()], "verified-b").unwrap());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn v9_migration_discards_previously_verified_dirty_history() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, reasoning_effort,
                  speed_mode, raw_total_tokens, source, confidence, created_at, updated_at)
                 VALUES ('a', 'thread', 'polluted', 1, 'high', 'standard',
                         100, 'rollout', 'high', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, reasoning_effort,
                  speed_mode, sampled_at, cumulative_tokens, delta_tokens,
                  source, confidence)
                 VALUES ('a', 'thread', 'polluted', 0, 'high', 'standard',
                         2, 100, 100, 'rollout', 'high')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO account_usage_data_versions
                 (account_key, rollout_parser_version, status, timeline_status,
                  missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
                  parse_error_count, source_incomplete_count, source_lag_seconds,
                  updated_at)
                 VALUES ('a', 3, 'verified', 'complete', 0, 0, 0, 0, 0, 0, 1)
                 ON CONFLICT(account_key) DO UPDATE SET
                   rollout_parser_version = 3, status = 'verified',
                   timeline_status = 'complete', updated_at = 1",
                [],
            )
            .unwrap();

        assert!(!rebuild_rollout_derived_data(&connection, &[], "a").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM turn_usage WHERE account_key = 'a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM turn_timeline_audits
                     WHERE account_key = 'a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let health = account_data_health(&connection, "a").unwrap().unwrap();
        assert_eq!(health.data_version, ROLLOUT_PARSER_VERSION);
        assert_eq!(health.status, DATA_HEALTH_VERIFIED);
    }

    #[test]
    fn v9_migration_advances_lagging_cursor_without_counting_skipped_history() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = std::env::temp_dir().join(format!(
            "codex-nexus-v9-rebase-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        let prefix = [
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, &prefix).unwrap();
        collect_one(&connection, &path, "a", "before-v9").unwrap();
        connection
            .execute(
                "INSERT INTO rollout_file_bindings
                 (file_path, account_key, status, reason, first_seen_at, last_seen_at)
                 VALUES (?1, 'a', 'bound', 'test', 1, 1)",
                [path.to_string_lossy().as_ref()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO account_usage_data_versions
                 (account_key, rollout_parser_version, status, timeline_status,
                  missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
                  parse_error_count, source_incomplete_count, source_lag_seconds, updated_at)
                 VALUES ('a', 5, 'verified', 'complete', 0, 0, 0, 0, 0, 0, 1)
                 ON CONFLICT(account_key) DO UPDATE SET
                   rollout_parser_version = 5, status = 'verified', updated_at = 1",
                [],
            )
            .unwrap();

        let skipped = prefix.clone()
            + r#"{"timestamp":"2026-08-14T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"total_tokens":200}}}}"#
            + "\n";
        fs::write(&path, &skipped).unwrap();
        assert!(!rebuild_rollout_derived_data(&connection, &[path.clone()], "a").unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM turn_usage WHERE account_key = 'a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        let fresh = skipped
            + r#"{"timestamp":"2026-08-14T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":250,"total_tokens":250}}}}"#
            + "\n";
        fs::write(&path, fresh).unwrap();
        assert!(collector_core::catch_up_path(
            &connection,
            &path,
            Some("a"),
            false,
            recorder::now_seconds(),
        )
        .unwrap());
        let total: i64 = connection
            .query_row(
                "SELECT raw_total_tokens FROM turn_usage WHERE account_key = 'a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total, 50);
        verify_token_accounting(&connection, Some("a")).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn v9_quarantined_active_file_resumes_from_migration_tail() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = std::env::temp_dir().join(format!(
            "codex-nexus-v9-active-tail-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        let historical = [
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, &historical).unwrap();
        connection
            .execute(
                "INSERT INTO rollout_file_bindings
                 (file_path, account_key, status, reason, first_seen_at, last_seen_at)
                 VALUES (?1, NULL, 'pending', 'awaiting_watcher_binding', 1, 1)",
                [path.to_string_lossy().as_ref()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO account_usage_data_versions
                 (account_key, rollout_parser_version, status, timeline_status,
                  missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
                  parse_error_count, source_incomplete_count, source_lag_seconds, updated_at)
                 VALUES ('a', 8, 'verified', 'complete', 0, 0, 0, 0, 0, 0, 1)",
                [],
            )
            .unwrap();

        assert!(!rebuild_rollout_derived_data(&connection, &[path.clone()], "a").unwrap());
        let migrated: (String, i64) = connection
            .query_row(
                "SELECT binding_status, last_offset
                 FROM rollout_sources WHERE canonical_path = ?1",
                [path.canonicalize().unwrap().to_string_lossy().as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(migrated, ("quarantined".into(), historical.len() as i64));

        let fresh = historical
            + r#"{"timestamp":"2026-08-14T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"total_tokens":150}}}}"#
            + "\n";
        fs::write(&path, fresh).unwrap();
        let source_id: String = connection
            .query_row(
                "SELECT source_id FROM rollout_sources WHERE canonical_path = ?1",
                [path.canonicalize().unwrap().to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(collector_core::bind_source(
            &connection,
            &source_id,
            "a",
            "test_recovery",
            "high",
            "historical_reset_tail",
            None,
            recorder::now_seconds(),
        )
        .unwrap());
        assert!(collector_core::catch_up_path(
            &connection,
            &path,
            None,
            false,
            recorder::now_seconds(),
        )
        .unwrap());
        let total: i64 = connection
            .query_row(
                "SELECT raw_total_tokens FROM turn_usage WHERE account_key = 'a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total, 50);
        verify_token_accounting(&connection, Some("a")).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn v9_migration_skips_malformed_historical_rows() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = std::env::temp_dir().join(format!(
            "codex-nexus-v9-malformed-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        let content = [
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            "not-json",
            r#"{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, &content).unwrap();
        connection
            .execute(
                "INSERT INTO rollout_file_bindings
                 (file_path, account_key, status, reason, first_seen_at, last_seen_at)
                 VALUES (?1, NULL, 'pending', 'awaiting_watcher_binding', 1, 1)",
                [path.to_string_lossy().as_ref()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO account_usage_data_versions
                 (account_key, rollout_parser_version, status, timeline_status,
                  missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
                  parse_error_count, source_incomplete_count, source_lag_seconds, updated_at)
                 VALUES ('a', 8, 'verified', 'complete', 0, 0, 0, 0, 0, 0, 1)",
                [],
            )
            .unwrap();

        assert!(!rebuild_rollout_derived_data(&connection, &[path.clone()], "a").unwrap());
        let cursor: i64 = connection
            .query_row(
                "SELECT last_offset FROM rollout_sources WHERE canonical_path = ?1",
                [path.canonicalize().unwrap().to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursor, content.len() as i64);
        assert_eq!(
            account_data_health(&connection, "a")
                .unwrap()
                .unwrap()
                .data_version,
            ROLLOUT_PARSER_VERSION
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM turn_usage", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn v9_missing_bound_file_is_rebased_if_it_returns() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let path = std::env::temp_dir().join(format!(
            "codex-nexus-v9-returned-{}-{}.jsonl",
            std::process::id(),
            recorder::now_seconds()
        ));
        let path_string = path.to_string_lossy().into_owned();
        connection
            .execute(
                "INSERT INTO rollout_file_bindings
                 (file_path, account_key, status, reason, first_seen_at, last_seen_at)
                 VALUES (?1, 'a', 'bound', 'old-binding', 1, 1)",
                [&path_string],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO rollout_cursors
                 (file_path, byte_offset, last_scanned_at, state_json)
                 VALUES (?1, 10, 1, '{\"parserVersion\":8}')",
                [&path_string],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO account_usage_data_versions
                 (account_key, rollout_parser_version, status, timeline_status,
                  missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
                  parse_error_count, source_incomplete_count, source_lag_seconds, updated_at)
                 VALUES ('a', 8, 'verified', 'complete', 0, 0, 0, 0, 0, 0, 1)",
                [],
            )
            .unwrap();

        assert!(!rebuild_rollout_derived_data(&connection, &[path.clone()], "a").unwrap());
        let migrated: (Option<String>, String, i64) = connection
            .query_row(
                "SELECT account_key, binding_status, last_offset
                 FROM rollout_sources WHERE canonical_path = ?1",
                [path
                    .canonicalize()
                    .unwrap_or(path.clone())
                    .to_string_lossy()
                    .as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(migrated, (None, "quarantined".into(), 0));

        let historical = [
            r#"{"timestamp":"2026-08-14T00:00:00Z","type":"session_meta","payload":{"session_id":"s"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:01Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-5.6-sol","effort":"high","service_tier":"standard"}}"#,
            r#"{"timestamp":"2026-08-14T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"total_tokens":100}}}}"#,
        ]
        .join("\n")
            + "\n";
        fs::write(&path, &historical).unwrap();
        assert!(!collector_core::catch_up_path(
            &connection,
            &path,
            Some("a"),
            true,
            recorder::now_seconds(),
        )
        .unwrap());

        let fresh = historical
            + r#"{"timestamp":"2026-08-14T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"total_tokens":150}}}}"#
            + "\n";
        fs::write(&path, fresh).unwrap();
        assert!(collector_core::catch_up_path(
            &connection,
            &path,
            Some("a"),
            true,
            recorder::now_seconds(),
        )
        .unwrap());
        assert_eq!(
            connection
                .query_row(
                    "SELECT raw_total_tokens FROM turn_usage WHERE account_key = 'a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            50
        );
        verify_token_accounting(&connection, Some("a")).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cumulative_total_wins_over_stale_last_turn_usage() {
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
        assert_eq!(total, 100);
        let sample_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM turn_token_samples", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sample_count, 1);
    }

    #[test]
    fn repeated_cumulative_total_does_not_use_stale_last_turn_usage() {
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
        for last in [7, 9] {
            let line = serde_json::json!({
                "timestamp": "2026-08-14T00:00:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": { "input_tokens": 100, "total_tokens": 100 },
                        "last_token_usage": { "input_tokens": last, "total_tokens": last }
                    }
                }
            })
            .to_string();
            process_line(&connection, &line, &mut state, "a").unwrap();
        }
        let total: i64 = connection
            .query_row("SELECT raw_total_tokens FROM turn_usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(total, 100);
        let sample_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM turn_token_samples", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sample_count, 1);
    }
}
