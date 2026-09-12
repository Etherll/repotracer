use crate::assess_output;
use crate::config::ExplorerBudget;
use crate::prompt::build_system_prompt;
use crate::types::{ScoutRequest, ScoutResult, ScoutStats, UsageStats, UsageStatus};
use repotracer_model::{ChatMessage, ModelBackend, ModelRequest, ToolSpec, Usage};
use repotracer_repo_tools::{resolve_in_root, RepoTools, ToolCall};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, warn};

/// Accumulates one reported usage object per model generation. OpenAI
/// Chat Completions usage is per request, so unlike the app-server transport
/// these values are additive across generations. `None` is sticky for a
/// dimension: a missing field in any generation makes that aggregate field
/// unknown instead of silently treating it as zero.
#[derive(Clone, Default)]
struct UsageAccumulator {
    observed: bool,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
    input_missing: bool,
    cached_input_missing: bool,
    cache_write_input_missing: bool,
    output_missing: bool,
    reasoning_output_missing: bool,
    total_missing: bool,
}

impl UsageAccumulator {
    fn add(&mut self, usage: &Usage) {
        self.observed = true;
        add_dimension(
            usage.prompt_tokens,
            &mut self.input_tokens,
            &mut self.input_missing,
        );
        add_dimension(
            usage.cached_prompt_tokens,
            &mut self.cached_input_tokens,
            &mut self.cached_input_missing,
        );
        add_dimension(
            usage.cache_write_prompt_tokens,
            &mut self.cache_write_input_tokens,
            &mut self.cache_write_input_missing,
        );
        add_dimension(
            usage.completion_tokens,
            &mut self.output_tokens,
            &mut self.output_missing,
        );
        add_dimension(
            usage.reasoning_output_tokens,
            &mut self.reasoning_output_tokens,
            &mut self.reasoning_output_missing,
        );
        add_dimension(
            usage.total_tokens,
            &mut self.total_tokens,
            &mut self.total_missing,
        );
    }

    fn finish(&self) -> (UsageStats, UsageStatus) {
        if !self.observed {
            return (UsageStats::default(), UsageStatus::Unknown);
        }
        let usage = UsageStats {
            input_tokens: known_total(self.input_tokens, self.input_missing),
            cached_input_tokens: known_total(self.cached_input_tokens, self.cached_input_missing),
            cache_write_input_tokens: known_total(
                self.cache_write_input_tokens,
                self.cache_write_input_missing,
            ),
            output_tokens: known_total(self.output_tokens, self.output_missing),
            reasoning_output_tokens: known_total(
                self.reasoning_output_tokens,
                self.reasoning_output_missing,
            ),
            total_tokens: known_total(self.total_tokens, self.total_missing),
        };
        let complete = !self.input_missing
            && !self.cached_input_missing
            && !self.cache_write_input_missing
            && !self.output_missing
            && !self.reasoning_output_missing
            && !self.total_missing;
        (
            usage,
            if complete {
                UsageStatus::Complete
            } else {
                UsageStatus::Partial
            },
        )
    }
}

fn add_dimension(value: Option<u32>, total: &mut u64, missing: &mut bool) {
    match value {
        Some(value) => *total = total.saturating_add(value as u64),
        None => *missing = true,
    }
}

fn known_total(total: u64, missing: bool) -> Option<u32> {
    (!missing).then_some(total.min(u32::MAX as u64) as u32)
}

fn failure_with_usage(error: impl std::fmt::Display, usage: &UsageAccumulator) -> anyhow::Error {
    let (usage, status) = usage.finish();
    let status = match status {
        UsageStatus::Unknown => UsageStatus::Unknown,
        _ => UsageStatus::Partial,
    };
    let diagnostic = serde_json::json!({
        "status": status,
        "input_tokens": usage.input_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "cache_write_input_tokens": usage.cache_write_input_tokens,
        "output_tokens": usage.output_tokens,
        "reasoning_output_tokens": usage.reasoning_output_tokens,
        "total_tokens": usage.total_tokens,
    });
    anyhow::anyhow!("{error}; scout usage diagnostic: {}", diagnostic)
}

