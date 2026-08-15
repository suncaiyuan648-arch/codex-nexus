use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SOURCE_OFFICIAL: &str = "official";
pub const SOURCE_ROLLOUT: &str = "rollout";
pub const SOURCE_APP_SERVER: &str = "app-server";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Confidence {
    High,
    Medium,
    Low,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AccountScope {
    Single { account_key: String },
    All,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyModelUsageCategory {
    pub model: String,
    pub reasoning_effort: String,
    pub speed_mode: String,
    pub raw_tokens: i64,
    pub turn_count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyModelUsage {
    pub date: String,
    pub account_key: Option<String>,
    pub official_tokens: Option<i64>,
    pub categories: Vec<DailyModelUsageCategory>,
    /// The current Codex RPCs expose weekly quota at account/window level only.
    /// This field intentionally stays explicit instead of allocating quota to
    /// model categories with a local price/rate-card heuristic.
    pub model_quota_attribution: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAnalyticsQuery {
    pub account_scope: AccountScope,
    pub from: String,
    pub to: String,
    pub timezone: String,
    pub breakdown: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePeriod {
    pub from: String,
    pub to: String,
    pub timezone: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub raw_tokens: i64,
    pub estimated_credits: Option<f64>,
    pub observed_quota_percent: Option<f64>,
    pub attributable_quota_percent: Option<f64>,
    pub unattributed_quota_percent: Option<f64>,
    pub active_days: i64,
    pub turn_count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdownItem {
    pub key: String,
    pub label: String,
    pub raw_tokens: i64,
    pub raw_token_share: f64,
    pub estimated_credits: Option<f64>,
    pub attributed_quota_percent: Option<f64>,
    pub quota_share: Option<f64>,
    pub source: String,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsageCategory {
    pub raw_tokens: i64,
    pub estimated_credits: Option<f64>,
    pub attributed_quota_percent: Option<f64>,
    pub source: String,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsageAnalytics {
    pub date: String,
    pub local_tokens: i64,
    pub raw_tokens: i64,
    pub official_tokens: Option<i64>,
    pub estimated_credits: Option<f64>,
    pub observed_quota_percent: Option<f64>,
    pub attributable_quota_percent: Option<f64>,
    pub unattributed_quota_percent: Option<f64>,
    pub turn_count: i64,
    pub categories: BTreeMap<String, DailyUsageCategory>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsageSummary {
    pub account_key: String,
    pub display_name: Option<String>,
    pub raw_tokens: i64,
    pub estimated_credits: Option<f64>,
    pub observed_quota_percent: Option<f64>,
    pub current_used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub active_days: i64,
    pub turn_count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAnalyticsPoint {
    pub date: String,
    pub official_tokens: Option<i64>,
    pub local_tokens: i64,
    pub unattributed_tokens: i64,
    pub category_values: BTreeMap<String, i64>,
}

/// Compatibility fields are kept while the React layer migrates to the V1 DTO.
/// New consumers should use `scope`, `period`, `summary`, `breakdown_items`, and
/// `timeline`; none of these values contain prompt, response, code, or tool data.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAnalytics {
    pub scope: AccountScope,
    pub period: UsagePeriod,
    pub summary: UsageSummary,
    pub breakdown_items: Vec<UsageBreakdownItem>,
    pub timeline: Vec<DailyUsageAnalytics>,
    pub accounts: Vec<AccountUsageSummary>,

    pub account_key: Option<String>,
    pub range: String,
    pub breakdown: String,
    pub categories: Vec<String>,
    pub points: Vec<UsageAnalyticsPoint>,
    pub turn_count: i64,
    pub official_total_tokens: i64,
    pub local_total_tokens: i64,
    pub estimated_remaining_tokens: Option<i64>,
    pub estimate_sample_count: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub raw_total_tokens: i64,
}

impl TokenUsage {
    pub fn normalized(mut self) -> Self {
        self.input_tokens = self.input_tokens.max(0);
        self.cached_input_tokens = self.cached_input_tokens.max(0);
        self.output_tokens = self.output_tokens.max(0);
        self.reasoning_output_tokens = self.reasoning_output_tokens.max(0).min(self.output_tokens);
        if self.raw_total_tokens <= 0 {
            self.raw_total_tokens = self.input_tokens.saturating_add(self.output_tokens);
        }
        self.raw_total_tokens = self.raw_total_tokens.max(0);
        self
    }
}

#[derive(Clone, Debug)]
pub struct TurnUsageRecord {
    pub account_key: String,
    pub thread_id: String,
    pub turn_id: String,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub model: Option<String>,
    pub reasoning_effort: String,
    pub speed_mode: String,
    pub usage: TokenUsage,
    pub estimated_credits: Option<f64>,
    pub rate_card_version: Option<String>,
    pub source: String,
    pub confidence: Confidence,
}
