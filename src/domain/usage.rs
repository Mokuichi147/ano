use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiOperation {
    Response,
    Compaction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    FinalAnswer,
    RoundLimit,
    TokenLimit,
    UsageUnavailable,
    /// The same calls kept repeating after the runtime asked for another
    /// approach, so the run stopped with a report.
    NoProgress,
}

/// Observed usage, not a bill estimate. Missing provider usage is counted
/// explicitly; retries and requests without a returned body cannot be measured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageSummary {
    pub responses: u64,
    pub compactions: u64,
    pub unreported_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl UsageSummary {
    pub(crate) fn from_response(response: &Value, operation: ApiOperation) -> Self {
        let mut usage = Self::default();
        match operation {
            ApiOperation::Response => usage.responses = 1,
            ApiOperation::Compaction => usage.compactions = 1,
        }
        let data = &response["usage"];
        match (
            data["input_tokens"].as_u64(),
            data["output_tokens"].as_u64(),
            data["total_tokens"].as_u64(),
        ) {
            (Some(input), Some(output), Some(total))
                if input.checked_add(output) == Some(total) =>
            {
                usage.input_tokens = input;
                usage.output_tokens = output;
                usage.total_tokens = total;
                usage.cached_input_tokens = data["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .min(input);
                usage.reasoning_tokens = data["output_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .min(output);
            }
            _ => usage.unreported_requests = 1,
        }
        usage
    }

    pub(crate) fn add(&mut self, other: &Self) {
        self.responses = self.responses.saturating_add(other.responses);
        self.compactions = self.compactions.saturating_add(other.compactions);
        self.unreported_requests = self
            .unreported_requests
            .saturating_add(other.unreported_requests);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }

    pub(crate) fn stop_reason(&self, limit: Option<u64>) -> Option<StopReason> {
        limit.and_then(|limit| {
            if self.unreported_requests > 0 {
                Some(StopReason::UsageUnavailable)
            } else if self.total_tokens >= limit {
                Some(StopReason::TokenLimit)
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_compaction_without_double_counting_cached_or_reasoning_tokens() {
        let mut usage = UsageSummary::from_response(
            &json!({"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,
            "input_tokens_details":{"cached_tokens":80},"output_tokens_details":{"reasoning_tokens":10}}}),
            ApiOperation::Response,
        );
        usage.add(&UsageSummary::from_response(
            &json!({"usage":{"input_tokens":30,"output_tokens":10,"total_tokens":40}}),
            ApiOperation::Compaction,
        ));
        assert_eq!(usage.total_tokens, 160);
        assert_eq!((usage.responses, usage.compactions), (1, 1));
        assert_eq!(usage.stop_reason(Some(160)), Some(StopReason::TokenLimit));
        assert_eq!(usage.stop_reason(Some(161)), None);
    }

    #[test]
    fn missing_or_inconsistent_usage_is_not_treated_as_free() {
        for response in [
            json!({}),
            json!({"usage":null}),
            json!({"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":1}}),
        ] {
            let usage = UsageSummary::from_response(&response, ApiOperation::Response);
            assert_eq!(usage.unreported_requests, 1);
            assert_eq!(
                usage.stop_reason(Some(100)),
                Some(StopReason::UsageUnavailable)
            );
            assert_eq!(usage.stop_reason(None), None);
        }
    }
}
