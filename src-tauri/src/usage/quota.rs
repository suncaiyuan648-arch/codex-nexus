use rusqlite::{params, Connection};

use super::models::{is_unresolved_account_key, Confidence};

/// `resetsAt` is an epoch-second value, but the server can move it by a few
/// seconds between otherwise identical rate-limit snapshots. Treat that
/// jitter as the same quota cycle; a real reset is still detected by a
/// monotonicity break or a larger cycle identity change.
pub const RESET_AT_TOLERANCE_SECS: i64 = 5;

#[derive(Clone, Debug)]
struct Sample {
    id: i64,
    sampled_at: i64,
    window_duration_mins: i64,
    used_percent: f64,
    resets_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GapObservation {
    duration_ms: i64,
    token_count: i64,
    quality: &'static str,
}

fn observation_gap(
    connection: &Connection,
    account_key: &str,
    start_at: i64,
    end_at: i64,
) -> Result<GapObservation, String> {
    let gaps: Vec<(i64, i64)> = connection
        .prepare(
            "SELECT start_at, end_at FROM collector_gaps
             WHERE end_at > ?1 AND start_at < ?2 ORDER BY start_at",
        )
        .map_err(|error| error.to_string())?
        .query_map(params![start_at, end_at], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())?;
    let mut duration_ms = 0_i64;
    for (gap_start, gap_end) in gaps {
        let start = gap_start.max(start_at);
        let end = gap_end.min(end_at);
        if end > start {
            duration_ms =
                duration_ms.saturating_add(end.saturating_sub(start).saturating_mul(1000));
        }
    }
    let token_count = connection
        .query_row(
            "SELECT COALESCE(SUM(delta_tokens), 0) FROM turn_token_samples
             WHERE account_key = ?1 AND sampled_at > ?2 AND sampled_at <= ?3
               AND EXISTS (
                 SELECT 1 FROM collector_gaps g
                 WHERE g.end_at > ?2 AND g.start_at < ?3
                   AND turn_token_samples.sampled_at > g.start_at
                   AND turn_token_samples.sampled_at <= g.end_at
               )",
            params![account_key, start_at, end_at],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())?;
    let unresolved = connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM rollout_sources
               WHERE binding_status = 'unresolved'
                 AND COALESCE(first_activity_at, first_seen_at) < ?2
                 AND COALESCE(last_activity_at, first_seen_at) >= ?1
             )",
            params![start_at, end_at],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| error.to_string())?;
    let quality = if unresolved {
        "unresolved"
    } else if duration_ms > 5 * 60 * 1000 {
        "long_gap"
    } else if duration_ms > 0 {
        "bounded_gap"
    } else {
        "exact"
    };
    Ok(GapObservation {
        duration_ms,
        token_count,
        quality,
    })
}

pub fn same_reset_at(previous: Option<i64>, current: Option<i64>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => (current - previous).abs() <= RESET_AT_TOLERANCE_SECS,
        (None, None) => true,
        _ => false,
    }
}

pub fn cycle_id(resets_at: Option<i64>) -> String {
    resets_at
        .map(|value| format!("reset:{}", value.div_euclid(RESET_AT_TOLERANCE_SECS)))
        .unwrap_or_else(|| "reset:unknown".into())
}

/// Rebuilds quota-step intervals for one raw rate-limit bucket.
///
/// A row is a closed quota step: it starts at one observed quota transition
/// and ends at the next transition. This deliberately keeps the whole quota
/// plateau between transitions, instead of attributing only the samples
/// immediately adjacent to the second transition. The first and current open
/// plateaus remain pending and are not emitted as estimates.
pub fn refresh_intervals(
    connection: &Connection,
    account_key: &str,
    limit_id: &str,
    window: &str,
) -> Result<(), String> {
    if is_unresolved_account_key(account_key) {
        return Ok(());
    }
    let mut statement = connection
        .prepare(
            "SELECT id, sampled_at, window_duration_mins, used_percent, resets_at
             FROM rate_limit_samples
             WHERE account_key = ?1 AND limit_id = ?2 AND window = ?3
             ORDER BY sampled_at ASC, id ASC",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, limit_id, window], |row| {
            Ok(Sample {
                id: row.get(0)?,
                sampled_at: row.get(1)?,
                window_duration_mins: row.get(2)?,
                used_percent: row.get(3)?,
                resets_at: row.get(4)?,
            })
        })
        .map_err(|error| error.to_string())?;
    let mut samples = Vec::new();
    for row in rows {
        samples.push(row.map_err(|error| error.to_string())?);
    }

    connection
        .execute(
            "DELETE FROM quota_intervals WHERE account_key = ?1 AND limit_id = ?2 AND window = ?3",
            params![account_key, limit_id, window],
        )
        .map_err(|error| error.to_string())?;

    let mut previous: Option<Sample> = None;
    let mut last_transition: Option<Sample> = None;
    for current in samples {
        let Some(previous_sample) = previous.as_ref() else {
            previous = Some(current);
            continue;
        };

        let discontinuity = current.sampled_at <= previous_sample.sampled_at
            || current.window_duration_mins != previous_sample.window_duration_mins
            || !same_reset_at(previous_sample.resets_at, current.resets_at)
            || current.used_percent < previous_sample.used_percent;
        if discontinuity {
            last_transition = None;
            previous = Some(current);
            continue;
        }

        if current.used_percent > previous_sample.used_percent {
            if let Some(start) = last_transition.take() {
                insert_step(connection, account_key, limit_id, window, &start, &current)?;
            }
            last_transition = Some(current.clone());
        }
        previous = Some(current);
    }
    Ok(())
}

