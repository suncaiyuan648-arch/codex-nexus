use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::models::{
    Confidence, PROVENANCE_ACCOUNT_RATE_LIMIT, PROVENANCE_APP_SERVER_THREAD_USAGE, SOURCE_OFFICIAL,
};
use super::{db, quota};

pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn number_i64(value: Option<&Value>) -> Option<i64> {
    value
        .and_then(Value::as_i64)
        .or_else(|| value.and_then(Value::as_u64).map(|value| value as i64))
        .or_else(|| value.and_then(Value::as_f64).map(|value| value as i64))
}

fn number_f64(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .or_else(|| value.and_then(Value::as_i64).map(|value| value as f64))
        .or_else(|| value.and_then(Value::as_u64).map(|value| value as f64))
}

fn string_value(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn timestamp(value: Option<&Value>) -> i64 {
    number_i64(value)
        .map(|value| {
            if value > 10_000_000_000 {
                value / 1000
            } else {
                value
            }
        })
        .unwrap_or_else(now_seconds)
}

pub fn snapshot_account_key(snapshot: &Value) -> String {
    let account = snapshot
        .get("account")
        .and_then(|value| value.get("account"));
    if let Some(id) = account
        .and_then(|value| value.get("id").or_else(|| value.get("accountId")))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return format!("account:{}", id.trim());
    }

    let email = account
        .and_then(|value| value.get("email"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let auth_identity = snapshot
        .get("codexPath")
        .and_then(Value::as_str)
        .unwrap_or("codex")
        .trim()
        .to_ascii_lowercase();
    if !email.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(email.as_bytes());
        hasher.update(b"\n");
        hasher.update(auth_identity.as_bytes());
        return format!("sha256:{}", hex::encode(hasher.finalize()));
    }
    format!("local:{}", auth_identity)
}

pub fn record_account_snapshot(
    connection: &Connection,
    snapshot: &Value,
    account_key: &str,
    seen_at: i64,
) -> Result<(), String> {
    let account = snapshot
        .get("account")
        .and_then(|value| value.get("account"));
    let email = string_value(account.and_then(|value| value.get("email")));
    let display_name = string_value(
        account.and_then(|value| value.get("name").or_else(|| value.get("displayName"))),
    );
    let plan_type = string_value(account.and_then(|value| value.get("planType")));
    connection
        .execute(
            "INSERT INTO accounts
         (account_key, display_name, email, plan_type, auth_identity, first_seen_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(account_key) DO UPDATE SET
           display_name = COALESCE(excluded.display_name, accounts.display_name),
           email = COALESCE(excluded.email, accounts.email),
           plan_type = COALESCE(excluded.plan_type, accounts.plan_type),
           last_seen_at = excluded.last_seen_at",
            params![
                account_key,
                display_name,
                email,
                plan_type,
                snapshot.get("codexPath").and_then(Value::as_str),
                seen_at
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn record_official_snapshot(
    app: &tauri::AppHandle<tauri::Wry>,
    snapshot: &Value,
) -> Result<(), String> {
    let connection = db::open_database(app)?;
    let account_key = snapshot_account_key(snapshot);
    let account_key = if account_key.starts_with("local:") {
        current_account_key(&connection)?.unwrap_or(account_key)
    } else {
        account_key
    };
    let fetched_at = timestamp(snapshot.get("fetchedAt"));
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    record_account_snapshot(&transaction, snapshot, &account_key, fetched_at)?;

    if let Some(buckets) = snapshot
        .get("usage")
        .and_then(|value| value.get("dailyUsageBuckets"))
        .and_then(Value::as_array)
    {
        for bucket in buckets {
            let Some(date) = bucket.get("startDate").and_then(Value::as_str) else {
                continue;
            };
            let Some(tokens) = number_i64(bucket.get("tokens")) else {
                continue;
            };
            transaction
                .execute(
                    "INSERT INTO account_daily_usage
                 (account_key, date, official_tokens, fetched_at, source, confidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(account_key, date) DO UPDATE SET
                   official_tokens = excluded.official_tokens,
                   fetched_at = excluded.fetched_at,
                   source = excluded.source,
                   confidence = excluded.confidence",
                    params![
                        account_key,
                        date,
                        tokens.max(0),
                        fetched_at,
                        SOURCE_OFFICIAL,
                        "high"
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
    }

    record_rate_limit_samples(&transaction, &account_key, fetched_at, snapshot)?;
    transaction.commit().map_err(|error| error.to_string())
}

pub fn record_rate_limit_update(
    app: &tauri::AppHandle<tauri::Wry>,
    payload: &Value,
) -> Result<(), String> {
    let connection = db::open_database(app)?;
    let Some(account_key) = current_account_key(&connection)? else {
        return Ok(());
    };
    record_rate_limit_samples(&connection, &account_key, now_seconds(), payload)
}

fn normalize_reasoning(value: Option<&Value>) -> String {
    match string_value(value)
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("low") | Some("medium") | Some("high") | Some("xhigh") | Some("ultra") => {
            string_value(value).unwrap().to_ascii_lowercase()
        }
        _ => "unknown".into(),
    }
}

fn normalize_speed(value: Option<&Value>) -> String {
    match string_value(value)
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("standard") | Some("default") => "standard".into(),
        Some("fast") | Some("priority") | Some("fast_requested") => "fast_requested".into(),
        _ => "unknown".into(),
    }
}

fn record_thread_usage_capability(
    connection: &Connection,
    account_key: &str,
    thread_id: &str,
    attempted_at: i64,
    capability: &str,
    sampled_at: Option<i64>,
    error: Option<&str>,
) -> Result<(), String> {
    let latest_turn_update: Option<i64> = connection
        .query_row(
            "SELECT MAX(updated_at) FROM turn_usage
             WHERE account_key = ?1 AND thread_id = ?2",
            params![account_key, thread_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let previous_attempt: Option<i64> = connection
        .query_row(
            "SELECT last_attempted_at FROM thread_usage_capabilities
             WHERE account_key = ?1 AND thread_id = ?2",
            params![account_key, thread_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if previous_attempt.is_some_and(|attempt| latest_turn_update.unwrap_or_default() > attempt) {
        connection
            .execute(
                "UPDATE thread_usage_capabilities SET retry_count = 0
                 WHERE account_key = ?1 AND thread_id = ?2",
                params![account_key, thread_id],
            )
            .map_err(|error| error.to_string())?;
    }
    connection
        .execute(
            "INSERT INTO thread_usage_capabilities
             (account_key, thread_id, last_attempted_at, last_sampled_at, capability, error, retry_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
             ON CONFLICT(account_key, thread_id) DO UPDATE SET
               last_attempted_at = excluded.last_attempted_at,
               last_sampled_at = excluded.last_sampled_at,
               capability = excluded.capability,
               error = excluded.error,
               retry_count = thread_usage_capabilities.retry_count + 1",
            params![
                account_key,
                thread_id,
                attempted_at,
                sampled_at,
                capability,
                error
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Persist the server's thread-level usage snapshot without merging it into
/// local turn usage. `threadUsage` is optional and may be null when the
/// account/server cannot expose billing data.
pub fn record_thread_usage_snapshot(
    app: &tauri::AppHandle<tauri::Wry>,
    thread_id: &str,
    response: &Value,
) -> Result<(), String> {
    let connection = db::open_database(app)?;
    let Some(account_key) = current_account_key(&connection)? else {
        return Ok(());
    };
    let sampled_at = timestamp(response.get("sampledAt")).max(now_seconds());
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let thread_usage = response.get("threadUsage").and_then(Value::as_object);
    let capability = if thread_usage.is_some() {
        "available"
    } else {
        "unavailable"
    };
    record_thread_usage_capability(
        &transaction,
        &account_key,
        thread_id,
        sampled_at,
        capability,
        thread_usage.map(|_| sampled_at),
        None,
    )?;

    if let Some(thread_usage) = thread_usage {
        let groups = thread_usage
            .get("groups")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let usage_thread_id = thread_usage
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or(thread_id);
        let estimated_usd = number_i64(thread_usage.get("estimatedUsageUsdMicros"));
        for group in groups {
            let Some(credits) = number_i64(group.get("estimatedUsageCreditsMicros")) else {
                continue;
            };
            transaction
                .execute(
                    "INSERT INTO thread_usage_group_samples
                     (account_key, thread_id, sampled_at, model, reasoning_effort, speed_mode,
                      estimated_usage_credits_micros, estimated_usage_usd_micros,
                      net_new_input_tokens, cached_input_tokens, input_tokens, output_tokens,
                      total_tokens, source, confidence)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    params![
                        account_key,
                        usage_thread_id,
                        sampled_at,
                        string_value(group.get("model")),
                        normalize_reasoning(group.get("reasoningEffort")),
                        normalize_speed(group.get("speed")),
                        credits.max(0),
                        estimated_usd,
                        number_i64(group.get("netNewInputTokens")),
                        number_i64(group.get("cachedInputTokens")),
                        number_i64(group.get("inputTokens")),
                        number_i64(group.get("outputTokens")),
                        number_i64(group.get("totalTokens")),
                        PROVENANCE_APP_SERVER_THREAD_USAGE,
                        "high",
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

pub fn record_thread_usage_failure(
    app: &tauri::AppHandle<tauri::Wry>,
    thread_id: &str,
    error: &str,
) -> Result<(), String> {
    let connection = db::open_database(app)?;
    let Some(account_key) = current_account_key(&connection)? else {
        return Ok(());
    };
    record_thread_usage_capability(
        &connection,
        &account_key,
        thread_id,
        now_seconds(),
        "error",
        None,
        Some(error),
    )
}

pub fn current_account_key(connection: &Connection) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT account_key FROM accounts ORDER BY last_seen_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())
}

pub fn pending_thread_usage_threads(
    connection: &Connection,
    now: i64,
) -> Result<Vec<String>, String> {
    let Some(account_key) = current_account_key(connection)? else {
        return Ok(Vec::new());
    };
    let mut statement = connection
        .prepare(
            "SELECT t.thread_id
             FROM turn_usage t
             LEFT JOIN thread_usage_capabilities c
               ON c.account_key = t.account_key AND c.thread_id = t.thread_id
             WHERE t.account_key = ?1
               AND t.completed_at IS NOT NULL
               AND length(t.thread_id) = 36
               AND substr(t.thread_id, 9, 1) = '-'
               AND substr(t.thread_id, 14, 1) = '-'
               AND substr(t.thread_id, 19, 1) = '-'
               AND substr(t.thread_id, 24, 1) = '-'
             GROUP BY t.thread_id
             HAVING c.last_attempted_at IS NULL
                OR (MAX(t.updated_at) > c.last_attempted_at
                    AND c.last_attempted_at <= ?2 - 15)
                OR (MAX(t.updated_at) <= c.last_attempted_at
                    AND c.retry_count < 4
                    AND c.last_attempted_at <= ?2 -
                      CASE c.retry_count
                        WHEN 1 THEN 15
                        WHEN 2 THEN 60
                        WHEN 3 THEN 120
                        ELSE 15
                      END)
             ORDER BY MAX(t.updated_at) DESC",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, now], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    rows.map(|row| row.map_err(|error| error.to_string()))
        .collect()
}

pub fn next_thread_usage_at(connection: &Connection, now: i64) -> Result<Option<i64>, String> {
    let Some(account_key) = current_account_key(connection)? else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(
            "SELECT MAX(t.updated_at), c.last_attempted_at, COALESCE(c.retry_count, 0)
             FROM turn_usage t
             LEFT JOIN thread_usage_capabilities c
               ON c.account_key = t.account_key AND c.thread_id = t.thread_id
             WHERE t.account_key = ?1
               AND t.completed_at IS NOT NULL
               AND length(t.thread_id) = 36
               AND substr(t.thread_id, 9, 1) = '-'
               AND substr(t.thread_id, 14, 1) = '-'
               AND substr(t.thread_id, 19, 1) = '-'
               AND substr(t.thread_id, 24, 1) = '-'
             GROUP BY t.thread_id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut next = None;
    for row in rows {
        let (latest_update, last_attempt, retry_count) = row.map_err(|error| error.to_string())?;
        let Some(latest_update) = latest_update else {
            continue;
        };
        let due_at = match last_attempt {
            None => now,
            Some(last_attempt) if latest_update > last_attempt => last_attempt.saturating_add(15),
            Some(_) if retry_count >= 4 => continue,
            Some(last_attempt) => last_attempt.saturating_add(match retry_count {
                1 => 15,
                2 => 60,
                3 => 120,
                _ => 15,
            }),
        };
        next = Some(next.map_or(due_at, |current: i64| current.min(due_at)));
    }
    Ok(next)
}

fn record_rate_limit_samples(
    connection: &Connection,
    account_key: &str,
    sampled_at: i64,
    snapshot: &Value,
) -> Result<(), String> {
    let rate_limits = snapshot.get("rateLimits").unwrap_or(snapshot);
    let mut buckets: Vec<(String, &Value)> = Vec::new();
    if let Some(by_id) = rate_limits
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        buckets.extend(by_id.iter().map(|(key, value)| (key.clone(), value)));
    }
    if buckets.is_empty() {
        if let Some(bucket) = rate_limits.get("rateLimits") {
            if let Some(limit_id) = bucket.get("limitId").and_then(Value::as_str) {
                buckets.push((limit_id.to_owned(), bucket));
            }
        }
    }
    if buckets.is_empty() {
        if let Some(limit_id) = rate_limits.get("limitId").and_then(Value::as_str) {
            buckets.push((limit_id.to_owned(), rate_limits));
        }
    }

    for (limit_id, bucket) in buckets {
        for window_name in ["primary", "secondary"] {
            let Some(window) = bucket.get(window_name) else {
                continue;
            };
            let Some(duration) = number_i64(
                window
                    .get("windowDurationMins")
                    .or_else(|| window.get("window_minutes")),
            ) else {
                continue;
            };
            let Some(used_percent) = number_f64(window.get("usedPercent")) else {
                continue;
            };
            let resets_at = number_i64(window.get("resetsAt"));
            let duplicate: bool = connection
                .query_row(
                    "SELECT EXISTS(
                   SELECT 1 FROM rate_limit_samples
                   WHERE account_key = ?1 AND limit_id = ?2 AND window = ?3
                     AND window_duration_mins = ?4 AND sampled_at >= ?5
                     AND used_percent = ?6
                     AND ((?7 IS NULL AND resets_at IS NULL)
                          OR (?7 IS NOT NULL AND resets_at IS NOT NULL
                              AND ABS(resets_at - ?7) <= ?8))
                 )",
                    params![
                        account_key,
                        limit_id,
                        window_name,
                        duration,
                        sampled_at - 30,
                        used_percent,
                        resets_at,
                        quota::RESET_AT_TOLERANCE_SECS,
                    ],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if duplicate {
                continue;
            }
            connection
                .execute(
                    "INSERT INTO rate_limit_samples
                 (account_key, sampled_at, limit_id, window, window_duration_mins,
                  used_percent, resets_at, generation, source, confidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        account_key,
                        sampled_at,
                        limit_id,
                        window_name,
                        duration,
                        used_percent,
                        resets_at,
                        number_i64(snapshot.get("generation")),
                        PROVENANCE_ACCOUNT_RATE_LIMIT,
                        "high"
                    ],
                )
                .map_err(|error| error.to_string())?;
            quota::refresh_intervals(connection, account_key, &limit_id, window_name)?;
        }
    }
    Ok(())
}

pub fn ensure_account(
    connection: &Connection,
    account_key: &str,
    seen_at: i64,
) -> Result<(), String> {
    connection.execute(
        "INSERT INTO accounts (account_key, first_seen_at, last_seen_at)
         VALUES (?1, ?2, ?2)
         ON CONFLICT(account_key) DO UPDATE SET last_seen_at = MAX(accounts.last_seen_at, excluded.last_seen_at)",
        params![account_key, seen_at],
    ).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::db::initialize_schema;
    use serde_json::json;

    #[test]
    fn account_key_prefers_stable_id_and_hashes_email_fallback() {
        let with_id = json!({
            "account": { "account": { "id": "acct-1", "email": "user@example.com" } }
        });
        assert_eq!(snapshot_account_key(&with_id), "account:acct-1");

        let email = json!({
            "codexPath": "/usr/local/bin/codex",
            "account": { "account": { "email": " User@Example.com " } }
        });
        let same_email = json!({
            "codexPath": "/usr/local/bin/codex",
            "account": { "account": { "email": "user@example.com" } }
        });
        assert_eq!(
            snapshot_account_key(&email),
            snapshot_account_key(&same_email)
        );
        assert!(snapshot_account_key(&email).starts_with("sha256:"));
    }

    #[test]
    fn identical_rate_limit_notifications_are_deduplicated() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let snapshot = json!({
            "rateLimits": {
                "rateLimitsByLimitId": {
                    "codex": {
                        "primary": { "windowDurationMins": 10080, "usedPercent": 10.0, "resetsAt": 99 }
                    }
                }
            }
        });
        record_rate_limit_samples(&connection, "a", 100, &snapshot).unwrap();
        record_rate_limit_samples(&connection, "a", 101, &snapshot).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM rate_limit_samples", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn resets_at_jitter_within_tolerance_is_deduplicated() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        let first = json!({
            "rateLimits": {
                "rateLimitsByLimitId": {
                    "codex": {
                        "primary": { "windowDurationMins": 10080, "usedPercent": 10.0, "resetsAt": 99 }
                    }
                }
            }
        });
        let jittered = json!({
            "rateLimits": {
                "rateLimitsByLimitId": {
                    "codex": {
                        "primary": { "windowDurationMins": 10080, "usedPercent": 10.0, "resetsAt": 104 }
                    }
                }
            }
        });
        record_rate_limit_samples(&connection, "a", 100, &first).unwrap();
        record_rate_limit_samples(&connection, "a", 101, &jittered).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM rate_limit_samples", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }
}

pub fn confidence_string(confidence: &Confidence) -> &'static str {
    quota::confidence_string(confidence)
}
