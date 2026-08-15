use rusqlite::{params, Connection};

use super::models::Confidence;

#[derive(Clone, Debug)]
struct Sample {
    sampled_at: i64,
    used_percent: f64,
    resets_at: Option<i64>,
}

/// Rebuilds intervals for one bucket from monotonic samples. A drop in used
/// percent or a reset timestamp change starts a new cycle; it is never emitted
/// as a negative usage interval.
pub fn refresh_intervals(
    connection: &Connection,
    account_key: &str,
    limit_id: &str,
    window: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(
            "SELECT sampled_at, used_percent, resets_at
             FROM rate_limit_samples
             WHERE account_key = ?1 AND limit_id = ?2 AND window = ?3
             ORDER BY sampled_at ASC, id ASC",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, limit_id, window], |row| {
            Ok(Sample {
                sampled_at: row.get(0)?,
                used_percent: row.get(1)?,
                resets_at: row.get(2)?,
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

    for pair in samples.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if current.sampled_at <= previous.sampled_at
            || current.used_percent < previous.used_percent
            || previous.resets_at != current.resets_at
        {
            continue;
        }

        let delta = (current.used_percent - previous.used_percent).max(0.0);
        if delta <= 0.0 {
            continue;
        }

        let local_credits: Option<f64> = connection
            .query_row(
                "SELECT SUM(estimated_credits) FROM turn_usage
                 WHERE account_key = ?1 AND started_at > ?2 AND started_at <= ?3",
                params![account_key, previous.sampled_at, current.sampled_at],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let confidence = if local_credits.unwrap_or(0.0) > 0.0 {
            Confidence::Medium
        } else {
            Confidence::Low
        };
        let unattributed = if local_credits.unwrap_or(0.0) > 0.0 {
            Some(0.0)
        } else {
            Some(delta)
        };

        connection
            .execute(
                "INSERT INTO quota_intervals
                 (account_key, limit_id, window, start_at, end_at, start_percent,
                  end_percent, observed_delta_percent, local_weighted_credits,
                  unattributed_percent, confidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    account_key,
                    limit_id,
                    window,
                    previous.sampled_at,
                    current.sampled_at,
                    previous.used_percent,
                    current.used_percent,
                    delta,
                    local_credits,
                    unattributed,
                    confidence_string(&confidence),
                ],
            )
            .map_err(|error| error.to_string())?;
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
        for (timestamp, used, reset) in [(1, 98.0, 10), (2, 2.0, 20), (3, 5.0, 20)] {
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
    }
}