fn insert_step(
    connection: &Connection,
    account_key: &str,
    limit_id: &str,
    window: &str,
    start: &Sample,
    end: &Sample,
) -> Result<(), String> {
    let delta = (end.used_percent - start.used_percent).max(0.0);
    if delta <= 0.0 {
        return Ok(());
    }
    // Quota percentage is account/window-level official usage. There is no
    // direct category-level observation here, so never manufacture a model
    // credit amount by weighting it with local Token deltas.
    let local_credits: Option<f64> = None;
    let unattributed_percent = Some(delta);
    let confidence = Confidence::Low;
    let gap = observation_gap(connection, account_key, start.sampled_at, end.sampled_at)?;
    let confidence = match gap.quality {
        "exact" => confidence,
        "bounded_gap" => Confidence::Low,
        _ => Confidence::Unknown,
    };

    connection
        .execute(
            "INSERT INTO quota_intervals
             (account_key, limit_id, window, window_duration_mins, cycle_id,
              start_sample_id, end_sample_id, start_at, end_at, start_percent,
              end_percent, observed_delta_percent, local_weighted_credits,
              unattributed_percent, sample_quality, observation_gap_ms,
              gap_token_count, rejection_reason, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                account_key,
                limit_id,
                window,
                end.window_duration_mins,
                cycle_id(end.resets_at),
                start.id,
                end.id,
                start.sampled_at,
                end.sampled_at,
                start.used_percent,
                end.used_percent,
                delta,
                local_credits,
                unattributed_percent,
                gap.quality,
                gap.duration_ms,
                gap.token_count,
                Option::<String>::None,
                confidence_string(&confidence),
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Rebuild all derived quota steps from the immutable raw sample table.
pub fn rebuild_all_intervals(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT account_key, limit_id, window
             FROM rate_limit_samples",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let buckets = rows
        .map(|row| row.map_err(|error| error.to_string()))
        .filter(|row| {
            row.as_ref()
                .map(|(account_key, _, _)| !is_unresolved_account_key(account_key))
                .unwrap_or(true)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (account_key, limit_id, window) in buckets {
        refresh_intervals(connection, &account_key, &limit_id, &window)?;
    }
    Ok(())
}

/// Rebuild derived quota steps for one account. Account-scoped rollout
/// migrations must not mutate another account's derived attribution.
pub fn rebuild_account_intervals(connection: &Connection, account_key: &str) -> Result<(), String> {
    if is_unresolved_account_key(account_key) {
        return Ok(());
    }
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT limit_id, window
             FROM rate_limit_samples
             WHERE account_key = ?1",
        )
        .map_err(|error| error.to_string())?;
    let buckets = statement
        .query_map([account_key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (limit_id, window) in buckets {
        refresh_intervals(connection, account_key, &limit_id, &window)?;
    }
    Ok(())
}

pub fn confidence_string(confidence: &Confidence) -> &'static str {
    match confidence {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
        Confidence::Unknown => "unknown",
    }
}

pub fn confidence_from_str(value: &str) -> Confidence {
    match value {
        "high" => Confidence::High,
        "medium" => Confidence::Medium,
        "low" => Confidence::Low,
        _ => Confidence::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::db::initialize_schema;

    #[test]
    fn reset_does_not_create_negative_interval() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for (timestamp, used, reset) in [(1, 98.0, 10), (2, 2.0, 20), (3, 5.0, 20), (4, 8.0, 20)] {
            connection
                .execute(
                    "INSERT INTO rate_limit_samples
                     (account_key, sampled_at, limit_id, window, window_duration_mins,
                      used_percent, resets_at, source, confidence)
                     VALUES ('a', ?1, 'codex', 'primary', 10080, ?2, ?3, 'official', 'high')",
                    params![timestamp, used, reset],
                )
                .unwrap();
        }
        refresh_intervals(&connection, "a", "codex", "primary").unwrap();
        let interval: f64 = connection
            .query_row(
                "SELECT observed_delta_percent FROM quota_intervals",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(interval, 3.0);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM quota_intervals", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn reset_timestamp_jitter_keeps_the_same_cycle() {
        assert!(same_reset_at(Some(100), Some(104)));
        assert!(!same_reset_at(Some(100), Some(106)));
    }

    #[test]
    fn interval_records_gap_quality_and_tokens_without_splitting_quota_delta() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO collector_gaps
                 (session_id, start_at, end_at, duration_ms, reason, created_at)
                 VALUES ('s', 2, 3, 1000, 'os_sleep', 4)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, reasoning_effort,
                  speed_mode, sampled_at, cumulative_tokens, delta_tokens, source, confidence)
                 VALUES ('a', 'thread', 'turn', 0, 'medium', 'standard', 3, 7, 7,
                         'local_rollout', 'high')",
                [],
            )
            .unwrap();
        for (timestamp, used) in [(1, 10.0), (2, 11.0), (3, 11.0), (4, 12.0)] {
            connection
                .execute(
                    "INSERT INTO rate_limit_samples
                     (account_key, sampled_at, limit_id, window, window_duration_mins,
                      used_percent, resets_at, source, confidence)
                     VALUES ('a', ?1, 'codex', 'primary', 10080, ?2, 100, 'official', 'high')",
                    params![timestamp, used],
                )
                .unwrap();
        }
        refresh_intervals(&connection, "a", "codex", "primary").unwrap();
        let row: (String, i64, i64, String, String) = connection
            .query_row(
                "SELECT sample_quality, observation_gap_ms, gap_token_count,
                        confidence, rejection_reason
                 FROM quota_intervals",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            ("bounded_gap".into(), 1000, 7, "low".into(), "".into())
        );
    }

    #[test]
    fn quota_step_contains_the_plateau_between_transitions() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for (timestamp, used) in [(1, 22.0), (2, 23.0), (3, 23.0), (4, 23.0), (5, 24.0)] {
            connection
                .execute(
                    "INSERT INTO rate_limit_samples
                     (account_key, sampled_at, limit_id, window, window_duration_mins,
                      used_percent, resets_at, source, confidence)
                     VALUES ('a', ?1, 'codex', 'primary', 10080, ?2, 100, 'official', 'high')",
                    params![timestamp, used],
                )
                .unwrap();
        }
        refresh_intervals(&connection, "a", "codex", "primary").unwrap();
        let step: (i64, i64, f64) = connection
            .query_row(
                "SELECT start_at, end_at, observed_delta_percent FROM quota_intervals",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(step, (2, 5, 1.0));
    }

    #[test]
    fn quota_delta_is_not_proportionally_assigned_to_local_credits() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, completed_at,
                  reasoning_effort, speed_mode, raw_total_tokens, estimated_credits,
                  source, confidence, created_at, updated_at)
                 VALUES ('a', 'thread', 'long', 1, 4, 'high', 'standard',
                         100, 10.0, 'rollout', 'high', 1, 4)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, segment_no, reasoning_effort,
                  speed_mode, sampled_at, cumulative_tokens, delta_tokens,
                  source, confidence)
                 VALUES
                   ('a', 'thread', 'long', 0, 'high', 'standard', 1, 40, 40,
                    'rollout', 'high'),
                   ('a', 'thread', 'long', 0, 'high', 'standard', 3, 100, 60,
                    'rollout', 'high')",
                [],
            )
            .unwrap();
        for (timestamp, used) in [(1, 22.0), (2, 23.0), (3, 23.0), (4, 24.0)] {
            connection
                .execute(
                    "INSERT INTO rate_limit_samples
                     (account_key, sampled_at, limit_id, window, window_duration_mins,
                      used_percent, resets_at, source, confidence)
                     VALUES ('a', ?1, 'codex', 'primary', 10080, ?2, 100,
                             'official', 'high')",
                    params![timestamp, used],
                )
                .unwrap();
        }

        refresh_intervals(&connection, "a", "codex", "primary").unwrap();
        let local_credits: Option<f64> = connection
            .query_row(
                "SELECT local_weighted_credits FROM quota_intervals",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(local_credits, None);
        let unattributed: f64 = connection
            .query_row(
                "SELECT unattributed_percent FROM quota_intervals",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unattributed, 1.0);
    }
}