pub struct ScoutEngine {
    model: Arc<dyn ModelBackend>,
    tools: RepoTools,
    budget: ExplorerBudget,
}

impl ScoutEngine {
    pub fn new(model: Arc<dyn ModelBackend>, tools: RepoTools, budget: ExplorerBudget) -> Self {
        let tools = tools
            .with_concurrency(budget.concurrency)
            .with_timeout(budget.tool_timeout());
        Self {
            model,
            tools,
            budget,
        }
    }

    pub fn tools(&self) -> &RepoTools {
        &self.tools
    }

    pub async fn scout(&self, request: ScoutRequest) -> anyhow::Result<ScoutResult> {
        crate::validate_request(&request)?;
        anyhow::ensure!(
            request.investigation.reasoning_effort.is_none(),
            "per-investigation reasoning_effort requires a native Codex or Claude Code backend"
        );
        let started = Instant::now();
        let max_turns = request
            .investigation
            .intent
            .turn_limit(request.max_turns.unwrap_or(self.budget.max_turns));
        let total_timeout = request.timeout.or_else(|| self.budget.total_timeout());
        let observed_usage = Arc::new(Mutex::new(UsageAccumulator::default()));
        let run = self.scout_inner(request, max_turns, Arc::clone(&observed_usage));
        let mut result = if let Some(total_timeout) = total_timeout {
            match tokio::time::timeout(total_timeout, run).await {
                Ok(result) => result?,
                Err(_) => {
                    let usage = observed_usage.lock().unwrap_or_else(|e| e.into_inner());
                    let (usage_stats, mut usage_status) = usage.finish();
                    if usage_status != UsageStatus::Unknown {
                        usage_status = UsageStatus::Partial;
                    }
                    let mut stats = ScoutStats {
                        warm_process: false,
                        thread_turn: 0,
                        turns: 0,
                        tool_calls: 0,
                        duration_ms: started.elapsed().as_millis() as u64,
                        model: self.model.name().to_string(),
                        ..Default::default()
                    };
                    usage_stats.apply_to(&mut stats);
                    stats.usage_status = usage_status;
                    return Ok(ScoutResult {
                        investigation: crate::InvestigationReport {
                            status: crate::InvestigationStatus::Failed,
                            ..Default::default()
                        },
                        summary: format!("Scout timed out after {}s.", total_timeout.as_secs()),
                        citations: vec![],
                        stats,
                        raw_final: None,
                    });
                }
            }
        } else {
            run.await?
        };
        result.stats.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    async fn scout_inner(
        &self,
        request: ScoutRequest,
        max_turns: u32,
        observed_usage: Arc<Mutex<UsageAccumulator>>,
    ) -> anyhow::Result<ScoutResult> {
        let tools = self.tools.for_root(request.root.clone());
        let system = build_system_prompt(&request.root);
        let mut messages = vec![
            ChatMessage::system(system),
            ChatMessage::user(crate::investigation_prompt(&request)),
        ];

        let tool_specs: Vec<ToolSpec> = tools
            .definitions()
            .into_iter()
            .map(|d| ToolSpec {
                name: d.name,
                description: d.description,
                parameters: d.parameters,
            })
            .collect();

        let mut turns: u32 = 0;
        let mut tool_calls_total: u32 = 0;
        let mut usage = UsageAccumulator::default();
        let mut correction_used = false;

        loop {
            turns += 1;
            if max_turns > 0 && turns > max_turns.saturating_add(1) {
                return Ok(empty_result(
                    &format!("No final answer after {max_turns} turns."),
                    turns - 1,
                    tool_calls_total,
                    self.model.name(),
                    &usage,
                ));
            }

            let final_turn = (max_turns > 0 && turns == max_turns.saturating_add(1))
                || tool_calls_total >= self.budget.max_tool_calls;
            if final_turn {
                messages.push(ChatMessage::user(
                    "Configured investigation budget reached. Return the final answer now from gathered evidence, with no more tool calls.",
                ));
            }

            let response = self
                .model
                .complete(ModelRequest {
                    messages: messages.clone(),
                    tools: if final_turn {
                        Vec::new()
                    } else {
                        tool_specs.clone()
                    },
                    temperature: self.model.temperature(),
                    max_tokens: None,
                })
                .await
                .map_err(|error| failure_with_usage(error, &usage))?;

            if let Some(u) = &response.usage {
                usage.add(u);
                *observed_usage.lock().unwrap_or_else(|e| e.into_inner()) = usage.clone();
            }

            let msg = response.message;
            messages.push(msg.clone());

            if let Some(calls) = &msg.tool_calls {
                if !calls.is_empty() {
                    if final_turn {
                        return Ok(empty_result(
                            "The model requested more tools after the configured budget was exhausted.",
                            turns,
                            tool_calls_total,
                            self.model.name(),
                            &usage,
                        ));
                    }
                    if tool_calls_total as usize + calls.len() > self.budget.max_tool_calls as usize
                    {
                        warn!("max tool calls reached");
                        for call in calls {
                            messages.push(ChatMessage::tool(
                                call.id.clone(),
                                "Tool budget exhausted; this call was not executed.",
                            ));
                        }
                        tool_calls_total = self.budget.max_tool_calls;
                        messages.push(ChatMessage::user(
                            "Tool call budget exhausted. Return the investigation JSON now, marking unresolved questions partial.",
                        ));
                        continue;
                    }

                    let tool_calls: Vec<ToolCall> = calls
                        .iter()
                        .map(|c| ToolCall {
                            id: c.id.clone(),
                            name: c.name.clone(),
                            arguments: sandbox_search_arguments(
                                &c.name,
                                &c.arguments,
                                tools.root(),
                            ),
                        })
                        .collect();
                    for call in &tool_calls {
                        debug!(name = %call.name, arguments = %call.arguments, "model tool call");
                    }

                    debug!(count = tool_calls.len(), "executing tools concurrently");
                    let results = tools.call_many(&tool_calls).await;
                    tool_calls_total += results.len() as u32;
                    for result in results {
                        messages.push(ChatMessage::tool(result.tool_call_id, result.output));
                    }
                    continue;
                }
            }

            // Final assistant message (no tool calls).
            let content = msg.content.clone().unwrap_or_default();
            let (summary, validated, investigation) = assess_output(&request, &content);

            // One correction turn if claimed citations are invalid or malformed.
            if validated.is_empty() && !correction_used && !content.trim().is_empty() {
                correction_used = true;
                messages.push(ChatMessage::user(
                    "The cited locations were invalid. Return investigation JSON with direct source evidence. Mark unresolved questions partial instead of inventing citations.",
                ));
                continue;
            }

            let (usage_stats, usage_status) = usage.finish();
            let mut stats = ScoutStats {
                warm_process: false,
                thread_turn: 0,
                turns,
                tool_calls: tool_calls_total,
                duration_ms: 0,
                model: self.model.name().to_string(),
                ..Default::default()
            };
            usage_stats.apply_to(&mut stats);
            stats.usage_status = usage_status;

            return Ok(ScoutResult {
                investigation,
                summary,
                citations: validated,
                stats,
                raw_final: Some(content),
            });
        }
    }
}

#[async_trait::async_trait]
impl crate::types::ScoutBackend for ScoutEngine {
    async fn scout(&self, request: ScoutRequest) -> anyhow::Result<ScoutResult> {
        ScoutEngine::scout(self, request).await
    }
}

/// Whether a path the *model* produced looks absolute.
///
/// `Path::is_absolute` answers for the host, not the model, and gets this wrong
/// in both directions: Windows rejects `/guessed/auth` for lacking a drive
/// letter, and Unix rejects `C:\guessed`. Either way an invented path would be
/// passed straight through to the tool instead of broadened to the repository
/// root, so decide from the shape of the string rather than the host.
fn model_path_is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive_prefixed = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    path.starts_with('/') || path.starts_with('\\') || drive_prefixed
}

