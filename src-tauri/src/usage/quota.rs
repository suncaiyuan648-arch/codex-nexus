use rusqlite::{params, Connection};

use super::models::Confidence;

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
    let local_credits: Option<f64> = connection
        .query_row(
            "SELECT SUM(estimated_credits) FROM turn_usage
             WHERE account_key = ?1 AND started_at > ?2 AND started_at <= ?3",
            params![account_key, start.sampled_at, end.sampled_at],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let confidence = if local_credits.unwrap_or(0.0) > 0.0 {
        Confidence::Medium
    } else {
        Confidence::Low
    };

    connection
        .execute(
            "INSERT INTO quota_intervals
             (account_key, limit_id, window, window_duration_mins, cycle_id,
              start_sample_id, end_sample_id, start_at, end_at, start_percent,
              end_percent, observed_delta_percent, local_weighted_credits,
              unattributed_percent, sample_quality, rejection_reason, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17)",
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
                if local_credits.unwrap_or(0.0) > 0.0 {
                    Some(0.0)
                } else {
                    Some(delta)
                },
                "quota_step",
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
        .collect::<Result<Vec<_>, _>>()?;
    for (account_key, limit_id, window) in buckets {
        refresh_intervals(connection, &account_key, &limit_id, &window)?;
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
}
