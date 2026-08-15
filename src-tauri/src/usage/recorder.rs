use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::models::{Confidence, SOURCE_APP_SERVER, SOURCE_OFFICIAL};
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
                     AND sampled_at >= ?4 AND used_percent = ?5
                     AND (resets_at IS ?6 OR resets_at = ?6)
                 )",
                    params![
                        account_key,
                        limit_id,
                        window_name,
                        sampled_at - 30,
                        used_percent,
                        resets_at
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
                        SOURCE_APP_SERVER,
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
}

pub fn confidence_string(confidence: &Confidence) -> &'static str {
    quota::confidence_string(confidence)
}
