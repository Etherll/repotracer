use repotracer_repo_tools::ToolResult;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[async_trait::async_trait]
pub trait ScoutBackend: Send + Sync {
    async fn scout(&self, request: ScoutRequest) -> anyhow::Result<ScoutResult>;
}

/// A provider reached a terminal failure after reporting usage.
///
/// This remains an error for callers that need to stop the investigation, but
/// carries the provider accounting so transports such as MCP do not erase
/// work performed before a bounded failure.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ScoutBackendError {
    pub message: String,
    pub stats: ScoutStats,
}

impl ScoutBackendError {
    pub fn new(message: impl Into<String>, stats: ScoutStats) -> Self {
        Self {
            message: message.into(),
            stats,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScoutRequest {
    pub investigation: crate::InvestigationSpec,
    pub query: String,
    pub root: PathBuf,
    pub focus: Option<PathBuf>,
    pub max_turns: Option<u32>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedCitation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScoutStats {
    /// MCP-owned handle metadata, not a model claim or a provider thread ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationInfo>,
    #[serde(default)]
    pub warm_process: bool,
    #[serde(default)]
    pub thread_turn: u32,
    pub turns: u32,
    pub tool_calls: u32,
    pub duration_ms: u64,
    pub model: String,
    /// Effort sent to the native backend, not a measurement of hidden reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// None means telemetry is unknown, not that the index was unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_usage: Option<IndexUsage>,
    /// Per-attempt accounting when an investigation used a continuation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<ScoutAttemptStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u32>,
    /// Cache-write input is a subset of `prompt_tokens`, when the provider
    /// reports it. It is intentionally separate from cache reads because the
    /// two dimensions can have different pricing and retention semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_prompt_tokens: Option<u32>,
    /// Provider-reported total. RepoTracer never infers this from the other
    /// dimensions because providers may count hidden tokens differently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// Provider-reported total cost, when the native provider reports a
    /// finite non-negative value. This is not inferred from token usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_cost_usd: Option<f64>,
    /// Detailed cache-aware usage. The legacy fields above remain populated
    /// for callers that have not migrated to this shape.
    #[serde(default, skip_serializing_if = "UsageStats::is_empty")]
    pub usage: UsageStats,
    #[serde(default)]
    pub usage_status: UsageStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationInfo {
    pub id: String,
    pub repository: String,
    /// resumed, fresh, or unknown, based on the first native attempt.
    pub status: String,
}

/// Counts from the existing Symbols tool, without source text or search terms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexUsage {
    pub available: bool,
    pub calls: u32,
    pub failed_calls: u32,
    pub parsed_files: u64,
    pub reused_files: u64,
    pub incomplete_calls: u32,
    pub duration_ms: u64,
    pub output_bytes: u64,
}

/// Nonrecursive attempt record. Unknown usage remains absent even after failure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScoutAttemptStats {
    pub reasoning_effort: String,
    pub succeeded: bool,
    pub usage: UsageStats,
    pub usage_status: UsageStatus,
    pub reported_cost_usd: Option<f64>,
    pub tool_calls: u32,
    pub duration_ms: u64,
    pub warm_process: bool,
    pub thread_turn: u32,
    pub index_usage: Option<IndexUsage>,
}

/// Availability of provider-reported token usage for one product operation.
///
/// `Unknown` means no usage event was observed. `Partial` means some usage was
/// observed but at least one dimension was absent, or a generation ended
/// before its terminal usage could be trusted. Missing values stay `None` in
/// [`UsageStats`], rather than being represented by a misleading zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageStatus {
    Complete,
    Partial,
    #[default]
    Unknown,
}

/// Cache-aware token dimensions for a single request or an aggregate of
/// requests. Cache reads, cache writes, and reasoning output are subsets of
/// input or output respectively. Every field is optional because providers
/// can omit usage entirely or report only a subset of dimensions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
}

impl UsageStats {
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.cache_write_input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.reasoning_output_tokens.is_none()
            && self.total_tokens.is_none()
    }

