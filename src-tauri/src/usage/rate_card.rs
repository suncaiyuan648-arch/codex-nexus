use super::models::TokenUsage;
use serde::Serialize;

pub const CURRENT_RATE_CARD_VERSION: &str = "2026-04-token-v1";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRate {
    pub model: String,
    pub input_credits_per_million: f64,
    pub cached_input_credits_per_million: f64,
    pub output_credits_per_million: f64,
    pub fast_multiplier: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct RateCard;

impl RateCard {
    pub fn current() -> Self {
        Self
    }

    pub fn rate_for(&self, model: &str) -> Option<ModelRate> {
        let normalized = model.trim().to_ascii_lowercase().replace('_', "-");
        let rate = match normalized.as_str() {
            "gpt-5.6-sol" | "gpt-5.6-sol-mini" => (125.0, 12.5, 750.0, Some(2.5)),
            "gpt-5.6-terra" => (50.0, 5.0, 300.0, Some(2.5)),
            "gpt-5.6-luna" => (5.0, 0.5, 30.0, Some(2.5)),
            "gpt-5.5" => (125.0, 12.5, 750.0, Some(2.5)),
            "gpt-5.5-cyber" => (312.5, 31.25, 1875.0, Some(2.5)),
            "gpt-5.4" => (62.5, 6.25, 375.0, Some(2.0)),
            "gpt-5.4-mini" | "gpt-5.4-mini-codex" => (18.75, 1.875, 113.0, None),
            "gpt-5.3-codex" => (43.75, 4.375, 350.0, None),
            "gpt-5.2" | "gpt-5.2-codex" => (43.75, 4.375, 350.0, None),
            _ => return None,
        };
        Some(ModelRate {
            model: model.to_owned(),
            input_credits_per_million: rate.0,
            cached_input_credits_per_million: rate.1,
            output_credits_per_million: rate.2,
            fast_multiplier: rate.3,
        })
    }

    pub fn calculate(
        &self,
        model: Option<&str>,
        speed_mode: &str,
        usage: &TokenUsage,
    ) -> Option<f64> {
        let model = model?;
        let rate = self.rate_for(model)?;
        weighted_credits(&rate, speed_mode, usage)
    }
}

pub fn weighted_credits(rate: &ModelRate, speed_mode: &str, usage: &TokenUsage) -> Option<f64> {
    let multiplier = match speed_mode {
        "standard" => 1.0,
        "fast_requested" => rate.fast_multiplier?,
        _ => return None,
    };
    Some(
        (usage.input_tokens as f64 / 1_000_000.0 * rate.input_credits_per_million
            + usage.cached_input_tokens as f64 / 1_000_000.0
                * rate.cached_input_credits_per_million
            + usage.output_tokens as f64 / 1_000_000.0 * rate.output_credits_per_million)
            * multiplier,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_card_does_not_invent_reasoning_multiplier() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            reasoning_output_tokens: 1_000_000,
            raw_total_tokens: 2_000_000,
            ..TokenUsage::default()
        };
        let card = RateCard::current();
        let normal = card
            .calculate(Some("gpt-5.6-sol"), "standard", &usage)
            .unwrap();
        let high = card
            .calculate(Some("gpt-5.6-sol"), "standard", &usage)
            .unwrap();
        assert_eq!(normal, high);
        assert_eq!(normal, 875.0);
    }

    #[test]
    fn unknown_speed_is_not_silently_standard() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        assert!(RateCard::current()
            .calculate(Some("gpt-5.6-sol"), "unknown", &usage)
            .is_none());
        assert_eq!(
            RateCard::current().calculate(Some("gpt-5.6-sol"), "fast_requested", &usage),
            Some(312.5)
        );
    }
}
