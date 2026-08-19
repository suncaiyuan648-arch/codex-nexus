use chrono::{FixedOffset, Local, NaiveDate, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::models::*;
use super::{db, quota, rate_card::RateCard, recorder};

#[derive(Clone, Debug)]
struct TurnRow {
    account_key: String,
    started_at: i64,
    model: Option<String>,
    reasoning_effort: String,
    speed_mode: String,
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    raw_total_tokens: i64,
    confidence: Confidence,
}

#[derive(Clone, Debug, Default)]
struct Aggregate {
    raw_tokens: i64,
    estimated_credits: Option<f64>,
    attributed_quota_percent: f64,
    turn_count: i64,
    low_confidence: bool,
}

impl Aggregate {
    fn add(&mut self, turn: &TurnRow, credits: Option<f64>, raw_tokens: i64) {
        self.raw_tokens = self.raw_tokens.saturating_add(raw_tokens.max(0));
        if let Some(credits) = credits {
            self.estimated_credits = Some(self.estimated_credits.unwrap_or(0.0) + credits);
        } else {
            self.low_confidence = true;
        }
        self.turn_count += 1;
        if matches!(turn.confidence, Confidence::Low | Confidence::Unknown) {
            self.low_confidence = true;
        }
    }

    fn confidence(&self) -> Confidence {
        if self.turn_count == 0 {
            return Confidence::Unknown;
        }
        if self.low_confidence {
            Confidence::Low
        } else if self.turn_count == 1 {
            Confidence::High
        } else {
            Confidence::Medium
        }
    }
}

#[derive(Clone, Debug)]
struct IntervalRow {
    account_key: String,
    start_at: i64,
    end_at: i64,
    observed_delta_percent: f64,
}

#[derive(Clone, Debug, Default)]
struct DailyAggregate {
    official_tokens: Option<i64>,
    local_tokens: i64,
    estimated_credits: Option<f64>,
    observed_quota_percent: f64,
    attributable_quota_percent: f64,
    unattributed_quota_percent: f64,
    turn_count: i64,
    categories: BTreeMap<String, Aggregate>,
}

fn parse_timezone(value: &str) -> FixedOffset {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("utc") || trimmed == "z" {
        return FixedOffset::east_opt(0).unwrap();
    }
    if let Some((sign, rest)) = trimmed.split_at_checked(1) {
        if (sign == "+" || sign == "-") && rest.len() == 5 && rest.as_bytes()[3] == b':' {
            if let (Ok(hours), Ok(minutes)) = (rest[..2].parse::<i32>(), rest[3..].parse::<i32>()) {
                let seconds = (hours * 60 + minutes) * 60;
                return FixedOffset::east_opt(if sign == "+" { seconds } else { -seconds })
                    .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
            }
        }
    }
    FixedOffset::east_opt(Local::now().offset().local_minus_utc())
        .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap())
}