    /// Copy detailed values into the pre-existing `ScoutStats` fields.
    /// Keeping this conversion in core prevents each provider adapter from
    /// accidentally changing the meaning of those compatibility fields.
    pub fn apply_to(&self, stats: &mut ScoutStats) {
        stats.prompt_tokens = self.input_tokens;
        stats.cached_prompt_tokens = self.cached_input_tokens;
        stats.cache_write_prompt_tokens = self.cache_write_input_tokens;
        stats.completion_tokens = self.output_tokens;
        stats.reasoning_output_tokens = self.reasoning_output_tokens;
        stats.total_tokens = self.total_tokens;
        stats.usage = self.clone();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoutResult {
    #[serde(default)]
    pub investigation: crate::InvestigationReport,
    pub summary: String,
    pub citations: Vec<ValidatedCitation>,
    pub stats: ScoutStats,
    /// Raw final assistant text (for debugging).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_final: Option<String>,
}

impl ScoutResult {
    /// Compact text for MCP / frontier model consumption.
    pub fn compact_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Investigation: {:?}\n", self.investigation.status));
        for finding in &self.investigation.findings {
            out.push_str(&format!(
                "\nQuestion: {}\nFinding: {}\n",
                finding.question, finding.answer
            ));
            for citation in &finding.citations {
                out.push_str(&format!(
                    "Evidence: {}:{}-{}\n",
                    citation.path, citation.start_line, citation.end_line
                ));
            }
        }
        for scope in &self.investigation.searched_scope {
            out.push_str(&format!("Searched scope: {scope}\n"));
        }
        for question in &self.investigation.unresolved {
            out.push_str(&format!("Unresolved: {question}\n"));
        }
        for limitation in &self.investigation.limitations {
            out.push_str(&format!("Limitation: {limitation}\n"));
        }
        if !self.summary.is_empty() {
            out.push_str(&self.summary);
            out.push_str("\n\n");
        }
        if self.citations.is_empty() {
            out.push_str("No validated citations.");
        } else {
            out.push_str("Citations:\n");
            for c in &self.citations {
                if let Some(r) = &c.reason {
                    out.push_str(&format!(
                        "- {}:{}-{} — {}\n",
                        c.path, c.start_line, c.end_line, r
                    ));
                } else {
                    out.push_str(&format!("- {}:{}-{}\n", c.path, c.start_line, c.end_line));
                }
            }
        }
        out.push_str(&format!(
            "\n(scout: {} · {} model steps · {} tools · {} ms)",
            self.stats.model, self.stats.turns, self.stats.tool_calls, self.stats.duration_ms
        ));
        out
    }

    pub fn cli_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Investigation: {:?} ({:?})\n",
            self.investigation.status, self.investigation.intent
        ));
        for question in &self.investigation.unresolved {
            out.push_str(&format!("Unresolved: {question}\n"));
        }
        let n = self.citations.len();
        out.push_str(&format!(
            "Found {n} relevant location{} in {:.1}s\n\n",
            if n == 1 { "" } else { "s" },
            self.stats.duration_ms as f64 / 1000.0
        ));
        for c in &self.citations {
            out.push_str(&format!("{}:{}-{}\n", c.path, c.start_line, c.end_line));
            if let Some(r) = &c.reason {
                let reason = r.trim().trim_start_matches('(').trim_end_matches(')');
                out.push_str(&format!("  {reason}\n"));
            }
            out.push('\n');
        }
        if !self.summary.is_empty() {
            out.push_str(&format!("Summary: {}\n\n", self.summary.trim()));
        }
        out.push_str(&format!(
            "Scout: {}\nModel steps: {}\nTool calls: {}\n",
            self.stats.model, self.stats.turns, self.stats.tool_calls
        ));
        out
    }
}

#[derive(Debug, Clone)]
pub struct ExplorerTurn {
    pub index: u32,
    pub assistant_text: Option<String>,
    pub tool_results: Vec<ToolResult>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_stats_do_not_claim_index_unavailable_or_unused() {
        let stats: ScoutStats = serde_json::from_value(json!({
            "turns": 1, "tool_calls": 0, "duration_ms": 1, "model": "legacy"
        }))
        .unwrap();
        assert!(stats.index_usage.is_none());
        assert!(stats.reasoning_effort.is_none());
        assert!(stats.attempts.is_empty());
    }

    #[test]
    fn unavailable_and_unused_index_are_distinct() {
        let unavailable = serde_json::to_value(IndexUsage::default()).unwrap();
        let unused = serde_json::to_value(IndexUsage {
            available: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(unavailable["available"], false);
        assert_eq!(unused["available"], true);
        assert_eq!(unused["calls"], 0);
    }

    #[test]
    fn attempt_usage_is_not_lost_when_aggregate_is_unknown() {
        let stats = ScoutStats {
            attempts: vec![
                ScoutAttemptStats {
                    reasoning_effort: "medium".into(),
                    succeeded: true,
                    usage: UsageStats {
                        input_tokens: Some(100),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ScoutAttemptStats {
                    reasoning_effort: "high".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let encoded = serde_json::to_value(stats).unwrap();
        assert!(encoded.get("usage").is_none());
        assert_eq!(encoded["attempts"][0]["usage"]["input_tokens"], 100);
        assert_eq!(encoded["attempts"][1]["usage_status"], "unknown");
    }
}
