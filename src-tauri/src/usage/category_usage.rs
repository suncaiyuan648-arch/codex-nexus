use chrono::{DateTime, Local, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::models::{
    AccountDataHealth, CategoryTokenEstimate, CategoryUsage, CategoryUsageItem,
    CategoryUsageQuotaWindow, Confidence, TokenUsageMetric, UsageMetric,
    PROVENANCE_ACCOUNT_RATE_LIMIT, PROVENANCE_DERIVED_ESTIMATE, PROVENANCE_LOCAL_ROLLOUT,
    USAGE_STATUS_ESTIMATED, USAGE_STATUS_INSUFFICIENT_DATA, USAGE_STATUS_OBSERVED,
};
use super::{db, quota, recorder, rollout};

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
    // The sample timeline is the only source that can attribute a long turn
    // to the time it actually produced tokens. Using turn_usage.started_at
    // here would assign a turn that spans the boundary entirely to its start
    // bucket and would double-count it when a later timeline correction is
    // applied.
    let mut statement = connection
        .prepare(
            "SELECT COALESCE(s.model, 'unknown'), s.reasoning_effort, s.speed_mode,
                    SUM(s.delta_tokens), COUNT(DISTINCT s.thread_id || ':' || s.turn_id)
             FROM turn_token_samples s
             INNER JOIN turn_usage t
               ON t.account_key = s.account_key
              AND t.thread_id = s.thread_id
              AND t.turn_id = s.turn_id
             WHERE s.account_key = ?1
               AND s.sampled_at >= ?2 AND s.sampled_at < ?3
             GROUP BY s.model, s.reasoning_effort, s.speed_mode",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, start, end], |row| {
            Ok((
                (
                    row.get::<_, Option<String>>(0)?
                        .unwrap_or_else(|| "unknown".into()),
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ),
                row.get::<_, Option<i64>>(3)?.unwrap_or_default().max(0),
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut categories: BTreeMap<CategoryKey, LocalAggregate> = BTreeMap::new();
    let mut total: i64 = 0;
    for row in rows {
        let (key, tokens, turn_count) = row.map_err(|error| error.to_string())?;
        let entry = categories.entry(key).or_default();
        entry.tokens = entry.tokens.saturating_add(tokens);
        entry.turn_count += turn_count;
        total = total.saturating_add(tokens);
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

#[derive(Clone, Debug)]
struct TokenDeltaSample {
    key: CategoryKey,
    sampled_at: i64,
    delta_tokens: i64,
    previous_sampled_at: Option<i64>,
}

#[derive(Clone, Debug, Default)]
struct CategoryEstimateAccumulator {
    current_tokens: i64,
    observed_sample_count: i64,
    valid_sample_count: i64,
    observed_tokens: i64,
    candidate_tokens: i64,
    excluded_tokens: i64,
    ambiguous_boundary_tokens: i64,
    observed_quota_percent: f64,
    pre_observation_tokens: i64,
    pending_tokens: i64,
    rejected_sample_count: i64,
    boundary_overlap_count: i64,
    rates: Vec<(f64, f64)>,
    hard_blockers: Vec<String>,
    warnings: Vec<String>,
    rejection_reasons: Vec<String>,
}

fn add_rejection_reason(value: &mut CategoryEstimateAccumulator, reason: &str) {
    if !value.rejection_reasons.iter().any(|item| item == reason) {
        value.rejection_reasons.push(reason.into());
    }
    let destination = match reason {
        "boundary_overlap"
        | "boundary_ambiguity"
        | "pending_tokens"
        | "external_usage_risk"
        | "pre_observation_tokens"
        | "source_gap"
        | "mixed_category_unresolved" => &mut value.warnings,
        _ => &mut value.hard_blockers,
    };
    if !destination.iter().any(|item| item == reason) {
        destination.push(reason.into());
    }
}

fn diagnostic_reasons(value: &CategoryEstimateAccumulator) -> Vec<String> {
    value
        .hard_blockers
        .iter()
        .chain(value.warnings.iter())
        .cloned()
        .collect()
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

fn timeline_token_samples(
    connection: &Connection,
    account_key: &str,
    start: i64,
    end: i64,
) -> Result<Vec<TokenDeltaSample>, String> {
    let mut statement = connection
        .prepare(
            "SELECT s.thread_id, s.turn_id, COALESCE(s.model, 'unknown'),
                    s.reasoning_effort, s.speed_mode, s.sampled_at, s.delta_tokens
             FROM turn_token_samples s
             INNER JOIN turn_usage t
               ON t.account_key = s.account_key
              AND t.thread_id = s.thread_id
              AND t.turn_id = s.turn_id
             WHERE s.account_key = ?1
               AND s.sampled_at <= ?2
             ORDER BY s.thread_id, s.turn_id, s.sampled_at, s.id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![account_key, end], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                (
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ),
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut previous_by_turn: HashMap<(String, String), i64> = HashMap::new();
    let mut samples = Vec::new();
    for row in rows {
        let (thread_id, turn_id, key, sampled_at, delta_tokens) =
            row.map_err(|error| error.to_string())?;
        let turn_key = (thread_id.clone(), turn_id.clone());
        let previous_sampled_at = previous_by_turn.insert(turn_key, sampled_at);
        if sampled_at > start && sampled_at <= end && delta_tokens > 0 {
            samples.push(TokenDeltaSample {
                key,
                sampled_at,
                delta_tokens,
                previous_sampled_at,
            });
        }
    }
    Ok(samples)
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

fn weighted_median(values: &[(f64, f64)]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.0.total_cmp(&right.0));
    let total_weight: f64 = sorted.iter().map(|(_, weight)| weight.max(0.0)).sum();
    if total_weight <= 0.0 {
        return median(&sorted.iter().map(|(value, _)| *value).collect::<Vec<_>>());
    }
    let midpoint = total_weight / 2.0;
    let mut accumulated = 0.0;
    for (value, weight) in sorted {
        accumulated += weight.max(0.0);
        if accumulated >= midpoint {
            return Some(value);
        }
    }
    None
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
        .map(|key| {
            let current_tokens = categories.get(&key).map(|item| item.tokens).unwrap_or(0);
            (
                key,
                CategoryEstimateAccumulator {
                    current_tokens,
                    ..CategoryEstimateAccumulator::default()
                },
            )
        })
        .collect();

    let Some(quota_window) = quota_window else {
        return Ok(accumulators
            .into_iter()
            .map(|(key, mut value)| {
                add_rejection_reason(&mut value, "quota_window_missing");
                (
                    key,
                    insufficient_estimate(value, 0.0, 0.0, 0.0, PROVENANCE_DERIVED_ESTIMATE),
                )
            })
            .collect());
    };

    // A category estimate is not allowed to paper over a broken lower-level
    // ledger. Keep this as a category diagnostic so the UI can still expose
    // the failure instead of turning it into an RPC error or a clamped ratio.
    let account_health_blocker = match account_key {
        Some(account_key) => match rollout::account_data_health(connection, account_key)? {
            Some(health) if health.status == rollout::DATA_HEALTH_VERIFIED => None,
            Some(health) if health.status == rollout::DATA_HEALTH_REBUILDING => {
                Some("account_data_rebuilding")
            }
            Some(health) if health.status == rollout::DATA_HEALTH_ACCOUNTING_INCONSISTENT => {
                Some("accounting_inconsistent")
            }
            Some(health) if health.status == rollout::DATA_HEALTH_SOURCE_INCOMPLETE => {
                Some("source_incomplete")
            }
            _ => Some("legacy_unverified"),
        },
        None => Some("legacy_unverified"),
    };

    let steps = quota_steps(
        connection,
        account_key,
        &quota_window.limit_id,
        &quota_window.window,
        start,
        end,
    )?;
    let timeline_samples = account_key
        .map(|account_key| timeline_token_samples(connection, account_key, start, end))
        .transpose()?
        .unwrap_or_default();
    let observation_horizon = steps.first().map(|step| step.start_at).unwrap_or(end);
    for step in &steps {
        let mut categories_in_step = BTreeSet::new();
        let mut valid_tokens_by_category: BTreeMap<CategoryKey, i64> = BTreeMap::new();
        let mut boundary_categories = BTreeSet::new();
        for sample in timeline_samples
            .iter()
            .filter(|sample| sample.sampled_at > step.start_at && sample.sampled_at <= step.end_at)
        {
            let sample_tokens = sample.delta_tokens.max(0);
            categories_in_step.insert(sample.key.clone());
            if let Some(accumulator) = accumulators.get_mut(&sample.key) {
                accumulator.candidate_tokens =
                    accumulator.candidate_tokens.saturating_add(sample_tokens);
            }
            let crosses_step_start = sample
                .previous_sampled_at
                .is_some_and(|previous| previous < step.start_at);
            if crosses_step_start {
                boundary_categories.insert(sample.key.clone());
                if let Some(accumulator) = accumulators.get_mut(&sample.key) {
                    accumulator.ambiguous_boundary_tokens = accumulator
                        .ambiguous_boundary_tokens
                        .saturating_add(sample_tokens);
                }
                continue;
            }
            let entry = valid_tokens_by_category
                .entry(sample.key.clone())
                .or_default();
            *entry = entry.saturating_add(sample_tokens);
        }

        if categories_in_step.is_empty() {
            // A quota transition with no local Token sample is a source gap,
            // not evidence that every category has too few observations.
            for value in accumulators.values_mut() {
                add_rejection_reason(value, "source_gap");
            }
            continue;
        }
        for key in &categories_in_step {
            if let Some(accumulator) = accumulators.get_mut(key) {
                accumulator.observed_sample_count += 1;
            }
        }

        for key in &boundary_categories {
            if let Some(accumulator) = accumulators.get_mut(key) {
                accumulator.boundary_overlap_count += 1;
                add_rejection_reason(accumulator, "boundary_overlap");
                add_rejection_reason(accumulator, "boundary_ambiguity");
            }
        }

        if categories_in_step.len() != 1 {
            for sample in timeline_samples.iter().filter(|sample| {
                sample.sampled_at > step.start_at && sample.sampled_at <= step.end_at
            }) {
                let crosses_step_start = sample
                    .previous_sampled_at
                    .is_some_and(|previous| previous < step.start_at);
                if !crosses_step_start {
                    if let Some(accumulator) = accumulators.get_mut(&sample.key) {
                        accumulator.excluded_tokens = accumulator
                            .excluded_tokens
                            .saturating_add(sample.delta_tokens.max(0));
                    }
                }
            }
            for key in &categories_in_step {
                if let Some(value) = accumulators.get_mut(key) {
                    value.rejected_sample_count += 1;
                    add_rejection_reason(value, "mixed_category_unresolved");
                }
            }
            // Mixed steps are excluded from every involved category's
            // evidence, but remain warnings. A single mixed step must not
            // erase otherwise valid pure-category observations.
            continue;
        }

        let key = categories_in_step.into_iter().next().unwrap();
        let tokens = valid_tokens_by_category
            .get(&key)
            .copied()
            .unwrap_or_default();
        let Some(accumulator) = accumulators.get_mut(&key) else {
            continue;
        };
        if tokens <= 0 {
            accumulator.rejected_sample_count += 1;
            if !boundary_categories.contains(&key) {
                add_rejection_reason(accumulator, "insufficient_observed_tokens");
            }
            accumulator.excluded_tokens = accumulator.excluded_tokens.saturating_add(tokens.max(0));
            continue;
        }
        accumulator.valid_sample_count += 1;
        accumulator.observed_tokens = accumulator.observed_tokens.saturating_add(tokens);
        accumulator.observed_quota_percent += step.observed_delta_percent;
        accumulator.rates.push((
            tokens as f64 / step.observed_delta_percent.max(f64::EPSILON),
            step.observed_delta_percent,
        ));
    }

    let pending_start = steps.last().map(|step| step.end_at).unwrap_or(start);
    for sample in &timeline_samples {
        if sample.sampled_at <= observation_horizon {
            if let Some(accumulator) = accumulators.get_mut(&sample.key) {
                accumulator.pre_observation_tokens = accumulator
                    .pre_observation_tokens
                    .saturating_add(sample.delta_tokens.max(0));
            }
        }
        if sample.sampled_at > pending_start {
            if let Some(accumulator) = accumulators.get_mut(&sample.key) {
                accumulator.pending_tokens = accumulator
                    .pending_tokens
                    .saturating_add(sample.delta_tokens.max(0));
            }
        }
    }

    Ok(accumulators
        .into_iter()
        .map(|(key, mut value)| {
            let total_category_tokens = value.current_tokens.max(0);
            let current_tokens = total_category_tokens;
            let raw_eligible_tokens =
                current_tokens - value.pre_observation_tokens - value.pending_tokens;
            let eligible_tokens = raw_eligible_tokens
                .saturating_sub(value.excluded_tokens)
                .saturating_sub(value.ambiguous_boundary_tokens)
                .max(0);
            let coverage_ratio = if eligible_tokens > 0 {
                value.observed_tokens as f64 / eligible_tokens as f64
            } else if value.observed_tokens > 0 {
                // Keep the raw invariant failure visible to diagnostics. Do
                // not clamp it into an apparently valid 100% coverage.
                value.observed_tokens as f64
            } else {
                0.0
            };
            let ambiguous_boundary_ratio = if value.candidate_tokens > 0 {
                value.ambiguous_boundary_tokens as f64 / value.candidate_tokens as f64
            } else {
                0.0
            };
            let accounted_step_tokens = value
                .observed_tokens
                .saturating_add(value.ambiguous_boundary_tokens)
                .saturating_add(value.excluded_tokens);
            if let Some(reason) = account_health_blocker {
                add_rejection_reason(&mut value, reason);
            }
            if raw_eligible_tokens < 0
                || value.observed_tokens > eligible_tokens
                || eligible_tokens > total_category_tokens
                || value.candidate_tokens > raw_eligible_tokens
                || accounted_step_tokens != value.candidate_tokens
            {
                add_rejection_reason(&mut value, "accounting_inconsistent");
            }
            let rate_values = value
                .rates
                .iter()
                .map(|(rate, _)| *rate)
                .collect::<Vec<_>>();
            let median_rate = weighted_median(&value.rates).unwrap_or(0.0);
            let dispersion = if median_rate > 0.0 {
                median_absolute_deviation(&rate_values, median_rate) / median_rate
            } else {
                f64::INFINITY
            };
            let full_week_tokens = (median_rate * 100.0).round().clamp(0.0, i64::MAX as f64) as i64;
            let current_quota_lower_bound =
                full_week_tokens as f64 * quota_window.used_percent.clamp(0.0, 100.0) / 100.0;
            if value.valid_sample_count < MIN_VALID_SAMPLES {
                add_rejection_reason(&mut value, "insufficient_samples");
            }
            if value.observed_tokens < MIN_OBSERVED_TOKENS {
                add_rejection_reason(&mut value, "insufficient_observed_tokens");
            }
            if value.observed_quota_percent < MIN_OBSERVED_QUOTA_PERCENT {
                add_rejection_reason(&mut value, "insufficient_quota_span");
            }
            if coverage_ratio < MIN_COVERAGE_RATIO {
                add_rejection_reason(&mut value, "insufficient_coverage");
            }
            if current_tokens > 0 && eligible_tokens <= 0 {
                add_rejection_reason(&mut value, "insufficient_eligible_tokens");
            }
            if full_week_tokens < current_tokens
                || current_quota_lower_bound + 1.0 < current_tokens as f64
            {
                add_rejection_reason(&mut value, "sanity_check_failed");
            }
            if dispersion > MAX_DISPERSION_RATIO {
                add_rejection_reason(&mut value, "excessive_dispersion");
            }
            if value.pending_tokens > 0 {
                add_rejection_reason(&mut value, "pending_tokens");
            }
            if value.pre_observation_tokens > 0 || coverage_ratio < 1.0 {
                add_rejection_reason(&mut value, "external_usage_risk");
            }
            let hard_blockers = value.hard_blockers.clone();
            let warnings = value.warnings.clone();
            let rejection_reasons = diagnostic_reasons(&value);
            let estimate = if hard_blockers.is_empty() {
                let remaining_tokens =
                    ((full_week_tokens as f64 * quota_window.remaining_percent.max(0.0)) / 100.0)
                        .round()
                        .clamp(0.0, i64::MAX as f64) as i64;
                CategoryTokenEstimate {
                    status: USAGE_STATUS_ESTIMATED.into(),
                    estimated_tokens: Some(full_week_tokens),
                    remaining_tokens: Some(remaining_tokens),
                    current_tokens,
                    total_category_tokens: current_tokens,
                    observed_sample_count: value.observed_sample_count,
                    valid_sample_count: value.valid_sample_count,
                    observed_tokens: value.observed_tokens,
                    observed_quota_percent: value.observed_quota_percent,
                    cumulative_observed_quota_delta: value.observed_quota_percent,
                    coverage_ratio,
                    pre_observation_tokens: value.pre_observation_tokens,
                    eligible_tokens,
                    pending_tokens: value.pending_tokens,
                    rejected_sample_count: value.rejected_sample_count,
                    boundary_overlap_count: value.boundary_overlap_count,
                    boundary_overlap_ratio: if value.observed_sample_count > 0 {
                        value.boundary_overlap_count as f64 / value.observed_sample_count as f64
                    } else {
                        0.0
                    },
                    ambiguous_boundary_tokens: value.ambiguous_boundary_tokens,
                    ambiguous_boundary_ratio,
                    dispersion_ratio: dispersion,
                    hard_blockers,
                    warnings,
                    rejection_reasons,
                    external_usage_risk: value.pre_observation_tokens > 0
                        || value.pending_tokens > 0,
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
                    ambiguous_boundary_ratio,
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
    ambiguous_boundary_ratio: f64,
    source: &str,
) -> CategoryTokenEstimate {
    let eligible_tokens = (value.current_tokens
        - value.pre_observation_tokens
        - value.pending_tokens
        - value.excluded_tokens
        - value.ambiguous_boundary_tokens)
        .max(0);
    let hard_blockers = value.hard_blockers.clone();
    let warnings = value.warnings.clone();
    let rejection_reasons = diagnostic_reasons(&value);
    CategoryTokenEstimate {
        status: USAGE_STATUS_INSUFFICIENT_DATA.into(),
        estimated_tokens: None,
        remaining_tokens: None,
        current_tokens: value.current_tokens,
        total_category_tokens: value.current_tokens.max(0),
        observed_sample_count: value.observed_sample_count,
        valid_sample_count: value.valid_sample_count,
        observed_tokens: value.observed_tokens,
        observed_quota_percent: value.observed_quota_percent,
        cumulative_observed_quota_delta: value.observed_quota_percent,
        coverage_ratio,
        pre_observation_tokens: value.pre_observation_tokens,
        eligible_tokens,
        pending_tokens: value.pending_tokens,
        rejected_sample_count: value.rejected_sample_count,
        boundary_overlap_count: value.boundary_overlap_count,
        boundary_overlap_ratio: if value.observed_sample_count > 0 {
            value.boundary_overlap_count as f64 / value.observed_sample_count as f64
        } else {
            0.0
        },
        ambiguous_boundary_tokens: value.ambiguous_boundary_tokens,
        ambiguous_boundary_ratio,
        dispersion_ratio: dispersion,
        hard_blockers,
        warnings,
        rejection_reasons,
        external_usage_risk: value.pre_observation_tokens > 0 || value.pending_tokens > 0,
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
    let mut change_count = 0;
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
        let delta = current.used_percent - previous.used_percent;
        if delta > f64::EPSILON {
            change_count += 1;
        }
        observed_percent += delta;
    }
    if sample_count == 0 {
        return insufficient_quota_metric();
    }
    UsageMetric {
        status: USAGE_STATUS_OBSERVED.into(),
        value: Some(observed_percent),
        sample_count,
        change_count,
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
        change_count: 0,
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
    let data_health: Option<AccountDataHealth> = account_key
        .as_deref()
        .map(|account_key| rollout::account_data_health(connection, account_key))
        .transpose()?
        .flatten();
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
        data_health,
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

    fn seed_weekly_steps(connection: &Connection) {
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
        quota::refresh_intervals(connection, "a", "weekly", "primary").unwrap();
    }

    fn insert_timeline_sample(
        connection: &Connection,
        turn_id: &str,
        model: &str,
        sampled_at: i64,
        cumulative_tokens: i64,
        delta_tokens: i64,
    ) {
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, model, reasoning_effort, speed_mode,
                  sampled_at, cumulative_tokens, delta_tokens, source, confidence)
                 VALUES ('a', 'thread', ?1, ?2, 'xhigh', 'standard',
                         ?3, ?4, ?5, 'rollout', 'high')",
                params![turn_id, model, sampled_at, cumulative_tokens, delta_tokens],
            )
            .unwrap();
        let canonical_tokens: i64 = connection
            .query_row(
                "SELECT SUM(delta_tokens) FROM turn_token_samples
                 WHERE account_key = 'a' AND thread_id = 'thread' AND turn_id = ?1",
                [turn_id],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, completed_at, model,
                  reasoning_effort, speed_mode, raw_total_tokens, source, confidence,
                  created_at, updated_at)
                 VALUES ('a', 'thread', ?1, ?2, ?2, ?3, 'xhigh', 'standard',
                         ?4, 'rollout', 'high', ?2, ?2)
                 ON CONFLICT(account_key, thread_id, turn_id) DO UPDATE SET
                   completed_at = excluded.completed_at,
                   raw_total_tokens = excluded.raw_total_tokens,
                   updated_at = excluded.updated_at",
                params![turn_id, sampled_at, model, canonical_tokens],
            )
            .unwrap();
        mark_account_verified(connection);
    }

    fn mark_account_verified(connection: &Connection) {
        connection
            .execute(
                "INSERT INTO account_usage_data_versions
                 (account_key, rollout_parser_version, status, timeline_status,
                  missing_timeline_turns, orphan_timeline_samples, mismatched_turns,
                  verified_at, updated_at)
                 VALUES ('a', ?1, 'verified', 'complete', 0, 0, 0, 1, 1)
                 ON CONFLICT(account_key) DO UPDATE SET
                   rollout_parser_version = excluded.rollout_parser_version,
                   status = excluded.status,
                   timeline_status = excluded.timeline_status,
                   missing_timeline_turns = 0,
                   orphan_timeline_samples = 0,
                   mismatched_turns = 0,
                   verified_at = 1,
                   updated_at = 1",
                [rollout::ROLLOUT_PARSER_VERSION],
            )
            .unwrap();
    }

    #[test]
    fn local_usage_is_attributed_by_token_sample_time() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for (sampled_at, delta_tokens) in [(900, 5), (1500, 7), (2500, 11)] {
            insert_timeline_sample(
                &connection,
                "long-turn",
                "gpt-5.6-sol",
                sampled_at,
                delta_tokens,
                delta_tokens,
            );
        }

        let (categories, total) = local_category_usage(&connection, Some("a"), 1000, 2000).unwrap();
        assert_eq!(total, 7);
        assert_eq!(
            categories
                .get(&("gpt-5.6-sol".into(), "xhigh".into(), "standard".into()))
                .map(|value| (value.tokens, value.turn_count)),
            Some((7, 1))
        );
    }

    #[test]
    fn orphan_timeline_is_quarantined_from_category_usage_and_estimator() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        seed_weekly_steps(&connection);
        insert_timeline_sample(&connection, "valid", "gpt-5.6-sol", 2500, 1_000, 1_000);
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, model, reasoning_effort, speed_mode,
                  sampled_at, cumulative_tokens, delta_tokens, source, confidence)
                 VALUES ('a', 'thread', 'orphan', 'gpt-5.6-sol', 'xhigh', 'standard',
                         2600, 5000, 5000, 'rollout', 'high')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE account_usage_data_versions
                 SET status = 'accounting_inconsistent', orphan_timeline_samples = 1
                 WHERE account_key = 'a'",
                [],
            )
            .unwrap();

        let samples = timeline_token_samples(&connection, "a", 1000, 10_000).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].delta_tokens, 1_000);
        let (categories, total) =
            local_category_usage(&connection, Some("a"), 1000, 10_000).unwrap();
        assert_eq!(total, 1_000);

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
            10_000,
            Some(&quota_window),
            &categories,
        )
        .unwrap()
        .remove(&("gpt-5.6-sol".into(), "xhigh".into(), "standard".into()))
        .unwrap();
        assert!(estimate
            .hard_blockers
            .iter()
            .any(|reason| reason == "accounting_inconsistent"));
    }

    fn single_category_tokens(
        connection: &Connection,
        model: &str,
        tokens: i64,
    ) -> BTreeMap<CategoryKey, LocalAggregate> {
        let mut categories = BTreeMap::new();
        categories.insert(
            (model.into(), "xhigh".into(), "standard".into()),
            LocalAggregate {
                tokens,
                turn_count: 1,
            },
        );
        let _ = connection;
        categories
    }

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
    fn daily_quota_usage_separates_changes_from_valid_sample_intervals() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO rate_limit_samples
                 (account_key, sampled_at, limit_id, window, window_duration_mins,
                  used_percent, resets_at, source, confidence)
                 VALUES
                   ('a', 100, 'weekly', 'secondary', 10080, 10, 20000, 'account_rate_limit', 'high'),
                   ('a', 200, 'weekly', 'secondary', 10080, 10, 20000, 'account_rate_limit', 'high'),
                   ('a', 300, 'weekly', 'secondary', 10080, 11, 20000, 'account_rate_limit', 'high'),
                   ('a', 400, 'weekly', 'secondary', 10080, 11, 20000, 'account_rate_limit', 'high')",
                [],
            )
            .unwrap();
        let quota_window = CategoryUsageQuotaWindow {
            limit_id: "weekly".into(),
            window: "secondary".into(),
            used_percent: 11.0,
            remaining_percent: 89.0,
            window_duration_mins: 10080,
            resets_at: Some(20000),
        };
        let metric = daily_quota_usage(&connection, Some("a"), 100, 500, Some(&quota_window));
        assert_eq!(metric.value, Some(1.0));
        assert_eq!(metric.sample_count, 3);
        assert_eq!(metric.change_count, 1);
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
            connection
                .execute(
                    "INSERT INTO turn_token_samples
                     (account_key, thread_id, turn_id, model, reasoning_effort, speed_mode,
                      sampled_at, cumulative_tokens, delta_tokens, source, confidence)
                     VALUES ('a', 'thread', ?1, 'gpt-5.6-sol', 'high', 'standard',
                             ?2, 2000, 2000, 'rollout', 'high')",
                    params![turn_id, started_at],
                )
                .unwrap();
        }
        mark_account_verified(&connection);
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
        mark_account_verified(&connection);
        connection
            .execute(
                "INSERT INTO turn_token_samples
                 (account_key, thread_id, turn_id, model, reasoning_effort, speed_mode,
                  sampled_at, cumulative_tokens, delta_tokens, source, confidence)
                 VALUES ('a', 'thread', 'turn-1', 'gpt-5.6-sol', 'high', 'standard',
                         2500, 1000, 1000, 'rollout', 'high')",
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
    fn long_turn_crossing_a_quota_step_boundary_keeps_token_deltas() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        insert_timeline_sample(
            &connection,
            "long",
            "gpt-5.6-sol",
            1500,
            5_000_000,
            5_000_000,
        );
        insert_timeline_sample(
            &connection,
            "long",
            "gpt-5.6-sol",
            2500,
            12_000_000,
            7_000_000,
        );
        let samples = timeline_token_samples(&connection, "a", 1000, 3000).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].previous_sampled_at, None);
        assert_eq!(samples[1].previous_sampled_at, Some(1500));
        assert_eq!(samples[1].delta_tokens, 7_000_000);
    }

    #[test]
    fn mixed_quota_step_is_rejected_without_token_ratio_split() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        seed_weekly_steps(&connection);
        insert_timeline_sample(
            &connection,
            "luna",
            "gpt-5.6-luna",
            2500,
            5_000_000,
            5_000_000,
        );
        insert_timeline_sample(
            &connection,
            "sol",
            "gpt-5.6-sol",
            2600,
            3_000_000,
            3_000_000,
        );
        let mut categories = BTreeMap::new();
        categories.extend(single_category_tokens(
            &connection,
            "gpt-5.6-luna",
            5_000_000,
        ));
        categories.insert(
            ("gpt-5.6-sol".into(), "xhigh".into(), "standard".into()),
            LocalAggregate {
                tokens: 3_000_000,
                turn_count: 1,
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
        let estimates = estimate_weekly_categories(
            &connection,
            Some("a"),
            1000,
            10000,
            Some(&quota_window),
            &categories,
        )
        .unwrap();
        for key in categories.keys() {
            let estimate = estimates.get(key).unwrap();
            assert_eq!(estimate.status, USAGE_STATUS_INSUFFICIENT_DATA);
            assert!(estimate
                .rejection_reasons
                .iter()
                .any(|reason| reason == "mixed_category_unresolved"));
        }
    }

    #[test]
    fn one_mixed_step_does_not_block_categories_with_five_pure_steps() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        for timestamp in 1000..=9000 {
            if timestamp % 1000 != 0 {
                continue;
            }
            connection
                .execute(
                    "INSERT INTO rate_limit_samples
                     (account_key, sampled_at, limit_id, window, window_duration_mins,
                      used_percent, resets_at, source, confidence)
                     VALUES ('a', ?1, 'weekly', 'primary', 10080, ?2, 20000,
                             'account_rate_limit', 'high')",
                    params![timestamp, 10.0 + (timestamp / 1000) as f64],
                )
                .unwrap();
        }
        for (index, sampled_at) in [2500, 3500, 4500, 5500, 6500].into_iter().enumerate() {
            insert_timeline_sample(
                &connection,
                &format!("pure-{index}"),
                "gpt-5.6-luna",
                sampled_at,
                2_000,
                2_000,
            );
        }
        insert_timeline_sample(
            &connection,
            "mixed-luna",
            "gpt-5.6-luna",
            7500,
            2_000,
            2_000,
        );
        insert_timeline_sample(&connection, "mixed-sol", "gpt-5.6-sol", 7500, 1_000, 1_000);
        quota::refresh_intervals(&connection, "a", "weekly", "primary").unwrap();

        let mut categories = BTreeMap::new();
        categories.insert(
            ("gpt-5.6-luna".into(), "xhigh".into(), "standard".into()),
            LocalAggregate {
                tokens: 12_000,
                turn_count: 6,
            },
        );
        categories.insert(
            ("gpt-5.6-sol".into(), "xhigh".into(), "standard".into()),
            LocalAggregate {
                tokens: 1_000,
                turn_count: 1,
            },
        );
        let quota_window = CategoryUsageQuotaWindow {
            limit_id: "weekly".into(),
            window: "primary".into(),
            used_percent: 18.0,
            remaining_percent: 82.0,
            window_duration_mins: 10080,
            resets_at: Some(20000),
        };
        let estimates = estimate_weekly_categories(
            &connection,
            Some("a"),
            1000,
            10_000,
            Some(&quota_window),
            &categories,
        )
        .unwrap();
        let luna = estimates
            .get(&("gpt-5.6-luna".into(), "xhigh".into(), "standard".into()))
            .unwrap();
        assert_eq!(luna.valid_sample_count, 5);
        assert_eq!(luna.status, USAGE_STATUS_ESTIMATED);
        assert!(!luna
            .hard_blockers
            .iter()
            .any(|reason| reason == "mixed_category_unresolved"));
        assert!(luna
            .warnings
            .iter()
            .any(|reason| reason == "mixed_category_unresolved"));
    }

    #[test]
    fn long_turn_crossing_two_boundaries_keeps_non_overlapping_deltas() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        seed_weekly_steps(&connection);
        for (sampled_at, cumulative_tokens, delta_tokens) in [
            (1500, 10_000_000, 10_000_000),
            (2500, 12_000_000, 2_000_000),
            (3200, 15_000_000, 3_000_000),
            (3500, 18_000_000, 3_000_000),
        ] {
            insert_timeline_sample(
                &connection,
                "long",
                "gpt-5.6-luna",
                sampled_at,
                cumulative_tokens,
                delta_tokens,
            );
        }
        let categories = single_category_tokens(&connection, "gpt-5.6-luna", 18_000_000);
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
        .remove(&("gpt-5.6-luna".into(), "xhigh".into(), "standard".into()))
        .unwrap();
        assert_eq!(estimate.boundary_overlap_count, 2);
        assert_eq!(estimate.valid_sample_count, 1);
        assert_eq!(estimate.observed_tokens, 3_000_000);
        assert!(estimate
            .rejection_reasons
            .iter()
            .any(|reason| reason == "boundary_overlap"));
        assert_eq!(estimate.ambiguous_boundary_tokens, 5_000_000);
        assert!(estimate.ambiguous_boundary_ratio > 0.6);
        assert!(!estimate
            .hard_blockers
            .iter()
            .any(|reason| reason == "boundary_overlap"));
        assert!(estimate
            .warnings
            .iter()
            .any(|reason| reason == "boundary_ambiguity"));
    }

    #[test]
    fn observed_tokens_above_eligible_tokens_is_a_hard_accounting_blocker() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        seed_weekly_steps(&connection);
        for (index, sampled_at) in [2500, 3500, 4500, 5500, 6500].into_iter().enumerate() {
            insert_timeline_sample(
                &connection,
                &format!("inconsistent-{index}"),
                "gpt-5.6-sol",
                sampled_at,
                1_000,
                1_000,
            );
        }
        let categories = single_category_tokens(&connection, "gpt-5.6-sol", 3_000);
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
        .remove(&("gpt-5.6-sol".into(), "xhigh".into(), "standard".into()))
        .unwrap();
        assert_eq!(estimate.status, USAGE_STATUS_INSUFFICIENT_DATA);
        assert!(estimate.coverage_ratio > 1.0);
        assert!(estimate
            .hard_blockers
            .iter()
            .any(|reason| reason == "accounting_inconsistent"));
    }

    #[test]
    fn pending_tokens_are_not_counted_as_closed_step_observations() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        seed_weekly_steps(&connection);
        insert_timeline_sample(
            &connection,
            "luna",
            "gpt-5.6-luna",
            2500,
            5_000_000,
            5_000_000,
        );
        insert_timeline_sample(
            &connection,
            "luna",
            "gpt-5.6-luna",
            7500,
            8_000_000,
            3_000_000,
        );
        let categories = single_category_tokens(&connection, "gpt-5.6-luna", 8_000_000);
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
        .remove(&("gpt-5.6-luna".into(), "xhigh".into(), "standard".into()))
        .unwrap();
        assert_eq!(estimate.observed_tokens, 5_000_000);
        assert_eq!(estimate.pending_tokens, 3_000_000);
        assert_eq!(estimate.eligible_tokens, 5_000_000);
        assert_eq!(estimate.coverage_ratio, 1.0);
    }

    #[test]
    fn pre_observation_tokens_do_not_lower_eligible_coverage() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        seed_weekly_steps(&connection);
        insert_timeline_sample(
            &connection,
            "luna",
            "gpt-5.6-luna",
            1500,
            1_000_000,
            1_000_000,
        );
        for (index, sampled_at) in [2500, 3500, 4500, 5500, 6500].into_iter().enumerate() {
            insert_timeline_sample(
                &connection,
                &format!("luna-{index}"),
                "gpt-5.6-luna",
                sampled_at,
                2_000_000,
                2_000_000,
            );
        }
        let categories = single_category_tokens(&connection, "gpt-5.6-luna", 11_000_000);
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
        .remove(&("gpt-5.6-luna".into(), "xhigh".into(), "standard".into()))
        .unwrap();
        assert_eq!(estimate.pre_observation_tokens, 1_000_000);
        assert_eq!(estimate.eligible_tokens, 10_000_000);
        assert_eq!(estimate.observed_tokens, 10_000_000);
        assert_eq!(estimate.coverage_ratio, 1.0);
    }

    #[test]
    fn estimate_below_current_category_tokens_is_rejected() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        seed_weekly_steps(&connection);
        for (index, sampled_at) in [2500, 3500, 4500, 5500, 6500].into_iter().enumerate() {
            insert_timeline_sample(
                &connection,
                &format!("small-{index}"),
                "gpt-5.6-luna",
                sampled_at,
                1_000,
                1_000,
            );
        }
        let categories = single_category_tokens(&connection, "gpt-5.6-luna", 10_000_000);
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
        .remove(&("gpt-5.6-luna".into(), "xhigh".into(), "standard".into()))
        .unwrap();
        assert_eq!(estimate.status, USAGE_STATUS_INSUFFICIENT_DATA);
        assert!(estimate
            .rejection_reasons
            .iter()
            .any(|reason| reason == "sanity_check_failed"));
    }
}