fn bounds(
    query: &UsageAnalyticsQuery,
) -> Result<(NaiveDate, NaiveDate, FixedOffset, i64, i64), String> {
    let from =
        NaiveDate::parse_from_str(&query.from, "%Y-%m-%d").map_err(|error| error.to_string())?;
    let to = NaiveDate::parse_from_str(&query.to, "%Y-%m-%d").map_err(|error| error.to_string())?;
    if to < from {
        return Err("Usage analytics 'to' must not precede 'from'".into());
    }
    let offset = parse_timezone(&query.timezone);
    let start = offset
        .from_local_datetime(&from.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp();
    let end = offset
        .from_local_datetime(
            &(to.succ_opt()
                .ok_or("Invalid analytics date")?
                .and_hms_opt(0, 0, 0)
                .unwrap()),
        )
        .single()
        .unwrap()
        .timestamp();
    Ok((from, to, offset, start, end))
}

fn dates(from: NaiveDate, to: NaiveDate) -> Vec<String> {
    let mut result = Vec::new();
    let mut cursor = from;
    while cursor <= to {
        result.push(cursor.to_string());
        let Some(next) = cursor.succ_opt() else { break };
        cursor = next;
    }
    result
}

fn date_for(timestamp: i64, offset: FixedOffset) -> String {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .with_timezone(&offset)
        .date_naive()
        .to_string()
}

fn scope_keys(connection: &Connection, scope: &AccountScope) -> Result<Vec<String>, String> {
    match scope {
        AccountScope::Single { account_key } if is_unresolved_account_key(account_key) => {
            Ok(Vec::new())
        }
        AccountScope::Single { account_key } => {
            let legacy: bool = connection
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM account_usage_data_versions
                       WHERE account_key = ?1 AND status = 'legacy_unverified'
                     )",
                    [account_key],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if legacy {
                Ok(Vec::new())
            } else {
                Ok(vec![account_key.clone()])
            }
        }
        AccountScope::All => {
            let mut statement = connection
                .prepare(
                    "SELECT account_key FROM accounts
                     WHERE account_key NOT LIKE 'unresolved:%'
                       AND NOT EXISTS (
                         SELECT 1 FROM account_usage_data_versions h
                         WHERE h.account_key = accounts.account_key
                           AND h.status = 'legacy_unverified'
                       )
                 UNION SELECT account_key FROM turn_usage
                     WHERE account_key NOT LIKE 'unresolved:%'
                       AND NOT EXISTS (
                         SELECT 1 FROM account_usage_data_versions h
                         WHERE h.account_key = turn_usage.account_key
                           AND h.status = 'legacy_unverified'
                       )
                 UNION SELECT account_key FROM account_daily_usage
                     WHERE account_key NOT LIKE 'unresolved:%'
                       AND NOT EXISTS (
                         SELECT 1 FROM account_usage_data_versions h
                         WHERE h.account_key = account_daily_usage.account_key
                           AND h.status = 'legacy_unverified'
                       )
                 ORDER BY account_key",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row.map_err(|error| error.to_string())?);
            }
            Ok(result)
        }
    }
}

fn load_turns(
    connection: &Connection,
    keys: &[String],
    start: i64,
    end: i64,
) -> Result<Vec<TurnRow>, String> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT account_key, started_at, model, reasoning_effort, speed_mode,
                input_tokens, cached_input_tokens, output_tokens,
                reasoning_output_tokens, raw_total_tokens, confidence
         FROM turn_usage WHERE started_at >= ?1 AND started_at < ?2 ORDER BY started_at",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![start, end], |row| {
            let account_key: String = row.get(0)?;
            let confidence: String = row.get(10)?;
            Ok(TurnRow {
                account_key,
                started_at: row.get(1)?,
                model: row.get(2)?,
                reasoning_effort: row.get(3)?,
                speed_mode: row.get(4)?,
                input_tokens: row.get(5)?,
                cached_input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                reasoning_output_tokens: row.get(8)?,
                raw_total_tokens: row.get(9)?,
                confidence: quota::confidence_from_str(&confidence),
            })
        })
        .map_err(|error| error.to_string())?;
    let allowed: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let mut result = Vec::new();
    for row in rows {
        let row = row.map_err(|error| error.to_string())?;
        if allowed.contains(row.account_key.as_str()) {
            result.push(row);
        }
    }
    Ok(result)
}

fn load_official(
    connection: &Connection,
    keys: &[String],
    from: &str,
    to: &str,
) -> Result<BTreeMap<(String, String), i64>, String> {
    let mut statement = connection.prepare("SELECT account_key, date, official_tokens FROM account_daily_usage WHERE date >= ?1 AND date <= ?2").map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![from, to], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let allowed: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let mut result = BTreeMap::new();
    for row in rows {
        let (account, date, tokens) = row.map_err(|error| error.to_string())?;
        if allowed.contains(account.as_str()) {
            result.insert((account, date), tokens.max(0));
        }
    }
    Ok(result)
}

fn load_intervals(
    connection: &Connection,
    keys: &[String],
    start: i64,
    end: i64,
) -> Result<Vec<IntervalRow>, String> {
    let mut statement = connection
        .prepare(
            "SELECT account_key, start_at, end_at, observed_delta_percent
         FROM quota_intervals WHERE end_at > ?1 AND start_at < ?2
           AND window = 'primary'
         ORDER BY end_at",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![start, end], |row| {
            Ok(IntervalRow {
                account_key: row.get(0)?,
                start_at: row.get(1)?,
                end_at: row.get(2)?,
                observed_delta_percent: row.get(3)?,
            })
        })
        .map_err(|error| error.to_string())?;
    let allowed: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let mut result = Vec::new();
    for row in rows {
        let row = row.map_err(|error| error.to_string())?;
        if allowed.contains(row.account_key.as_str()) {
            result.push(row);
        }
    }
    Ok(result)
}

