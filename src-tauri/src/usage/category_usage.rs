use chrono::{DateTime, Local, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::models::{
    CategoryTokenEstimate, CategoryUsage, CategoryUsageItem, CategoryUsageQuotaWindow, Confidence,
    TokenUsageMetric, UsageMetric, PROVENANCE_ACCOUNT_RATE_LIMIT, PROVENANCE_DERIVED_ESTIMATE,
    PROVENANCE_LOCAL_ROLLOUT, USAGE_STATUS_ESTIMATED, USAGE_STATUS_INSUFFICIENT_DATA,
    USAGE_STATUS_OBSERVED,
};
use super::{db, quota, recorder};

#[derive(Clone, Debug, Default)]
struct LocalAggregate {
    tokens: i64,
    turn_count: i64,
}

#[derive(Clone, Debug, Default)]
struct ServerAggregate {
    credits_micros: i64,
    known: bool,
}

type CategoryKey = (String, String, String);

fn local_offset() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(Local::now().offset().local_minus_utc())
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap())
}

fn local_day_bounds(now: DateTime<Local>) -> (i64, i64) {
    let date = now.date_naive();
    let offset = local_offset();
    let start = offset
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp();
    let end = offset
        .from_local_datetime(&date.succ_opt().unwrap().and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp();
    (start, end)
}

fn latest_quota_window(
    connection: &Connection,
    account_key: Option<&str>,
) -> Result<Option<(i64, i64, CategoryUsageQuotaWindow)>, String> {
    let Some(account_key) = account_key else {
        return Ok(None);
    };
    let row = connection
        .query_row(
            "SELECT sampled_at, limit_id, window, window_duration_mins, used_percent, resets_at
             FROM rate_limit_samples
             WHERE account_key = ?1 AND window_duration_mins = 10080
             ORDER BY sampled_at DESC LIMIT 1",
            params![account_key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;

    let Some((_sampled_at, limit_id, window, duration, used_percent, resets_at)) = row else {
        return Ok(None);
    };
    let Some(resets_at) = resets_at else {
        // Without resetsAt, windowDurationMins alone cannot identify the
        // current Codex quota cycle. Keep the weekly view explicitly in its
        // insufficient/fallback state instead of inventing a cycle boundary.
        return Ok(None);
    };
    let duration_seconds = duration.max(1).saturating_mul(60);
    let end = resets_at;
    let start = end.saturating_sub(duration_seconds);
    let used_percent = used_percent.clamp(0.0, 100.0);
    Ok(Some((
        start,
        end,
        CategoryUsageQuotaWindow {
            limit_id,
            window,
            used_percent,
            remaining_percent: (100.0 - used_percent).max(0.0),
            window_duration_mins: duration,
            resets_at: Some(resets_at),
        },
    )))
}

fn local_category_usage(
    connection: &Connection,
    account_key: Option<&str>,
    start: i64,
    end: i64,
) -> Result<(BTreeMap<CategoryKey, LocalAggregate>, i64), String> {
    let Some(account_key) = account_key else {
        return Ok((BTreeMap::new(), 0));
    };
    let mut statement = connection
        .prepare(
            "SELECT COALESCE(model, 'unknown'), reasoning_effort, speed_mode,
                    raw_total_tokens
             FROM turn_usage
             WHERE account_key = ?1 AND started_at >= ?2 AND started_at < ?3",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, start, end], |row| {
            Ok((
                (
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ),
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut categories: BTreeMap<CategoryKey, LocalAggregate> = BTreeMap::new();
    let mut total: i64 = 0;
    for row in rows {
        let (key, tokens) = row.map_err(|error| error.to_string())?;
        let entry = categories.entry(key).or_default();
        entry.tokens = entry.tokens.saturating_add(tokens.max(0));
        entry.turn_count += 1;
        total = total.saturating_add(tokens.max(0));
    }
    Ok((categories, total))
}

#[derive(Clone, Debug)]
struct RateSample {
    sampled_at: i64,
    used_percent: f64,
    resets_at: Option<i64>,
}

#[derive(Clone, Debug)]
struct QuotaStep {
    start_at: i64,
    end_at: i64,
    observed_delta_percent: f64,
}

#[derive(Clone, Debug, Default)]
struct CategoryEstimateAccumulator {
    observed_sample_count: i64,
    valid_sample_count: i64,
    observed_tokens: i64,
    observed_quota_percent: f64,
    pending_tokens: i64,
    rejected_sample_count: i64,
    boundary_overlap_count: i64,
    rates: Vec<f64>,
}

fn quota_steps(
    connection: &Connection,
    account_key: Option<&str>,
    limit_id: &str,
    window: &str,
    start: i64,
    end: i64,
) -> Result<Vec<QuotaStep>, String> {
    let Some(account_key) = account_key else {
        return Ok(Vec::new());
    };
    let mut statement = connection
        .prepare(
            "SELECT start_at, end_at, observed_delta_percent
             FROM quota_intervals
             WHERE account_key = ?1 AND limit_id = ?2 AND window = ?3
               AND window_duration_mins = 10080
               AND start_at >= ?4 AND end_at <= ?5
             ORDER BY end_at ASC, id ASC",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, limit_id, window, start, end], |row| {
            Ok(QuotaStep {
                start_at: row.get(0)?,
                end_at: row.get(1)?,
                observed_delta_percent: row.get(2)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.map(|row| row.map_err(|error| error.to_string()))
        .collect()
}

fn interval_category_usage(
    connection: &Connection,
    account_key: &str,
    start: i64,
    end: i64,
) -> Result<(BTreeMap<CategoryKey, LocalAggregate>, i64), String> {
    let mut statement = connection
        .prepare(
            "SELECT COALESCE(model, 'unknown'), reasoning_effort, speed_mode,
                    raw_total_tokens
             FROM turn_usage
             WHERE account_key = ?1 AND started_at > ?2 AND started_at <= ?3",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, start, end], |row| {
            Ok((
                (
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ),
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut categories: BTreeMap<CategoryKey, LocalAggregate> = BTreeMap::new();
    let mut total: i64 = 0;
    for row in rows {
        let (key, tokens) = row.map_err(|error| error.to_string())?;
        let tokens = tokens.max(0);
        let entry = categories.entry(key).or_default();
        entry.tokens = entry.tokens.saturating_add(tokens);
        entry.turn_count += 1;
        total = total.saturating_add(tokens);
    }
    Ok((categories, total))
}

fn has_boundary_overlap(
    connection: &Connection,
    account_key: &str,
    start: i64,
    end: i64,
) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM turn_usage
               WHERE account_key = ?1
                 AND (
                   (started_at < ?2 AND COALESCE(completed_at, ?3 + 1) > ?2)
                   OR
                   (started_at < ?3 AND COALESCE(completed_at, ?3 + 1) > ?3)
                 )
             )",
            params![account_key, start, end],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(if sorted.len() % 2 == 0 {
        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
    } else {
        sorted[sorted.len() / 2]
    })
}

fn median_absolute_deviation(values: &[f64], center: f64) -> f64 {
    let deviations = values
        .iter()
        .map(|value| (value - center).abs())
        .collect::<Vec<_>>();
    median(&deviations).unwrap_or(0.0)
}

fn estimate_weekly_categories(
    connection: &Connection,
    account_key: Option<&str>,
    start: i64,
    end: i64,
    quota_window: Option<&CategoryUsageQuotaWindow>,
    categories: &BTreeMap<CategoryKey, LocalAggregate>,
) -> Result<BTreeMap<CategoryKey, CategoryTokenEstimate>, String> {
    const MIN_VALID_SAMPLES: i64 = 5;
    const MIN_OBSERVED_TOKENS: i64 = 1_000;
    const MIN_OBSERVED_QUOTA_PERCENT: f64 = 5.0;
    const MIN_COVERAGE_RATIO: f64 = 0.60;
    const MAX_DISPERSION_RATIO: f64 = 0.75;

    let mut accumulators: BTreeMap<CategoryKey, CategoryEstimateAccumulator> = categories
        .keys()
        .cloned()
        .map(|key| (key, CategoryEstimateAccumulator::default()))
        .collect();

    let Some(quota_window) = quota_window else {
        return Ok(accumulators
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    insufficient_estimate(value, 0.0, 0.0, PROVENANCE_DERIVED_ESTIMATE),
                )
            })
            .collect());
    };

    let steps = quota_steps(
        connection,
        account_key,
        &quota_window.limit_id,
        &quota_window.window,
        start,
        end,
    )?;
    let mut last_closed_step_end = None;
    for step in &steps {
        last_closed_step_end = Some(step.end_at);
        for value in accumulators.values_mut() {
            value.observed_sample_count += 1;
        }
        let Some(account_key) = account_key else {
            continue;
        };
        let (interval_categories, _) =
            interval_category_usage(connection, account_key, step.start_at, step.end_at)?;
        let boundary_overlap =
            has_boundary_overlap(connection, account_key, step.start_at, step.end_at)?;
        if boundary_overlap {
            for value in accumulators.values_mut() {
                value.rejected_sample_count += 1;
                value.boundary_overlap_count += 1;
            }
            continue;
        }
        let positive_categories = interval_categories
            .into_iter()
            .filter(|(_, value)| value.tokens > 0)
            .collect::<Vec<_>>();
        if positive_categories.len() != 1 {
            for value in accumulators.values_mut() {
                value.rejected_sample_count += 1;
            }
            continue;
        }
        let (key, value) = positive_categories.into_iter().next().unwrap();
        let Some(accumulator) = accumulators.get_mut(&key) else {
            continue;
        };
        accumulator.valid_sample_count += 1;
        accumulator.observed_tokens = accumulator
            .observed_tokens
            .saturating_add(value.tokens.max(0));
        accumulator.observed_quota_percent += step.observed_delta_percent;
        accumulator
            .rates
            .push(value.tokens.max(0) as f64 / step.observed_delta_percent.max(f64::EPSILON));
    }

    if let Some(account_key) = account_key {
        let pending_start = last_closed_step_end.unwrap_or(start);
        if pending_start < end {
            let (pending_categories, _) =
                interval_category_usage(connection, account_key, pending_start, end)?;
            for (key, value) in pending_categories {
                if let Some(accumulator) = accumulators.get_mut(&key) {
                    accumulator.pending_tokens = accumulator
                        .pending_tokens
                        .saturating_add(value.tokens.max(0));
                }
            }
        }
    }

    Ok(accumulators
        .into_iter()
        .map(|(key, value)| {
            let current_tokens = categories.get(&key).map(|item| item.tokens).unwrap_or(0);
            let coverage_ratio = if current_tokens > 0 {
                (value.observed_tokens as f64 / current_tokens as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let median_rate = median(&value.rates).unwrap_or(0.0);
            let dispersion = if median_rate > 0.0 {
                median_absolute_deviation(&value.rates, median_rate) / median_rate
            } else {
                f64::INFINITY
            };
            let full_week_tokens = (median_rate * 100.0).round().clamp(0.0, i64::MAX as f64) as i64;
            let current_quota_lower_bound =
                full_week_tokens as f64 * quota_window.used_percent.clamp(0.0, 100.0) / 100.0;
            let estimate = if value.valid_sample_count >= MIN_VALID_SAMPLES
                && value.observed_tokens >= MIN_OBSERVED_TOKENS
                && value.observed_quota_percent >= MIN_OBSERVED_QUOTA_PERCENT
                && coverage_ratio >= MIN_COVERAGE_RATIO
                && full_week_tokens >= current_tokens
                && current_quota_lower_bound + 1.0 >= current_tokens as f64
                && dispersion <= MAX_DISPERSION_RATIO
            {
                let remaining_tokens =
                    ((full_week_tokens as f64 * quota_window.remaining_percent.max(0.0)) / 100.0)
                        .round()
                        .clamp(0.0, i64::MAX as f64) as i64;
                CategoryTokenEstimate {
                    status: USAGE_STATUS_ESTIMATED.into(),
                    estimated_tokens: Some(full_week_tokens),
                    remaining_tokens: Some(remaining_tokens),
                    observed_sample_count: value.observed_sample_count,
                    valid_sample_count: value.valid_sample_count,
                    observed_tokens: value.observed_tokens,
                    observed_quota_percent: value.observed_quota_percent,
                    cumulative_observed_quota_delta: value.observed_quota_percent,
                    coverage_ratio,
                    pending_tokens: value.pending_tokens,
                    rejected_sample_count: value.rejected_sample_count,
                    boundary_overlap_ratio: if value.observed_sample_count > 0 {
                        value.boundary_overlap_count as f64 / value.observed_sample_count as f64
                    } else {
                        0.0
                    },
                    dispersion_ratio: dispersion,
                    external_usage_risk: coverage_ratio < 1.0,
                    confidence: if coverage_ratio >= 0.85 && dispersion <= 0.35 {
                        Confidence::High
                    } else {
                        Confidence::Medium
                    },
                    source: PROVENANCE_DERIVED_ESTIMATE.into(),
                }
            } else {
                insufficient_estimate(
                    value,
                    coverage_ratio,
                    dispersion,
                    PROVENANCE_DERIVED_ESTIMATE,
                )
            };
            (key, estimate)
        })
        .collect())
}

fn insufficient_estimate(
    value: CategoryEstimateAccumulator,
    coverage_ratio: f64,
    dispersion: f64,
    source: &str,
) -> CategoryTokenEstimate {
    CategoryTokenEstimate {
        status: USAGE_STATUS_INSUFFICIENT_DATA.into(),
        estimated_tokens: None,
        remaining_tokens: None,
        observed_sample_count: value.observed_sample_count,
        valid_sample_count: value.valid_sample_count,
        observed_tokens: value.observed_tokens,
        observed_quota_percent: value.observed_quota_percent,
        cumulative_observed_quota_delta: value.observed_quota_percent,
        coverage_ratio,
        pending_tokens: value.pending_tokens,
        rejected_sample_count: value.rejected_sample_count,
        boundary_overlap_ratio: if value.observed_sample_count > 0 {
            value.boundary_overlap_count as f64 / value.observed_sample_count as f64
        } else {
            0.0
        },
        dispersion_ratio: dispersion,
        external_usage_risk: coverage_ratio < 1.0 || value.pending_tokens > 0,
        confidence: Confidence::Low,
        source: source.into(),
    }
}

fn daily_quota_usage(
    connection: &Connection,
    account_key: Option<&str>,
    start: i64,
    end: i64,
    quota_window: Option<&CategoryUsageQuotaWindow>,
) -> UsageMetric {
    let Some(account_key) = account_key else {
        return insufficient_quota_metric();
    };
    let Some(quota_window) = quota_window else {
        return insufficient_quota_metric();
    };
    let baseline_start = start.saturating_sub(24 * 60 * 60);
    let mut statement = match connection.prepare(
        "SELECT sampled_at, used_percent, resets_at
         FROM rate_limit_samples
         WHERE account_key = ?1 AND limit_id = ?2 AND window = ?3
           AND window_duration_mins = 10080
           AND sampled_at >= ?4 AND sampled_at < ?5
         ORDER BY sampled_at ASC, id ASC",
    ) {
        Ok(statement) => statement,
        Err(_) => return insufficient_quota_metric(),
    };
    let rows = match statement.query_map(
        params![
            account_key,
            quota_window.limit_id,
            quota_window.window,
            baseline_start,
            end
        ],
        |row| {
            Ok(RateSample {
                sampled_at: row.get(0)?,
                used_percent: row.get(1)?,
                resets_at: row.get(2)?,
            })
        },
    ) {
        Ok(rows) => rows,
        Err(_) => return insufficient_quota_metric(),
    };
    let samples = rows.filter_map(Result::ok).collect::<Vec<_>>();
    let mut observed_percent = 0.0;
    let mut sample_count = 0;
    for pair in samples.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if current.sampled_at < start
            || current.sampled_at >= end
            || current.sampled_at <= previous.sampled_at
            || current.used_percent < previous.used_percent
            || !quota::same_reset_at(previous.resets_at, current.resets_at)
        {
            continue;
        }
        sample_count += 1;
        observed_percent += current.used_percent - previous.used_percent;
    }
    if sample_count == 0 {
        return insufficient_quota_metric();
    }
    UsageMetric {
        status: USAGE_STATUS_OBSERVED.into(),
        value: Some(observed_percent),
        sample_count,
        confidence: if sample_count >= 3 {
            Confidence::High
        } else {
            Confidence::Medium
        },
        source: PROVENANCE_ACCOUNT_RATE_LIMIT.into(),
    }
}

fn insufficient_quota_metric() -> UsageMetric {
    UsageMetric {
        status: USAGE_STATUS_INSUFFICIENT_DATA.into(),
        value: None,
        sample_count: 0,
        confidence: Confidence::Low,
        source: PROVENANCE_ACCOUNT_RATE_LIMIT.into(),
    }
}

fn server_category_deltas(
    connection: &Connection,
    account_key: Option<&str>,
    start: i64,
    end: i64,
) -> Result<(BTreeMap<CategoryKey, ServerAggregate>, bool), String> {
    let Some(account_key) = account_key else {
        return Ok((BTreeMap::new(), false));
    };
    let mut statement = connection
        .prepare(
            "SELECT id, thread_id, sampled_at, COALESCE(model, 'unknown'),
                    reasoning_effort, speed_mode, estimated_usage_credits_micros
             FROM thread_usage_group_samples
             WHERE account_key = ?1 AND sampled_at < ?2
             ORDER BY thread_id, sampled_at, id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, end], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                (
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ),
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut previous: HashMap<(String, CategoryKey), i64> = HashMap::new();
    let mut deltas: BTreeMap<CategoryKey, ServerAggregate> = BTreeMap::new();
    let mut known = HashSet::new();
    for row in rows {
        let (_, thread_id, sampled_at, key, credits) = row.map_err(|error| error.to_string())?;
        let previous_value = previous.insert((thread_id, key.clone()), credits.max(0));
        let Some(previous_value) = previous_value else {
            continue;
        };
        if !(start..end).contains(&sampled_at) {
            continue;
        }
        let delta = if credits >= previous_value {
            credits - previous_value
        } else {
            // A reset or server correction is not a negative usage event. The
            // new cumulative value is the only non-negative observation we
            // can safely retain.
            credits.max(0)
        };
        let entry = deltas.entry(key.clone()).or_default();
        entry.credits_micros = entry.credits_micros.saturating_add(delta);
        entry.known = true;
        known.insert(key);
    }
    Ok((deltas, !known.is_empty()))
}

fn official_tokens(
    connection: &Connection,
    account_key: Option<&str>,
    start: i64,
    end: i64,
) -> Result<Option<i64>, String> {
    let Some(account_key) = account_key else {
        return Ok(None);
    };
    let start_date = DateTime::from_timestamp(start, 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
        .with_timezone(&Local)
        .date_naive()
        .to_string();
    let end_date = DateTime::from_timestamp(end.saturating_sub(1), 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
        .with_timezone(&Local)
        .date_naive()
        .to_string();
    connection
        .query_row(
            "SELECT SUM(official_tokens) FROM account_daily_usage
             WHERE account_key = ?1 AND date >= ?2 AND date <= ?3",
            params![account_key, start_date, end_date],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map(|value| value.map(|tokens| tokens.max(0)))
        .map_err(|error| error.to_string())
}

fn server_capability(connection: &Connection, account_key: Option<&str>) -> Result<String, String> {
    let Some(account_key) = account_key else {
        return Ok("syncing".into());
    };
    let states = connection
        .query_row(
            "SELECT
               COALESCE(SUM(CASE WHEN capability = 'available' THEN 1 ELSE 0 END), 0),
               COALESCE(SUM(CASE WHEN capability = 'unavailable' THEN 1 ELSE 0 END), 0),
               COUNT(*)
             FROM thread_usage_capabilities WHERE account_key = ?1",
            params![account_key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|error| error.to_string())?;
    if states.0 > 0 {
        Ok("available".into())
    } else if states.1 > 0 {
        Ok("unavailable".into())
    } else {
        Ok("syncing".into())
    }
}

pub fn category_usage(connection: &Connection, period: &str) -> Result<CategoryUsage, String> {
    let account_key = recorder::current_account_key(connection)?;
    let now = Local::now();
    let (start, end, period_source, quota_window) = match period {
        "day" => {
            let (start, end) = local_day_bounds(now);
            (start, end, "local_day".to_string(), None)
        }
        "quota_week" => match latest_quota_window(connection, account_key.as_deref())? {
            Some((start, end, quota)) => (start, end, "quota_window".to_string(), Some(quota)),
            None => {
                let now = now.timestamp();
                // Do not substitute a natural week or rolling seven days:
                // the requested weekly view is only valid when the current
                // Codex window can be derived from duration + resetsAt.
                (now, now, "insufficient_data".to_string(), None)
            }
        },
        _ => return Err(format!("Unknown usage period: {period}")),
    };

    let (local, local_total) =
        local_category_usage(connection, account_key.as_deref(), start, end)?;
    let (server, _) = server_category_deltas(connection, account_key.as_deref(), start, end)?;
    let capability = server_capability(connection, account_key.as_deref())?;
    // The homepage only lists categories backed by real local Turn records.
    // Server-only groups may be useful for diagnostics, but without a local
    // Token/Turn record they must not create a fabricated category row.
    let keys: BTreeSet<CategoryKey> = local.keys().cloned().collect();
    let weekly_estimates = if period == "quota_week" {
        estimate_weekly_categories(
            connection,
            account_key.as_deref(),
            start,
            end,
            quota_window.as_ref(),
            &local,
        )?
    } else {
        BTreeMap::new()
    };
    let mut categories = keys
        .into_iter()
        .map(|key| {
            let local_value = local.get(&key).cloned().unwrap_or_default();
            let server_value = server.get(&key).cloned().unwrap_or_default();
            let weekly_estimate = weekly_estimates.get(&key).cloned();
            CategoryUsageItem {
                model: key.0,
                reasoning_effort: key.1,
                fast: key.2 == "fast_requested",
                speed_mode: key.2,
                turn_count: local_value.turn_count,
                tokens: local_value.tokens,
                token_source: PROVENANCE_LOCAL_ROLLOUT.into(),
                server_estimated_credits_micros: server_value
                    .known
                    .then_some(server_value.credits_micros),
                credit_source: if server_value.known {
                    "app_server".into()
                } else {
                    "unavailable".into()
                },
                weekly_quota_percent: None,
                weekly_estimate,
            }
        })
        .collect::<Vec<_>>();
    categories.sort_by(|left, right| {
        right
            .tokens
            .cmp(&left.tokens)
            .then_with(|| left.model.cmp(&right.model))
            .then_with(|| left.reasoning_effort.cmp(&right.reasoning_effort))
            .then_with(|| left.speed_mode.cmp(&right.speed_mode))
    });

    let token_usage = TokenUsageMetric {
        status: USAGE_STATUS_OBSERVED.into(),
        value_tokens: local_total,
        sample_count: categories.iter().map(|category| category.turn_count).sum(),
        confidence: if local_total > 0 {
            Confidence::High
        } else {
            Confidence::Unknown
        },
        source: PROVENANCE_LOCAL_ROLLOUT.into(),
    };
    let daily_quota_window = if period == "day" {
        latest_quota_window(connection, account_key.as_deref())?.map(|(_, _, quota)| quota)
    } else {
        None
    };
    let quota_usage = if period == "day" {
        Some(daily_quota_usage(
            connection,
            account_key.as_deref(),
            start,
            end,
            daily_quota_window.as_ref(),
        ))
    } else {
        None
    };

    Ok(CategoryUsage {
        period: period.into(),
        period_start: start,
        period_end: end,
        period_source,
        account_key: account_key.clone(),
        official_tokens: official_tokens(connection, account_key.as_deref(), start, end)?,
        local_tokens: local_total,
        server_usage_capability: capability,
        quota_window,
        quota_usage,
        token_usage,
        categories,
    })
}

pub fn app_category_usage(
    app: &tauri::AppHandle<tauri::Wry>,
    period: &str,
) -> Result<CategoryUsage, String> {
    let connection = db::open_database(app)?;
    category_usage(&connection, period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::db::initialize_schema;

    #[test]
    fn server_credits_are_read_as_snapshot_deltas() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO accounts (account_key, first_seen_at, last_seen_at)
                 VALUES ('a', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO thread_usage_group_samples
                 (account_key, thread_id, sampled_at, model, reasoning_effort, speed_mode,
                  estimated_usage_credits_micros, source, confidence)
                 VALUES
                   ('a', 'thread', 100, 'gpt-5.6-sol', 'high', 'standard', 100, 'app_server_thread_usage', 'high'),
                   ('a', 'thread', 200, 'gpt-5.6-sol', 'high', 'standard', 250, 'app_server_thread_usage', 'high')",
                [],
            )
            .unwrap();

        let (categories, available) =
            server_category_deltas(&connection, Some("a"), 150, 250).unwrap();
        let value = categories
            .get(&("gpt-5.6-sol".into(), "high".into(), "standard".into()))
            .unwrap();
        assert!(available);
        assert!(value.known);
        assert_eq!(value.credits_micros, 150);
    }

    #[test]
    fn missing_snapshot_baseline_does_not_claim_period_credits() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO thread_usage_group_samples
                 (account_key, thread_id, sampled_at, model, reasoning_effort, speed_mode,
                  estimated_usage_credits_micros, source, confidence)
                 VALUES ('a', 'thread', 200, 'gpt-5.6-sol', 'high', 'standard', 250,
                         'app_server_thread_usage', 'high')",
                [],
            )
            .unwrap();
        let (categories, available) =
            server_category_deltas(&connection, Some("a"), 150, 250).unwrap();
        assert!(!available);
        assert!(categories.is_empty());
    }

    #[test]
    fn weekly_token_estimate_requires_observed_quota_deltas() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO rate_limit_samples
                 (account_key, sampled_at, limit_id, window, window_duration_mins,
                  used_percent, resets_at, source, confidence)
                 VALUES
                   ('a', 1000, 'weekly', 'primary', 10080, 10, 20000, 'account_rate_limit', 'high'),
                   ('a', 2000, 'weekly', 'primary', 10080, 11, 20000, 'account_rate_limit', 'high'),
                   ('a', 3000, 'weekly', 'primary', 10080, 12, 20000, 'account_rate_limit', 'high'),
                   ('a', 4000, 'weekly', 'primary', 10080, 13, 20000, 'account_rate_limit', 'high'),
                   ('a', 5000, 'weekly', 'primary', 10080, 14, 20000, 'account_rate_limit', 'high'),
                   ('a', 6000, 'weekly', 'primary', 10080, 15, 20000, 'account_rate_limit', 'high'),
                   ('a', 7000, 'weekly', 'primary', 10080, 16, 20000, 'account_rate_limit', 'high')",
                [],
            )
            .unwrap();
        for (turn_id, started_at) in [
            ("turn-1", 2500),
            ("turn-2", 3500),
            ("turn-3", 4500),
            ("turn-4", 5500),
            ("turn-5", 6500),
        ] {
            connection
                .execute(
                    "INSERT INTO turn_usage
                     (account_key, thread_id, turn_id, started_at, completed_at, model, reasoning_effort,
                      speed_mode, raw_total_tokens, source, confidence, created_at, updated_at)
                     VALUES ('a', 'thread', ?1, ?2, ?2, 'gpt-5.6-sol', 'high', 'standard',
                             2000, 'local_rollout', 'high', ?2, ?2)",
                    params![turn_id, started_at],
                )
                .unwrap();
        }
        quota::refresh_intervals(&connection, "a", "weekly", "primary").unwrap();
        let mut categories = BTreeMap::new();
        categories.insert(
            ("gpt-5.6-sol".into(), "high".into(), "standard".into()),
            LocalAggregate {
                tokens: 10_000,
                turn_count: 5,
            },
        );
        let quota_window = CategoryUsageQuotaWindow {
            limit_id: "weekly".into(),
            window: "primary".into(),
            used_percent: 16.0,
            remaining_percent: 84.0,
            window_duration_mins: 10080,
            resets_at: Some(20000),
        };
        let estimate = estimate_weekly_categories(
            &connection,
            Some("a"),
            1000,
            10000,
            Some(&quota_window),
            &categories,
        )
        .unwrap()
        .remove(&("gpt-5.6-sol".into(), "high".into(), "standard".into()))
        .unwrap();
        assert_eq!(estimate.status, USAGE_STATUS_ESTIMATED);
        assert_eq!(estimate.estimated_tokens, Some(200_000));
        assert_eq!(estimate.remaining_tokens, Some(168_000));
        assert_eq!(estimate.valid_sample_count, 5);
        assert_eq!(estimate.coverage_ratio, 1.0);
    }

    #[test]
    fn weekly_token_estimate_is_insufficient_with_one_valid_sample() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO rate_limit_samples
                   (account_key, sampled_at, limit_id, window, window_duration_mins,
                  used_percent, resets_at, source, confidence)
                 VALUES
                   ('a', 1000, 'weekly', 'primary', 10080, 10, 20000, 'account_rate_limit', 'high'),
                   ('a', 2000, 'weekly', 'primary', 10080, 11, 20000, 'account_rate_limit', 'high'),
                   ('a', 3000, 'weekly', 'primary', 10080, 12, 20000, 'account_rate_limit', 'high')",
                [],
            )
            .unwrap();
        quota::refresh_intervals(&connection, "a", "weekly", "primary").unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, completed_at, model, reasoning_effort,
                  speed_mode, raw_total_tokens, source, confidence, created_at, updated_at)
                 VALUES ('a', 'thread', 'turn-1', 2500, 2500, 'gpt-5.6-sol', 'high', 'standard',
                         1000, 'local_rollout', 'high', 2500, 2500)",
                [],
            )
            .unwrap();
        let mut categories = BTreeMap::new();
        categories.insert(
            ("gpt-5.6-sol".into(), "high".into(), "standard".into()),
            LocalAggregate {
                tokens: 1000,
                turn_count: 1,
            },
        );
        let quota_window = CategoryUsageQuotaWindow {
            limit_id: "weekly".into(),
            window: "primary".into(),
            used_percent: 20.0,
            remaining_percent: 80.0,
            window_duration_mins: 10080,
            resets_at: Some(20000),
        };
        let estimate = estimate_weekly_categories(
            &connection,
            Some("a"),
            1000,
            10000,
            Some(&quota_window),
            &categories,
        )
        .unwrap()
        .remove(&("gpt-5.6-sol".into(), "high".into(), "standard".into()))
        .unwrap();
        assert_eq!(estimate.status, USAGE_STATUS_INSUFFICIENT_DATA);
        assert_eq!(estimate.estimated_tokens, None);
        assert_eq!(estimate.valid_sample_count, 1);
    }

    #[test]
    fn long_turn_crossing_a_quota_step_boundary_is_rejected() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, completed_at, model,
                  reasoning_effort, speed_mode, raw_total_tokens, source, confidence,
                  created_at, updated_at)
                 VALUES ('a', 'thread', 'long', 1500, 2500, 'gpt-5.6-sol', 'high',
                         'standard', 12000000, 'local_rollout', 'medium', 2500, 2500)",
                [],
            )
            .unwrap();
        assert!(has_boundary_overlap(&connection, "a", 1000, 2000).unwrap());
        assert!(has_boundary_overlap(&connection, "a", 2000, 3000).unwrap());
    }
}