fn sandbox_search_arguments(name: &str, arguments: &str, root: &Path) -> String {
    let field = match name {
        "Read" | "Grep" => "path",
        "Glob" => "directory",
        _ => return arguments.to_string(),
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_string();
    };
    let Some(path) = value.get(field).and_then(|path| path.as_str()) else {
        return arguments.to_string();
    };

    if resolve_in_root(root, path).is_ok_and(|resolved| resolved.exists()) {
        return arguments.to_string();
    }

    let trimmed = path.trim_start_matches(['/', '\\']);
    let root_name = root.file_name().and_then(|name| name.to_str());
    let named_relative = root_name
        .and_then(|root_name| trimmed.strip_prefix(root_name))
        .and_then(|path| path.strip_prefix(['/', '\\']));
    let parts: Vec<_> = trimmed.split(['/', '\\']).collect();
    let existing_relative = (1..parts.len())
        .map(|index| parts[index..].join("/"))
        .find(|path| root.join(path).exists());
    let relative = named_relative
        .map(str::to_owned)
        .or(existing_relative)
        .filter(|path| resolve_in_root(root, path).is_ok());

    if let Some(relative) = relative {
        value[field] = relative.into();
        debug!(field, "normalized model path inside repository root");
    } else if model_path_is_absolute(path) && resolve_in_root(root, path).is_err() {
        if name == "Read" {
            value[field] = trimmed.into();
            debug!(
                field,
                "normalized escaped model read inside repository root"
            );
        } else {
            value.as_object_mut().unwrap().remove(field);
            debug!(field, "broadened escaped model search to repository root");
        }
    } else {
        return arguments.to_string();
    }
    value.to_string()
}