fn category_for(breakdown: &str, turn: &TurnRow) -> String {
    match breakdown {
        "reasoning" => match turn.reasoning_effort.as_str() {
            "low" => "低",
            "medium" => "中",
            "high" => "高",
            "xhigh" => "极高",
            "ultra" => "超高",
            _ => "未知",
        }
        .into(),
        "speed" => match turn.speed_mode.as_str() {
            "standard" => "标准",
            "fast_requested" => "已请求快速模式",
            _ => "未知",
        }
        .into(),
        "account" => turn.account_key.clone(),
        "tokenType" => "全部 Token".into(),
        _ => turn.model.clone().unwrap_or_else(|| "未知".into()),
    }
}

fn add_aggregate(
    map: &mut BTreeMap<String, Aggregate>,
    category: String,
    turn: &TurnRow,
    credits: Option<f64>,
    raw: i64,
) {
    map.entry(category).or_default().add(turn, credits, raw);
}

fn credits_for(card: &RateCard, turn: &TurnRow) -> Option<f64> {
    card.calculate(
        turn.model.as_deref(),
        &turn.speed_mode,
        &TokenUsage {
            input_tokens: turn.input_tokens,
            cached_input_tokens: turn.cached_input_tokens,
            output_tokens: turn.output_tokens,
            reasoning_output_tokens: turn.reasoning_output_tokens,
            raw_total_tokens: turn.raw_total_tokens,
        },
    )
}

fn add_turn_tokens(
    map: &mut BTreeMap<String, Aggregate>,
    breakdown: &str,
    turn: &TurnRow,
    credits: Option<f64>,
) {
    if breakdown != "tokenType" {
        add_aggregate(
            map,
            category_for(breakdown, turn),
            turn,
            credits,
            turn.raw_total_tokens,
        );
        return;
    }
    let parts = [
        ("输入", turn.input_tokens),
        ("缓存输入", turn.cached_input_tokens),
        (
            "输出",
            (turn.output_tokens - turn.reasoning_output_tokens).max(0),
        ),
        ("推理", turn.reasoning_output_tokens),
    ];
    for (category, raw) in parts {
        if raw > 0 {
            add_aggregate(
                map,
                category.into(),
                turn,
                credits.map(|value| value * raw as f64 / turn.raw_total_tokens.max(1) as f64),
                raw,
            );
        }
    }
}

fn latest_quota(
    connection: &Connection,
    account_key: &str,
) -> Result<(Option<f64>, Option<f64>, Option<i64>), String> {
    connection
        .query_row(
            "SELECT used_percent, resets_at FROM rate_limit_samples
         WHERE account_key = ?1 AND window = 'primary'
         ORDER BY (window_duration_mins = 10080) DESC, sampled_at DESC LIMIT 1",
            params![account_key],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .optional()
        .map(|value| {
            value
                .map(|(used, reset)| (Some(used), Some((100.0 - used).max(0.0)), reset))
                .unwrap_or((None, None, None))
        })
        .map_err(|error| error.to_string())
}

fn account_display(connection: &Connection, account_key: &str) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT display_name FROM accounts WHERE account_key = ?1",
            params![account_key],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(|value| value.flatten())
        .map_err(|error| error.to_string())
}

fn range_label(from: &str, to: &str) -> String {
    if from == to {
        "1d".into()
    } else {
        format!("{from}:{to}")
    }
}

pub fn query(
    connection: &Connection,
    request: &UsageAnalyticsQuery,
) -> Result<UsageAnalytics, String> {
    let (from, to, offset, start, end) = bounds(request)?;
    let dates = dates(from, to);
    let keys = scope_keys(connection, &request.account_scope)?;
    let turns = load_turns(connection, &keys, start, end)?;
    let official = load_official(connection, &keys, &from.to_string(), &to.to_string())?;
    let intervals = load_intervals(connection, &keys, start, end)?;
    let card = RateCard::current();

    let mut daily: BTreeMap<String, DailyAggregate> = dates
        .iter()
        .map(|date| (date.clone(), DailyAggregate::default()))
        .collect();
    let mut account_data: HashMap<String, (i64, Option<f64>, i64, BTreeSet<String>)> =
        HashMap::new();
    let mut turn_category: HashMap<usize, String> = HashMap::new();
    for (index, turn) in turns.iter().enumerate() {
        let date = date_for(turn.started_at, offset);
        let credits = credits_for(&card, turn);
        let entry = daily.entry(date.clone()).or_default();
        entry.local_tokens = entry
            .local_tokens
            .saturating_add(turn.raw_total_tokens.max(0));
        entry.turn_count += 1;
        if let Some(value) = credits {
            entry.estimated_credits = Some(entry.estimated_credits.unwrap_or(0.0) + value);
        }
        let category = category_for(&request.breakdown, turn);
        turn_category.insert(index, category.clone());
        add_turn_tokens(&mut entry.categories, &request.breakdown, turn, credits);
        let account =
            account_data
                .entry(turn.account_key.clone())
                .or_insert((0, None, 0, BTreeSet::new()));
        account.0 = account.0.saturating_add(turn.raw_total_tokens.max(0));
        if let Some(value) = credits {
            account.1 = Some(account.1.unwrap_or(0.0) + value);
        }
        account.2 += 1;
        account.3.insert(date);
    }
    for ((account, date), tokens) in official {
        if let Some(entry) = daily.get_mut(&date) {
            entry.official_tokens = Some(entry.official_tokens.unwrap_or(0).saturating_add(tokens));
            let local = entry.local_tokens;
            if tokens > local {
                entry
                    .categories
                    .entry("Unattributed".into())
                    .or_default()
                    .raw_tokens = tokens - local;
            }
        }
        let account_entry = account_data
            .entry(account)
            .or_insert((0, None, 0, BTreeSet::new()));
        account_entry.3.insert(date);
    }

    for interval in &intervals {
        let local_indices: Vec<usize> = turns
            .iter()
            .enumerate()
            .filter(|(_, turn)| {
                turn.account_key == interval.account_key
                    && turn.started_at > interval.start_at
                    && turn.started_at <= interval.end_at
            })
            .map(|(index, _)| index)
            .collect();
        let local_credits: f64 = local_indices
            .iter()
            .filter_map(|index| credits_for(&card, &turns[*index]))
            .sum();
        if local_credits > 0.0 {
            for index in local_indices {
                let turn = &turns[index];
                let contribution = interval.observed_delta_percent
                    * credits_for(&card, turn).unwrap_or(0.0)
                    / local_credits;
                let date = date_for(turn.started_at, offset);
                let entry = daily.entry(date).or_default();
                entry.attributable_quota_percent += contribution;
                if let Some(category) = turn_category.get(&index) {
                    entry
                        .categories
                        .entry(category.clone())
                        .or_default()
                        .attributed_quota_percent += contribution;
                }
            }
        } else {
            let date = date_for(interval.end_at, offset);
            let entry = daily.entry(date).or_default();
            entry.unattributed_quota_percent += interval.observed_delta_percent;
            entry
                .categories
                .entry("Unattributed".into())
                .or_default()
                .attributed_quota_percent += interval.observed_delta_percent;
        }
        let date = date_for(interval.end_at, offset);
        daily.entry(date).or_default().observed_quota_percent += interval.observed_delta_percent;
    }
    for entry in daily.values_mut() {
        entry.unattributed_quota_percent += (entry.observed_quota_percent
            - entry.attributable_quota_percent
            - entry.unattributed_quota_percent)
            .max(0.0);
    }

    let timeline: Vec<DailyUsageAnalytics> = dates
        .iter()
        .map(|date| {
            let entry = daily.get(date).cloned().unwrap_or_default();
            let categories = entry
                .categories
                .into_iter()
                .map(|(key, value)| {
                    let source = if key == "Unattributed" {
                        "official".into()
                    } else if value.attributed_quota_percent > 0.0 {
                        PROVENANCE_DERIVED_ESTIMATE.into()
                    } else {
                        "local".into()
                    };
                    (
                        key,
                        DailyUsageCategory {
                            raw_tokens: value.raw_tokens,
                            estimated_credits: value.estimated_credits,
                            attributed_quota_percent: if value.attributed_quota_percent > 0.0 {
                                Some(value.attributed_quota_percent)
                            } else {
                                None
                            },
                            source,
                            confidence: value.confidence(),
                        },
                    )
                })
                .collect();
            DailyUsageAnalytics {
                date: date.clone(),
                local_tokens: entry.local_tokens,
                raw_tokens: entry.official_tokens.unwrap_or(entry.local_tokens),
                official_tokens: entry.official_tokens,
                estimated_credits: entry.estimated_credits,
                observed_quota_percent: if entry.observed_quota_percent > 0.0 {
                    Some(entry.observed_quota_percent)
                } else {
                    None
                },
                attributable_quota_percent: if entry.attributable_quota_percent > 0.0 {
                    Some(entry.attributable_quota_percent)
                } else {
                    None
                },
                unattributed_quota_percent: if entry.unattributed_quota_percent > 0.0 {
                    Some(entry.unattributed_quota_percent)
                } else {
                    None
                },
                turn_count: entry.turn_count,
                categories,
            }
        })
        .collect();

    let mut breakdowns: BTreeMap<String, Aggregate> = BTreeMap::new();
    for entry in daily.values() {
        for (key, value) in &entry.categories {
            let aggregate = breakdowns.entry(key.clone()).or_default();
            aggregate.raw_tokens += value.raw_tokens;
            aggregate.estimated_credits =
                match (aggregate.estimated_credits, value.estimated_credits) {
                    (Some(a), Some(b)) => Some(a + b),
                    (None, Some(b)) => Some(b),
                    _ => aggregate.estimated_credits,
                };
            aggregate.attributed_quota_percent += value.attributed_quota_percent;
            aggregate.turn_count += value.turn_count;
        }
    }
    let official_total: i64 = timeline.iter().filter_map(|day| day.official_tokens).sum();
    let local_total: i64 = timeline.iter().map(|day| day.local_tokens).sum();
    let raw_total = timeline.iter().map(|day| day.raw_tokens).sum();
    let credits_total = turns
        .iter()
        .filter_map(|turn| credits_for(&card, turn))
        .sum::<f64>();
    let observed_quota: f64 = timeline
        .iter()
        .filter_map(|day| day.observed_quota_percent)
        .sum();
    let attributable_quota: f64 = timeline
        .iter()
        .filter_map(|day| day.attributable_quota_percent)
        .sum();
    let unattributed_quota: f64 = timeline
        .iter()
        .filter_map(|day| day.unattributed_quota_percent)
        .sum();
    let active_days = timeline
        .iter()
        .filter(|day| day.raw_tokens > 0 || day.turn_count > 0)
        .count() as i64;
    let breakdown_total_quota = attributable_quota;
    let breakdown_items = breakdowns
        .into_iter()
        .map(|(key, value)| {
            let source = if key == "Unattributed" {
                "official".into()
            } else if value.attributed_quota_percent > 0.0 {
                PROVENANCE_DERIVED_ESTIMATE.into()
            } else {
                "local".into()
            };
            UsageBreakdownItem {
                key: key.clone(),
                label: key.clone(),
                raw_tokens: value.raw_tokens,
                raw_token_share: if raw_total > 0 {
                    value.raw_tokens as f64 / raw_total as f64
                } else {
                    0.0
                },
                estimated_credits: value.estimated_credits,
                attributed_quota_percent: if value.attributed_quota_percent > 0.0 {
                    Some(value.attributed_quota_percent)
                } else {
                    None
                },
                quota_share: if breakdown_total_quota > 0.0 {
                    Some(value.attributed_quota_percent / breakdown_total_quota)
                } else {
                    None
                },
                source,
                confidence: value.confidence(),
            }
        })
        .collect::<Vec<_>>();

    let mut accounts = Vec::new();
    for account_key in keys {
        let (raw_tokens, credits, turn_count, active_dates) =
            account_data.remove(&account_key).unwrap_or_default();
        let (current_used, remaining, resets_at) = latest_quota(connection, &account_key)?;
        let observed = intervals
            .iter()
            .filter(|interval| interval.account_key == account_key)
            .map(|interval| interval.observed_delta_percent)
            .sum();
        accounts.push(AccountUsageSummary {
            display_name: account_display(connection, &account_key)?,
            account_key,
            raw_tokens,
            estimated_credits: credits,
            observed_quota_percent: if observed > 0.0 { Some(observed) } else { None },
            current_used_percent: current_used,
            remaining_percent: remaining,
            resets_at,
            active_days: active_dates.len() as i64,
            turn_count,
        });
    }

    let legacy_points = timeline
        .iter()
        .map(|day| UsageAnalyticsPoint {
            date: day.date.clone(),
            official_tokens: day.official_tokens,
            local_tokens: day.local_tokens,
            unattributed_tokens: day
                .official_tokens
                .map(|value| (value - day.local_tokens).max(0))
                .unwrap_or(0),
            category_values: day
                .categories
                .iter()
                .map(|(key, value)| (key.clone(), value.raw_tokens))
                .collect(),
        })
        .collect::<Vec<_>>();
    let categories = legacy_points
        .iter()
        .flat_map(|point| point.category_values.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let first_account = match &request.account_scope {
        AccountScope::Single { account_key } => Some(account_key.clone()),
        AccountScope::All => None,
    };
    Ok(UsageAnalytics {
        scope: request.account_scope.clone(),
        period: UsagePeriod {
            from: from.to_string(),
            to: to.to_string(),
            timezone: request.timezone.clone(),
        },
        summary: UsageSummary {
            raw_tokens: raw_total,
            estimated_credits: if credits_total > 0.0 {
                Some(credits_total)
            } else {
                None
            },
            observed_quota_percent: if observed_quota > 0.0 {
                Some(observed_quota)
            } else {
                None
            },
            attributable_quota_percent: if attributable_quota > 0.0 {
                Some(attributable_quota)
            } else {
                None
            },
            unattributed_quota_percent: if unattributed_quota > 0.0 {
                Some(unattributed_quota)
            } else {
                None
            },
            active_days,
            turn_count: turns.len() as i64,
        },
        breakdown_items,
        timeline,
        accounts,
        account_key: first_account,
        range: range_label(&from.to_string(), &to.to_string()),
        breakdown: request.breakdown.clone(),
        categories,
        points: legacy_points,
        turn_count: turns.len() as i64,
        official_total_tokens: official_total,
        local_total_tokens: local_total,
        estimated_remaining_tokens: None,
        estimate_sample_count: 0,
    })
}

pub fn legacy_query(
    connection: &Connection,
    range: &str,
    breakdown: &str,
) -> Result<UsageAnalytics, String> {
    let today = Local::now().date_naive();
    let from = match range {
        "7d" => today - chrono::Duration::days(6),
        "15d" => today - chrono::Duration::days(14),
        "30d" => today - chrono::Duration::days(29),
        "90d" => today - chrono::Duration::days(89),
        "all" => connection
            .query_row("SELECT MIN(date) FROM account_daily_usage", [], |row| {
                row.get::<_, Option<String>>(0)
            })
            .map_err(|error| error.to_string())?
            .and_then(|value| NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok())
            .unwrap_or(today),
        _ => return Err(format!("Unknown analytics range: {range}")),
    };
    let account_scope = recorder::current_account_key(connection)?
        .map(|account_key| AccountScope::Single { account_key })
        .unwrap_or(AccountScope::All);
    query(
        connection,
        &UsageAnalyticsQuery {
            account_scope,
            from: from.to_string(),
            to: today.to_string(),
            timezone: "local".into(),
            breakdown: breakdown.into(),
        },
    )
}

pub fn app_query(
    app: &tauri::AppHandle<tauri::Wry>,
    request: &UsageAnalyticsQuery,
) -> Result<UsageAnalytics, String> {
    let connection = db::open_database(app)?;
    query(&connection, request)
}

pub fn app_legacy_query(
    app: &tauri::AppHandle<tauri::Wry>,
    range: &str,
    breakdown: &str,
) -> Result<UsageAnalytics, String> {
    let connection = db::open_database(app)?;
    legacy_query(&connection, range, breakdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::db::initialize_schema;

    fn insert_account(connection: &Connection, account_key: &str) {
        connection
            .execute(
                "INSERT INTO accounts (account_key, first_seen_at, last_seen_at) VALUES (?1, 1, 1)",
                params![account_key],
            )
            .unwrap();
    }

    fn insert_turn(
        connection: &Connection,
        account_key: &str,
        turn_id: &str,
        timestamp: i64,
        tokens: i64,
    ) {
        connection
            .execute(
                "INSERT INTO turn_usage
                 (account_key, thread_id, turn_id, started_at, completed_at, model,
                  reasoning_effort, speed_mode, input_tokens, cached_input_tokens,
                  output_tokens, reasoning_output_tokens, raw_total_tokens,
                  estimated_credits, rate_card_version, source, confidence, created_at, updated_at)
                 VALUES (?1, 'thread', ?2, ?3, ?3, 'gpt-5.6-sol', 'high', 'standard',
                         ?4, 0, 0, 0, ?4, 1.0, 'test', 'rollout', 'high', 1, 1)",
                params![account_key, turn_id, timestamp, tokens],
            )
            .unwrap();
    }

    #[test]
    fn all_accounts_aggregate_tokens_but_keep_account_status_separate() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        insert_account(&connection, "personal");
        insert_account(&connection, "work");
        insert_turn(&connection, "personal", "p1", 1_786_694_400, 100);
        insert_turn(&connection, "work", "w1", 1_786_694_401, 50);
        for (account, tokens) in [("personal", 100), ("work", 50)] {
            connection.execute(
                "INSERT INTO account_daily_usage (account_key, date, official_tokens, fetched_at, source, confidence)
                 VALUES (?1, '2026-08-14', ?2, 1, 'official', 'high')",
                params![account, tokens],
            ).unwrap();
        }
        let result = query(
            &connection,
            &UsageAnalyticsQuery {
                account_scope: AccountScope::All,
                from: "2026-08-14".into(),
                to: "2026-08-14".into(),
                timezone: "+00:00".into(),
                breakdown: "account".into(),
            },
        )
        .unwrap();
        assert_eq!(result.summary.raw_tokens, 150);
        assert_eq!(result.accounts.len(), 2);
        assert_eq!(result.timeline.len(), 1);
        assert_eq!(result.timeline[0].official_tokens, Some(150));
        assert!(result
            .accounts
            .iter()
            .all(|account| account.remaining_percent.is_none()));
    }

    #[test]
    fn timezone_changes_turn_day_without_dropping_zero_buckets() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        insert_account(&connection, "a");
        insert_turn(&connection, "a", "t1", 1_786_665_300, 42); // 2026-08-13 16:35 UTC
        let result = query(
            &connection,
            &UsageAnalyticsQuery {
                account_scope: AccountScope::Single {
                    account_key: "a".into(),
                },
                from: "2026-08-14".into(),
                to: "2026-08-16".into(),
                timezone: "+08:00".into(),
                breakdown: "model".into(),
            },
        )
        .unwrap();
        assert_eq!(result.timeline.len(), 3);
        assert_eq!(result.timeline[0].local_tokens, 42);
        assert_eq!(result.timeline[1].local_tokens, 0);
        assert_eq!(result.timeline[2].local_tokens, 0);
    }

    #[test]
    fn observed_quota_is_attributed_by_credits_and_not_raw_tokens() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        insert_account(&connection, "a");
        insert_turn(&connection, "a", "t1", 1_786_694_550, 100);
        connection.execute(
            "INSERT INTO rate_limit_samples
             (account_key, sampled_at, limit_id, window, window_duration_mins, used_percent, resets_at, source, confidence)
             VALUES ('a', 1_786_694_400, 'codex', 'primary', 10080, 10, 99, 'official', 'high'),
                    ('a', 1_786_694_500, 'codex', 'primary', 10080, 11, 99, 'official', 'high'),
                    ('a', 1_786_694_600, 'codex', 'primary', 10080, 12, 99, 'official', 'high')",
            [],
        ).unwrap();
        quota::refresh_intervals(&connection, "a", "codex", "primary").unwrap();
        let result = query(
            &connection,
            &UsageAnalyticsQuery {
                account_scope: AccountScope::Single {
                    account_key: "a".into(),
                },
                from: "2026-08-14".into(),
                to: "2026-08-14".into(),
                timezone: "+00:00".into(),
                breakdown: "model".into(),
            },
        )
        .unwrap();
        assert_eq!(result.summary.observed_quota_percent, Some(1.0));
        assert_eq!(result.summary.attributable_quota_percent, Some(1.0));
        assert_eq!(result.summary.unattributed_quota_percent, None);
    }
}