fn empty_result(
    summary: &str,
    turns: u32,
    tool_calls: u32,
    model: &str,
    usage: &UsageAccumulator,
) -> ScoutResult {
    let (usage_stats, usage_status) = usage.finish();
    let mut stats = ScoutStats {
        warm_process: false,
        thread_turn: 0,
        turns,
        tool_calls,
        duration_ms: 0,
        model: model.into(),
        ..Default::default()
    };
    usage_stats.apply_to(&mut stats);
    stats.usage_status = usage_status;
    ScoutResult {
        investigation: crate::InvestigationReport {
            status: crate::InvestigationStatus::Partial,
            ..Default::default()
        },
        summary: summary.into(),
        citations: vec![],
        stats,
        raw_final: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use repotracer_model::{
        ChatMessage, MockModel, MockScript, MockStep, ModelError, ModelResponse,
    };
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    struct UsageModel {
        responses: Mutex<VecDeque<Result<ModelResponse, ModelError>>>,
    }

    #[async_trait::async_trait]
    impl ModelBackend for UsageModel {
        fn name(&self) -> &str {
            "usage-mock"
        }

        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, ModelError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ModelError::ScriptExhausted))
        }
    }

    fn reported_usage(input: u32) -> Usage {
        Usage {
            prompt_tokens: Some(input),
            cached_prompt_tokens: Some(input / 2),
            cache_write_prompt_tokens: Some(input / 10),
            completion_tokens: Some(input / 4),
            reasoning_output_tokens: Some(input / 8),
            total_tokens: Some(input + input / 4),
        }
    }

    #[tokio::test]
    async fn json_reports_with_invalid_citations_get_one_correction() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn answer() {}\n").unwrap();
        let report = |path: &str| {
            serde_json::json!({
            "answer": "The answer function is defined in lib.rs.",
            "status": "complete",
            "findings": [{"question": "find answer", "answer": "Function definition",
                "citations": [{"path": path, "start_line": 1, "end_line": 1, "reason": "definition"}]}],
            "unresolved": [], "searched_scope": ["lib.rs"], "limitations": []
        }).to_string()
        };
        for corrected_path in ["lib.rs", "still-missing.rs"] {
            let model = Arc::new(MockModel::new(MockScript {
                steps: vec![
                    MockStep::Final(report("missing.rs")),
                    MockStep::Final(report(corrected_path)),
                ],
            }));
            let engine =
                ScoutEngine::new(model, RepoTools::new(dir.path()), ExplorerBudget::default());
            let result = engine
                .scout(ScoutRequest {
                    investigation: Default::default(),
                    query: "find answer".into(),
                    root: dir.path().to_owned(),
                    focus: None,
                    max_turns: Some(4),
                    timeout: None,
                })
                .await
                .unwrap();
            assert_eq!(result.stats.turns, 2);
            assert_eq!(
                result.citations.len(),
                usize::from(corrected_path == "lib.rs")
            );
        }
    }

    #[tokio::test]
    async fn mock_end_to_end() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/auth")).unwrap();
        fs::write(
            dir.path().join("src/auth/session.rs"),
            (1..=100)
                .map(|i| format!("// line {i} session\n"))
                .collect::<String>(),
        )
        .unwrap();

        let model = Arc::new(MockModel::new(MockScript {
            steps: vec![
                MockStep::Tools(vec![
                    (
                        "Glob".into(),
                        r#"{"pattern":"**/*auth*"}"#.into(),
                    ),
                    (
                        "Grep".into(),
                        r#"{"pattern":"session","output_mode":"files_with_matches"}"#.into(),
                    ),
                ]),
                MockStep::Final(
                    "Session handling lives here.\n\n<final_answer>\nsrc/auth/session.rs:10-40 (session module)\n</final_answer>"
                        .into(),
                ),
            ],
        }));

        let tools = RepoTools::new(dir.path());
        let engine = ScoutEngine::new(model, tools, ExplorerBudget::default());
        let result = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "where is session handled?".into(),
                root: dir.path().to_path_buf(),
                focus: None,
                max_turns: Some(4),
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(result.citations.len(), 1);
        assert_eq!(result.citations[0].path, "src/auth/session.rs");
        assert!(result.stats.tool_calls >= 2);
        assert!(result.stats.turns >= 2);
    }

    #[tokio::test]
    async fn usage_aggregates_each_generation_once() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn answer() {}\n").unwrap();
        let model = Arc::new(UsageModel {
            responses: Mutex::new(VecDeque::from([
                Ok(ModelResponse {
                    message: ChatMessage::assistant_tools(
                        None,
                        vec![repotracer_model::FunctionCall {
                            id: "read".into(),
                            name: "Read".into(),
                            arguments: r#"{"path":"lib.rs"}"#.into(),
                        }],
                    ),
                    model: "usage-mock".into(),
                    usage: Some(reported_usage(100)),
                }),
                Ok(ModelResponse {
                    message: ChatMessage::assistant(
                        "<final_answer>\nlib.rs:1-1 (answer)\n</final_answer>",
                    ),
                    model: "usage-mock".into(),
                    usage: Some(reported_usage(40)),
                }),
            ])),
        });
        let engine = ScoutEngine::new(model, RepoTools::new(dir.path()), ExplorerBudget::default());
        let result = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "find answer".into(),
                root: dir.path().to_path_buf(),
                focus: None,
                max_turns: Some(2),
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(result.stats.prompt_tokens, Some(140));
        assert_eq!(result.stats.cached_prompt_tokens, Some(70));
        assert_eq!(result.stats.cache_write_prompt_tokens, Some(14));
        assert_eq!(result.stats.completion_tokens, Some(35));
        assert_eq!(result.stats.reasoning_output_tokens, Some(17));
        assert_eq!(result.stats.total_tokens, Some(175));
        assert_eq!(result.stats.usage_status, UsageStatus::Complete);
    }

    #[tokio::test]
    async fn failed_generation_surfaces_partial_usage_without_prompt_data() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn answer() {}\n").unwrap();
        let model = Arc::new(UsageModel {
            responses: Mutex::new(VecDeque::from([
                Ok(ModelResponse {
                    message: ChatMessage::assistant_tools(
                        None,
                        vec![repotracer_model::FunctionCall {
                            id: "read".into(),
                            name: "Read".into(),
                            arguments: r#"{"path":"lib.rs"}"#.into(),
                        }],
                    ),
                    model: "usage-mock".into(),
                    usage: Some(reported_usage(100)),
                }),
                Err(ModelError::Request("provider stopped".into())),
            ])),
        });
        let engine = ScoutEngine::new(model, RepoTools::new(dir.path()), ExplorerBudget::default());
        let error = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "find answer".into(),
                root: dir.path().to_path_buf(),
                focus: None,
                max_turns: Some(3),
                timeout: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("scout usage diagnostic"));
        assert!(error.contains("partial"));
        assert!(error.contains("100"));
        assert!(!error.contains("find answer"));
    }

    #[tokio::test]
    async fn turn_limit_forces_an_answer_without_tools() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn answer() {}\n").unwrap();
        let model = Arc::new(MockModel::new(MockScript {
            steps: vec![
                MockStep::Tools(vec![("Read".into(), r#"{"path":"lib.rs"}"#.into())]),
                MockStep::FinalWithoutTools(
                    "<final_answer>\nlib.rs:1-1 (answer)\n</final_answer>".into(),
                ),
            ],
        }));
        let engine = ScoutEngine::new(model, RepoTools::new(dir.path()), ExplorerBudget::default());

        let result = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "find answer".into(),
                root: dir.path().to_path_buf(),
                focus: None,
                max_turns: Some(1),
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(result.citations.len(), 1);
        assert_eq!(result.stats.turns, 2);
    }

    #[tokio::test]
    async fn uncapped_turns_allow_investigation_beyond_six_reads() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn setup() {}\n").unwrap();
        let mut steps =
            vec![MockStep::Tools(vec![("Read".into(), r#"{"path":"a.rs"}"#.into())]); 8];
        steps.push(MockStep::Final(
            "<final_answer>\na.rs:1-1\n</final_answer>".into(),
        ));
        let engine = ScoutEngine::new(
            Arc::new(MockModel::new(MockScript { steps })),
            RepoTools::new(dir.path()),
            ExplorerBudget::default(),
        );
        let result = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "inspect".into(),
                root: dir.path().into(),
                focus: None,
                max_turns: None,
                timeout: None,
            })
            .await
            .unwrap();
        assert_eq!(result.stats.turns, 9);
        assert_eq!(result.stats.tool_calls, 8);
        assert_eq!(result.citations.len(), 1);
    }

    #[tokio::test]
    async fn uncapped_turns_still_stop_when_model_ignores_tool_budget() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn setup() {}\n").unwrap();
        let steps = vec![MockStep::Tools(vec![("Read".into(), r#"{"path":"a.rs"}"#.into())]); 2];
        let engine = ScoutEngine::new(
            Arc::new(MockModel::new(MockScript { steps })),
            RepoTools::new(dir.path()),
            ExplorerBudget {
                max_tool_calls: 1,
                ..Default::default()
            },
        );
        let result = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "inspect".into(),
                root: dir.path().into(),
                focus: None,
                max_turns: Some(0),
                timeout: None,
            })
            .await
            .unwrap();
        assert_eq!(result.stats.turns, 2);
        assert_eq!(result.stats.tool_calls, 1);
        assert_eq!(
            result.investigation.status,
            crate::InvestigationStatus::Partial
        );
        assert!(result.summary.contains("budget was exhausted"));
    }

    #[tokio::test]
    async fn repeated_reads_are_allowed_and_count_against_tool_budget() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn setup() {}\n").unwrap();
        let repeated = ("Grep".into(), r#"{"pattern":"setup"}"#.into());
        let model = Arc::new(MockModel::new(MockScript {
            steps: vec![
                MockStep::Tools(vec![repeated.clone()]),
                MockStep::Tools(vec![repeated]),
                MockStep::Final("<final_answer>\na.rs:1-1\n</final_answer>".into()),
            ],
        }));
        let engine = ScoutEngine::new(model, RepoTools::new(dir.path()), ExplorerBudget::default());

        let result = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "find setup".into(),
                root: dir.path().into(),
                focus: None,
                max_turns: Some(4),
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(result.stats.tool_calls, 2);
        assert_eq!(result.citations.len(), 1);
    }

    #[tokio::test]
    async fn malformed_final_answer_gets_one_correction_turn() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn setup() {}\n").unwrap();
        let model = Arc::new(MockModel::new(MockScript {
            steps: vec![
                MockStep::Final("<final_answer>setup is in a.rs</final_answer>".into()),
                MockStep::Final("<final_answer>\na.rs:1-1 (setup)\n</final_answer>".into()),
            ],
        }));
        let engine = ScoutEngine::new(model, RepoTools::new(dir.path()), ExplorerBudget::default());

        let result = engine
            .scout(ScoutRequest {
                investigation: Default::default(),
                query: "find setup".into(),
                root: dir.path().into(),
                focus: None,
                max_turns: Some(4),
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(result.stats.turns, 2);
        assert_eq!(result.citations.len(), 1);
    }

    #[test]
    fn model_paths_are_absolute_regardless_of_host_platform() {
        // Windows' own is_absolute rejects these, but the model emits them on
        // every platform, so they must still be treated as escapes.
        assert!(model_path_is_absolute("/guessed/auth"));
        assert!(model_path_is_absolute("\\guessed\\auth"));
        assert!(model_path_is_absolute("C:\\guessed"));
        assert!(!model_path_is_absolute("src/main.rs"));
        assert!(!model_path_is_absolute("./src"));
    }

    #[test]
    fn escaped_model_searches_fall_back_to_repository_root() {
        let dir = tempdir().unwrap();
        let arguments = sandbox_search_arguments(
            "Grep",
            r#"{"pattern":"token","path":"/guessed/auth"}"#,
            dir.path(),
        );
        let value: serde_json::Value = serde_json::from_str(&arguments).unwrap();
        assert_eq!(value["pattern"], "token");
        assert!(value.get("path").is_none());
    }

    #[test]
    fn escaped_model_reads_are_normalized_inside_repository_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("Gigo");
        let arguments = sandbox_search_arguments(
            "Read",
            r#"{"path":"/Gigo/internal/server/server.go"}"#,
            &root,
        );
        let value: serde_json::Value = serde_json::from_str(&arguments).unwrap();
        assert_eq!(value["path"], "internal/server/server.go");
    }

    #[test]
    fn model_paths_prefixed_with_repository_name_are_normalized() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("checkout");
        fs::create_dir_all(root.join("crates/core")).unwrap();

        for (tool, field) in [("Read", "path"), ("Grep", "path"), ("Glob", "directory")] {
            let arguments = format!(r#"{{"{field}":"repotracer/crates/core","pattern":"engine"}}"#);
            let normalized = sandbox_search_arguments(tool, &arguments, &root);
            let value: serde_json::Value = serde_json::from_str(&normalized).unwrap();
            assert_eq!(value[field], "crates/core");
        }
    }
}
