//! Minimal MCP JSON-RPC server over stdio for repotracer.
//! NEVER write non-protocol text to stdout.

mod conversations;
mod transport;

use repotracer_core::{
    validate_request, ScoutBackend, ScoutBackendError, ScoutRequest, ScoutResult, ValidatedCitation,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "repotracer";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Ceiling for either supported rendering of the answer. Native Codex keeps
/// structuredContent alone; text-only clients keep content. Each must be
/// self-contained. Compatibility copies on the wire do not halve source space.
const MAX_HANDOFF_BYTES: usize = 36 * 1024;
const SUCCESSFUL_HANDOFF: &str = "Investigation complete. Continue the task using the findings and repository source below. Any reported gaps or source changes can be checked locally or in a follow-up.";
const EMPTY_HANDOFF: &str =
    "No validated evidence was returned. Fall back to normal repository exploration.";
const REPO_SCOUT_DESC: &str = "Delegate repository investigation to a cheaper read-only colleague. Ask the question you need answered; no search terms are required. Set repository to the task's checkout when it differs from the server's startup directory. Returns the actual repository, a conversation handle, findings, and line-numbered repository source. Reuse the returned conversation.id as investigation.conversation_id for related questions; status says resumed, fresh, or unknown. Independent calls can run in parallel. structuredContent contains the report and source; content[].text is a readable fallback. Use either representation, not both. Source blocks come from files; conclusions are scout judgments.";

#[derive(Clone)]
pub struct McpServer {
    scout: Arc<dyn ScoutBackend>,
    root: PathBuf,
    conversations: Arc<conversations::Conversations>,
}

impl McpServer {
    pub fn new(scout: Arc<dyn ScoutBackend>, root: PathBuf) -> Self {
        Self {
            scout,
            root,
            conversations: Arc::new(conversations::Conversations::default()),
        }
    }

    /// Serve MCP over stdin/stdout until EOF.
    pub async fn serve_stdio(&self) -> anyhow::Result<()> {
        let server = self.clone();
        transport::serve(tokio::io::stdin(), tokio::io::stdout(), move |message| {
            let server = server.clone();
            async move { server.handle_message(message).await }
        })
        .await
    }

    async fn handle_message(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        // Notifications have no id — no response.
        let is_notification = id.is_none() || id.as_ref().is_some_and(|v| v.is_null());

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": {},
                    "prompts": {}
                },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": SERVER_VERSION
                }
            })),
            "notifications/initialized" | "initialized" => {
                return None;
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                let mut tool = repo_scout_tool_def();
                let root = self
                    .root
                    .canonicalize()
                    .unwrap_or_else(|_| self.root.clone());
                tool["description"] = json!(format!(
                    "{REPO_SCOUT_DESC} Server startup repository: {}.",
                    json!(root)
                ));
                Ok(json!({"tools": [tool]}))
            }
            "tools/call" => self.tools_call(params).await,
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(json!({ "prompts": [repo_scout_prompt_def()] })),
            "prompts/get" => repo_scout_prompt(params),
            "" if msg.get("result").is_some() || msg.get("error").is_some() => {
                return None;
            }
            other => Err(rpc_error(-32601, format!("Method not found: {other}"))),
        };

        if is_notification {
            return None;
        }

        Some(match result {
            Ok(r) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": r
            }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": e
            }),
        })
    }

    async fn tools_call(&self, params: Value) -> Result<Value, Value> {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name != "repo_scout" {
            return Err(rpc_error(-32602, format!("Unknown tool: {name}")));
        }

        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return Ok(tool_text("Error: `query` is required.", true));
        }

        let focus = args
            .get("focus")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let explicit_root = match args.get("repository") {
            Some(Value::String(path)) => Some(path.as_str()),
            None => None,
            _ => {
                return Err(rpc_error(
                    -32602,
                    "repository must be a directory path string".into(),
                ))
            }
        };
        let mut investigation: repotracer_core::InvestigationSpec =
            serde_json::from_value(args.get("investigation").cloned().unwrap_or(json!({})))
                .map_err(|e| rpc_error(-32602, format!(
                    "Invalid investigation: {e}. Supply a JSON object, for example \"investigation\": {{\"intent\": \"diagnose\", \"reasoning_effort\": \"high\"}}, or omit investigation and use query alone."
                )))?;
        let id = investigation
            .conversation_id
            .clone()
            .unwrap_or_else(|| format!("rt-{}", uuid::Uuid::new_v4()));
        // Reserve FIFO order before repository selection can yield. Related
        // requests also read remembered roots only after the prior turn binds them.
        let _conversation_turn = match self.conversations.enter(&id).await {
            Ok(guard) => guard,
            Err(error) => return Ok(tool_text(&format!("Error: {error}"), true)),
        };
        let remembered = self.conversations.root(&id);
        if investigation.conversation_id.is_some()
            && id.starts_with("rt-")
            && remembered.is_none()
            && explicit_root.is_none()
        {
            return Ok(tool_text("Error: conversation handle is no longer known. Supply repository and necessary context to start fresh, or omit conversation_id for a new investigation.", true));
        }
        let default = self.root.clone();
        let explicit_root = explicit_root.map(str::to_owned);
        let repository_focus = focus.clone();
        let selection = tokio::task::spawn_blocking(move || {
            conversations::select_repository(
                &default,
                explicit_root.as_deref(),
                repository_focus.as_deref(),
                remembered.as_deref(),
            )
        })
        .await
        .map_err(|error| rpc_error(-32603, format!("Repository selection failed: {error}")))?;
        let root = match selection {
            Ok(root) => root,
            Err(error) => return Ok(tool_text(&format!("Error: {error}"), true)),
        };
        investigation.conversation_id = Some(id.clone());

        let mut request = ScoutRequest {
            investigation,
            query,
            root: root.clone(),
            focus,
            max_turns: None,
            timeout: None,
        };
        if let Err(error) = request.normalize_paths() {
            return Ok(tool_text(&format!("Error: {error}"), true));
        }
        if let Err(error) = validate_request(&request) {
            return Ok(tool_text(&format!("Error: {error}"), true));
        }

        if let Err(error) = self.conversations.bind(&id, &root) {
            return Ok(tool_text(&format!("Error: {error}"), true));
        }

        let mut result = match self.scout.scout(request).await {
            Ok(result) => result,
            Err(error) => {
                if let Some(error) = error.downcast_ref::<ScoutBackendError>() {
                    let mut error = error.clone();
                    set_conversation(&mut error.stats, &id, &root);
                    return Ok(terminal_failure_response(&error));
                }
                let mut error_response = rpc_error(-32000, error.to_string());
                error_response["data"] =
                    json!({"conversation": {"id": id, "repository": root, "status": "unknown"}});
                return Err(error_response);
            }
        };

        set_conversation(&mut result.stats, &id, &root);
        Ok(handoff_response(&root, result))
    }
}

fn set_conversation(stats: &mut repotracer_core::ScoutStats, id: &str, root: &Path) {
    // An adaptive continuation can make the aggregate turn count > 1 even
    // though the parent's initial request started a fresh thread.
    let turn = stats
        .attempts
        .first()
        .map_or(stats.thread_turn, |attempt| attempt.thread_turn);
    stats.conversation = Some(repotracer_core::ConversationInfo {
        id: id.to_owned(),
        repository: root.display().to_string(),
        status: match turn {
            0 => "unknown",
            1 => "fresh",
            _ => "resumed",
        }
        .into(),
    });
}

fn terminal_failure_response(error: &ScoutBackendError) -> Value {
    let context = error
        .stats
        .conversation
        .as_ref()
        .map(|c| {
            format!(
                "\nRepository: {}\nConversation: {} ({})",
                c.repository, c.id, c.status
            )
        })
        .unwrap_or_default();
    json!({
        "content": [{ "type": "text", "text": format!("{error}{context}") }],
        "structuredContent": { "stats": error.stats, "conversation": error.stats.conversation },
        "isError": true
    })
}

#[derive(Debug, Clone)]
struct EvidenceSpan {
    path: String,
    start_line: u32,
    end_line: u32,
    text: String,
    citation_count: usize,
    // First citation's position in the scout's task-importance order.
    priority: usize,
    truncated: bool,
}

#[derive(Debug, Default)]
struct EvidenceBundle {
    spans: Vec<EvidenceSpan>,
    omitted_citations: usize,
    omitted_spans: usize,
}

#[derive(Debug, Default)]
struct HandoffOmissions {
    omitted_source_citations: usize,
    omitted_source_spans: usize,
    report_omitted_citations: usize,
    truncated_spans: usize,
    report_trimmed: bool,
}

fn handoff_response(root: &Path, mut result: ScoutResult) -> Value {
    result.citations = unique_citations(result.citations);
    let bundle = evidence_excerpts(root, &result.citations);
    let mut spans = bundle.spans;
    let mut omissions = HandoffOmissions {
        omitted_source_citations: bundle.omitted_citations,
        omitted_source_spans: bundle.omitted_spans,
        ..Default::default()
    };
    let mut handoff_limitations = Vec::new();
    if omissions.omitted_source_spans > 0 {
        handoff_limitations.push(format!(
            "{} cited source span{} could not be embedded safely or did not contain the requested lines; this is a handoff limitation, not a determination that a question is unresolved.",
            omissions.omitted_source_spans,
            if omissions.omitted_source_spans == 1 { "" } else { "s" }
        ));
    }

    // Keep complete source spans whenever the complete result fits. If the
    // narrative alone is too large, shorten the report; otherwise remove
    // the lowest-priority source span only as needed for the one ceiling.
    // The scout orders findings/citations by usefulness to the next step.
    let mut report_target = MAX_HANDOFF_BYTES / 2;
    let mut emergency_compaction = false;
    loop {
        let response = build_handoff_response(&result, &spans, &omissions, &handoff_limitations);
        if response_size(&response) <= MAX_HANDOFF_BYTES {
            return response;
        }

        // Keep the narrative intact whenever it fits by itself. Source spans
        // are lower-priority only when the full result needs more room.
        let explanation_only =
            build_handoff_response(&result, &[], &omissions, &handoff_limitations);
        if response_size(&explanation_only) > MAX_HANDOFF_BYTES {
            let report_changed = bound_report_and_track(&mut result, report_target, &mut omissions);
            if report_changed {
                if !omissions.report_trimmed {
                    omissions.report_trimmed = true;
                    handoff_limitations.push(
                        "The model-authored report exceeded the single MCP handoff budget and was shortened; request a narrower investigation for omitted explanation.".into(),
                    );
                    result.investigation.status = repotracer_core::InvestigationStatus::Partial;
                }
                report_target = report_target.saturating_mul(3) / 4;
                continue;
            }
        }

        if spans.len() > 1 {
            // Do not evict every other span for one that cannot fit even on
            // its own. Otherwise preserve task importance, not shortest text.
            let individually_oversized = spans
                .iter()
                .enumerate()
                .filter(|(_, span)| {
                    response_size(&build_handoff_response(
                        &result,
                        std::slice::from_ref(*span),
                        &omissions,
                        &handoff_limitations,
                    )) > MAX_HANDOFF_BYTES
                })
                .max_by_key(|(_, span)| span.text.len())
                .map(|(index, _)| index);
            let discard = individually_oversized.unwrap_or_else(|| {
                spans
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, span)| span.priority)
                    .map(|(index, _)| index)
                    .expect("length checked above")
            });
            let span = spans.remove(discard);
            omissions.omitted_source_citations += span.citation_count;
            omissions.omitted_source_spans += 1;
            mark_source_omitted(&mut handoff_limitations);
            continue;
        }

        if spans.len() == 1 && !spans[0].truncated {
            let prior_limitations = handoff_limitations.len();
            mark_source_truncated(&mut handoff_limitations);
            // Include the metadata added by a successful truncation while
            // testing the candidate, then roll it back if the span is dropped.
            omissions.truncated_spans += 1;
            if truncate_last_span_to_fit(&mut spans, &result, &omissions, &handoff_limitations) {
                continue;
            }
            omissions.truncated_spans -= 1;
            handoff_limitations.truncate(prior_limitations);
            let span = spans.pop().expect("length checked above");
            omissions.omitted_source_citations += span.citation_count;
            omissions.omitted_source_spans += 1;
            mark_source_omitted(&mut handoff_limitations);
            continue;
        }

        if spans.len() == 1 && spans[0].truncated {
            let span = spans.pop().expect("length checked above");
            omissions.omitted_source_citations += span.citation_count;
            omissions.omitted_source_spans += 1;
            omissions.truncated_spans = omissions.truncated_spans.saturating_sub(1);
            mark_source_omitted(&mut handoff_limitations);
            continue;
        }

        // At this point no source span can be retained. Only now shorten an
        // oversized narrative/report, keeping its explicit limitation.
        let report_changed = bound_report_and_track(&mut result, report_target, &mut omissions);
        if report_changed {
            if !omissions.report_trimmed {
                omissions.report_trimmed = true;
                handoff_limitations.push(
                    "The model-authored report exceeded the single MCP handoff budget and was shortened; request a narrower investigation for omitted explanation.".into(),
                );
                result.investigation.status = repotracer_core::InvestigationStatus::Partial;
            }
            report_target = report_target.saturating_mul(3) / 4;
            continue;
        }

        if report_target > 512 {
            report_target = report_target.saturating_mul(3) / 4;
            continue;
        }

        // This is only reachable for adversarially large citation metadata.
        // Keep protocol shape and stats, but explicitly report that model
        // detail and citation records had to be discarded for transport.
        if !emergency_compaction {
            emergency_compaction = true;
            omissions.report_trimmed = true;
            omissions.report_omitted_citations += result.citations.len();
            result.summary.clear();
            result.investigation.findings.clear();
            result.investigation.searched_scope.clear();
            result.investigation.unresolved.clear();
            result.investigation.limitations.clear();
            result.investigation.confidence = Default::default();
            result.citations.clear();
            // Model identity is the only unbounded string in stats. Preserve
            // all usage counters if an oversized custom label reaches here.
            if result.stats.model.len() > 256 {
                result.stats.model = "[oversized model label omitted]".into();
            }
            result.investigation.status = repotracer_core::InvestigationStatus::Partial;
            handoff_limitations.push(
                "The handoff retained only its protocol metadata and stats after oversized report data exceeded the transport budget.".into(),
            );
            continue;
        }

        // The fixed protocol fields themselves are small, so this branch is
        // defensive. It prevents an accidental infinite loop if their schema
        // ever grows beyond the declared transport ceiling.
        return response;
    }
}

fn unique_citations(citations: Vec<ValidatedCitation>) -> Vec<ValidatedCitation> {
    let mut unique = Vec::with_capacity(citations.len());
    for citation in citations {
        if !unique.iter().any(|kept: &ValidatedCitation| {
            kept.path == citation.path
                && kept.start_line == citation.start_line
                && kept.end_line == citation.end_line
        }) {
            unique.push(citation);
        }
    }
    unique
}

fn response_size(response: &Value) -> usize {
    ["content", "structuredContent"]
        .iter()
        .map(|field| serde_json::to_vec(&response[field]).map_or(usize::MAX, |bytes| bytes.len()))
        .max()
        .unwrap_or(0)
}

fn push_limitation(limitations: &mut Vec<String>, message: String) {
    if !limitations.iter().any(|existing| existing == &message) {
        limitations.push(message);
    }
}

fn mark_source_omitted(limitations: &mut Vec<String>) {
    push_limitation(
        limitations,
        "Some source context was omitted to keep the complete MCP result within its transport budget; the investigation findings and citation locations remain separate from this output limitation.".into(),
    );
}

fn mark_source_truncated(limitations: &mut Vec<String>) {
    push_limitation(
        limitations,
        "A source span was truncated to keep the complete MCP result within its transport budget; the missing source text does not by itself make a question unresolved.".into(),
    );
}

fn build_handoff_response(
    result: &ScoutResult,
    spans: &[EvidenceSpan],
    omissions: &HandoffOmissions,
    handoff_limitations: &[String],
) -> Value {
    let explanation = handoff_explanation(result, handoff_limitations);
    let next_action = if result.citations.is_empty() {
        EMPTY_HANDOFF
    } else if result.investigation.status != repotracer_core::InvestigationStatus::Complete {
        "Partial investigation handoff. Use the supported explanation and embedded source context, then check the listed unresolved questions or handoff limitations. Missing source text is not proof of absence."
    } else if !handoff_limitations.is_empty() {
        "The scout reports the objective answered. Some source text could not be included; the explanation and source locations remain available. Consult the omission metadata if that context matters to your next step."
    } else {
        SUCCESSFUL_HANDOFF
    };
    // Keep the model's explanation and source context first; the routing
    // instruction is useful metadata, not the handoff's main payload.
    let mut text = explanation.clone();
    let evidence = append_evidence(&mut text, spans, omissions);
    let citations = source_delivery_citations(&result.citations, spans);
    let missing = citations
        .iter()
        .filter(|citation| citation["source_status"] != "included")
        .map(|citation| {
            format!(
                "{}:{}-{} ({})",
                citation["path"].as_str().unwrap_or(""),
                citation["start_line"],
                citation["end_line"],
                citation["source_status"].as_str().unwrap_or("omitted")
            )
        })
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        text.push_str("\n\nSource not fully included: ");
        text.push_str(&missing.join(", "));
    }
    text.push_str(&format!("\n\nNext action: {next_action}"));
    if let Some(conversation) = &result.stats.conversation {
        text.push_str(&format!("\nRepository: {}\nConversation: {} ({})\nUse this ID in investigation.conversation_id for a related follow-up. A fresh or unknown session needs the current question and necessary context.",
            conversation.repository, conversation.id, conversation.status));
    }
    let structured = json!({
        "handoff_version": 3,
        "repository": root_from_stats(&result.stats),
        "conversation": result.stats.conversation,
        "report": explanation,
        "investigation": {
            "intent": result.investigation.intent,
            "status": result.investigation.status,
            "confidence": result.investigation.confidence,
            "unresolved": result.investigation.unresolved,
        },
        // This legacy field counts citation records removed from the machine
        // citation list. Source-only omissions are reported separately below.
        "omitted_citations": omissions.report_omitted_citations,
        "citations": citations,
        "evidence": evidence,
        "evidence_omissions": {
            "omitted_citations": omissions.omitted_source_citations,
            "omitted_spans": omissions.omitted_source_spans,
            "truncated_spans": omissions.truncated_spans,
            "explicit": omissions.omitted_source_spans > 0 || omissions.truncated_spans > 0,
        },
        "handoff_limitations": handoff_limitations,
        "next_action": next_action,
        "stats": result.stats,
    });

    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false
    })
}

fn root_from_stats(stats: &repotracer_core::ScoutStats) -> Option<&str> {
    stats.conversation.as_ref().map(|c| c.repository.as_str())
}

/// Delivery describes the actual source text, not the truth of a finding.
/// Keep original citation fields for existing clients; status is additive.
fn source_delivery_citations(
    citations: &[ValidatedCitation],
    spans: &[EvidenceSpan],
) -> Vec<Value> {
    citations
        .iter()
        .map(|citation| {
            let status = spans
                .iter()
                .find(|span| {
                    span.path == citation.path
                        && span.start_line <= citation.start_line
                        && span.end_line >= citation.end_line
                })
                .map_or("omitted", |span| {
                    if span.truncated {
                        "truncated"
                    } else {
                        "included"
                    }
                });
            let mut value = serde_json::to_value(citation).expect("citation is serializable");
            value["source_status"] = json!(status);
            value
        })
        .collect()
}

fn handoff_explanation(result: &ScoutResult, handoff_limitations: &[String]) -> String {
    let mut out = format!("Investigation status: {:?}\n", result.investigation.status);
    let confidence = &result.investigation.confidence;
    out.push_str(&format!(
        "Scout-reported confidence: {:?}\nEvidence basis: {}\n",
        confidence.level,
        if confidence.basis.is_empty() {
            "Not reported."
        } else {
            &confidence.basis
        }
    ));
    if !result.summary.is_empty() {
        out.push_str("\nSummary:\n");
        out.push_str(&result.summary);
        out.push('\n');
    }
    if result.citations.is_empty() {
        out.push_str("\nNo validated citations.\n");
    }
    if !result.investigation.findings.is_empty() {
        out.push_str("\nFindings:\n");
        for finding in &result.investigation.findings {
            out.push_str(&format!(
                "\nQuestion: {}\nExplanation: {}\n",
                finding.question, finding.answer
            ));
            if !finding.citations.is_empty() {
                out.push_str("Sources: ");
                for (index, citation) in finding.citations.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&format!(
                        "{}:{}-{}",
                        citation.path, citation.start_line, citation.end_line
                    ));
                }
                out.push('\n');
            }
        }
    }
    if !result.investigation.searched_scope.is_empty() {
        out.push_str("\nSearched scope:\n");
        for scope in &result.investigation.searched_scope {
            out.push_str(&format!("- {scope}\n"));
        }
    }
    if !result.investigation.unresolved.is_empty() {
        out.push_str("\nUnresolved questions:\n");
        for question in &result.investigation.unresolved {
            out.push_str(&format!("- {question}\n"));
        }
    }
    if !result.investigation.limitations.is_empty() {
        out.push_str("\nInvestigation limitations:\n");
        for limitation in &result.investigation.limitations {
            out.push_str(&format!("- {limitation}\n"));
        }
    }
    if !handoff_limitations.is_empty() {
        out.push_str("\nHandoff limitations:\n");
        for limitation in handoff_limitations {
            out.push_str(&format!("- {limitation}\n"));
        }
    }
    out
}

fn append_evidence(
    out: &mut String,
    spans: &[EvidenceSpan],
    omissions: &HandoffOmissions,
) -> Vec<Value> {
    let mut evidence = Vec::new();
    if !spans.is_empty() {
        out.push_str("\n\nSource context (validated repository spans):");
        for span in spans {
            out.push_str(&format!(
                "\n\n--- {}:{}-{} ---\n",
                span.path, span.start_line, span.end_line
            ));
            out.push_str(&span.text);
            evidence.push(json!({
                "path": span.path, "start_line": span.start_line, "end_line": span.end_line,
                "text": span.text, "truncated": span.truncated,
            }));
        }
    }
    if omissions.omitted_source_spans > 0 || omissions.truncated_spans > 0 {
        out.push_str(&format!(
            "\n\nSource context note: {} span{} omitted and {} span{} truncated by the single MCP handoff budget or safe source checks. The findings above are not converted into unanswered questions solely for that reason.",
            omissions.omitted_source_spans,
            if omissions.omitted_source_spans == 1 { " was" } else { "s were" },
            omissions.truncated_spans,
            if omissions.truncated_spans == 1 { " was" } else { "s were" },
        ));
    }
    evidence
}

/// Trim only as much model-authored report material as the full-result fit
/// requires. The caller owns the single transport ceiling; this helper has no
/// independent output limit.
fn bound_report_and_track(
    result: &mut ScoutResult,
    target: usize,
    omissions: &mut HandoffOmissions,
) -> bool {
    let before = result.citations.len();
    let changed = bound_report(result, target);
    if changed {
        // The scout assessed its full report, not this shortened version.
        result.investigation.confidence = Default::default();
    }
    omissions.report_omitted_citations += before.saturating_sub(result.citations.len());
    changed
}

fn bound_report(result: &mut ScoutResult, target: usize) -> bool {
    let mut changed = false;
    while report_size(result) > target {
        if shorten_string(&mut result.summary) {
            changed = true;
        } else if let Some(finding) = result
            .investigation
            .findings
            .iter_mut()
            .rev()
            .find(|finding| finding.answer.len() > 64)
        {
            shorten_string(&mut finding.answer);
            changed = true;
        } else if let Some(finding) = result
            .investigation
            .findings
            .iter_mut()
            .rev()
            .find(|finding| finding.question.len() > 64)
        {
            shorten_string(&mut finding.question);
            changed = true;
        } else if let Some(value) = result
            .investigation
            .searched_scope
            .iter_mut()
            .rev()
            .find(|value| value.len() > 64)
        {
            shorten_string(value);
            changed = true;
        } else if let Some(value) = result
            .investigation
            .unresolved
            .iter_mut()
            .rev()
            .find(|value| value.len() > 64)
        {
            shorten_string(value);
            changed = true;
        } else if let Some(value) = result
            .investigation
            .limitations
            .iter_mut()
            .rev()
            .find(|value| value.len() > 64)
        {
            shorten_string(value);
            changed = true;
        } else if !result.investigation.findings.is_empty() {
            result.investigation.findings.pop();
            changed = true;
        } else if !result.investigation.searched_scope.is_empty() {
            result.investigation.searched_scope.pop();
            changed = true;
        } else if !result.investigation.unresolved.is_empty() {
            result.investigation.unresolved.pop();
            changed = true;
        } else if !result.investigation.limitations.is_empty() {
            result.investigation.limitations.pop();
            changed = true;
        } else if !result.investigation.confidence.basis.is_empty() {
            result.investigation.confidence = Default::default();
            changed = true;
        } else if let Some(citation) = result.citations.iter_mut().rev().find(|citation| {
            citation
                .reason
                .as_ref()
                .is_some_and(|reason| reason.len() > 64)
        }) {
            citation.reason = None;
            changed = true;
        } else if !result.citations.is_empty() {
            result.citations.pop();
            changed = true;
        } else {
            break;
        }
    }
    changed
}

fn report_size(result: &ScoutResult) -> usize {
    serde_json::to_vec(&(&result.summary, &result.investigation, &result.citations))
        .map_or(usize::MAX, |bytes| bytes.len())
}

fn shorten_string(value: &mut String) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut end = value.len().saturating_mul(3) / 4;
    if end >= value.len() {
        end = value.len().saturating_sub(1);
    }
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

fn truncate_last_span_to_fit(
    spans: &mut [EvidenceSpan],
    result: &ScoutResult,
    omissions: &HandoffOmissions,
    handoff_limitations: &[String],
) -> bool {
    let Some(index) = spans.len().checked_sub(1) else {
        return false;
    };
    let original = spans[index].text.clone();
    if original.is_empty() {
        return false;
    }
    let marker = "\n[Source span truncated by the MCP handoff budget.]";
    // Search over character-boundary indices rather than raw bytes. Besides
    // avoiding invalid UTF-8 slices, this keeps the binary search monotonic
    // for multi-byte source text.
    let boundaries = original
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(original.len()))
        .collect::<Vec<_>>();
    let mut low = 1usize;
    let mut high = boundaries.len() - 1;
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let end = boundaries[middle];
        spans[index].text = format!("{}{}", &original[..end], marker);
        spans[index].truncated = true;
        let candidate = build_handoff_response(result, spans, omissions, handoff_limitations);
        if response_size(&candidate) <= MAX_HANDOFF_BYTES {
            best = Some(end);
            low = middle.saturating_add(1);
        } else {
            high = middle - 1;
        }
    }
    let Some(end) = best else {
        spans[index].text = original;
        spans[index].truncated = false;
        return false;
    };
    spans[index].text = format!("{}{}", &original[..end], marker);
    spans[index].truncated = true;
    true
}

fn evidence_excerpts(root: &Path, citations: &[ValidatedCitation]) -> EvidenceBundle {
    #[derive(Debug)]
    struct PathRanges {
        path: String,
        ranges: Vec<(u32, u32, usize)>,
    }

    let mut groups: Vec<PathRanges> = Vec::new();
    let mut bundle = EvidenceBundle::default();
    for citation in citations {
        if citation.start_line == 0 || citation.end_line < citation.start_line {
            bundle.omitted_citations += 1;
            bundle.omitted_spans += 1;
            continue;
        }
        let Some(group) = groups.iter_mut().find(|group| group.path == citation.path) else {
            groups.push(PathRanges {
                path: citation.path.clone(),
                ranges: vec![(citation.start_line, citation.end_line, 1)],
            });
            continue;
        };
        group
            .ranges
            .push((citation.start_line, citation.end_line, 1));
    }

    for group in groups {
        let Ok(path) = repotracer_repo_tools::resolve_in_root(root, &group.path) else {
            bundle.omitted_citations += group.ranges.iter().map(|range| range.2).sum::<usize>();
            bundle.omitted_spans += group.ranges.len();
            continue;
        };
        let Ok(source) = std::fs::read_to_string(path) else {
            bundle.omitted_citations += group.ranges.iter().map(|range| range.2).sum::<usize>();
            bundle.omitted_spans += group.ranges.len();
            continue;
        };
        let line_count = source.lines().count() as u32;
        let mut ranges = group.ranges;
        ranges.sort_by_key(|range| (range.0, range.1));
        let mut merged: Vec<(u32, u32, usize)> = Vec::new();
        for (start, end, count) in ranges {
            if start > line_count || end > line_count {
                bundle.omitted_citations += count;
                bundle.omitted_spans += 1;
                continue;
            }
            if let Some(previous) = merged.last_mut() {
                if start <= previous.1.saturating_add(1) {
                    previous.1 = previous.1.max(end);
                    previous.2 += count;
                    continue;
                }
            }
            merged.push((start, end, count));
        }
        for (start_line, end_line, citation_count) in merged {
            let text = render_source_span(&source, start_line, end_line);
            if text.is_empty() {
                bundle.omitted_citations += citation_count;
                bundle.omitted_spans += 1;
            } else {
                bundle.spans.push(EvidenceSpan {
                    priority: citations
                        .iter()
                        .position(|citation| {
                            citation.path == group.path
                                && citation.start_line <= end_line
                                && citation.end_line >= start_line
                        })
                        .expect("span was constructed from a citation"),
                    path: group.path.clone(),
                    start_line,
                    end_line,
                    text,
                    citation_count,
                    truncated: false,
                });
            }
        }
    }
    bundle
}

fn render_source_span(source: &str, start: u32, end: u32) -> String {
    let mut excerpt = String::new();
    let take = end.saturating_sub(start).saturating_add(1) as usize;
    for (index, line) in source
        .lines()
        .enumerate()
        .skip(start.saturating_sub(1) as usize)
        .take(take)
    {
        excerpt.push_str(&format!("{}: {}\n", index + 1, line));
    }
    while excerpt.ends_with('\n') {
        excerpt.pop();
    }
    excerpt
}
fn repo_scout_tool_def() -> Value {
    json!({
        "name": "repo_scout",
        "description": REPO_SCOUT_DESC,
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        },
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language repository question. For changes, include relevant user requirements and compatibility constraints; the scout does not automatically receive the parent conversation. Separate requested behavior from assumptions about existing code."
                },
                "repository": {
                    "type": "string",
                    "description": "Directory to investigate, including another checkout or worktree. Prefer its absolute path; relative paths resolve from the server startup directory. Omit to use the returned conversation's repository, or the startup directory for a new investigation. The selected directory is the read-only source boundary and is reported in the response."
                },
                "focus": {
                    "type": "string",
                    "description": "Optional file or directory within the selected repository to bias exploration. Relative paths stay within that repository. With no repository or remembered conversation, an absolute focus in another Git checkout selects that checkout, including worktrees. For a non-Git directory set repository explicitly. Paths containing '..' and symlink escapes from the selected repository remain rejected."
                },
                "investigation": {
                    "type": "object", "additionalProperties": false,
                    "description": "Optional JSON object of investigation hints, for example {\"intent\":\"diagnose\",\"reasoning_effort\":\"high\"}. Query alone can request any repository investigation.",
                    "properties": {
                        "reasoning_effort": {"type":"string", "enum":["low","medium","high","xhigh","max"], "description":"Native subscription effort for this investigation only. Medium suits straightforward lookups; high suits diagnosis, indirect relationships, or cross-component change impact. Omit to use configured effort. Supported levels depend on the selected provider and model."},
                        "intent": {"type":"string", "enum":["locate","explain","change_impact","diagnose","inventory"]},
                        "questions": {"type":"array", "maxItems":24, "items":{"type":"string"}},
                        "conversation_id": {"type":"string", "maxLength":128, "description":"Use conversation.id from an earlier response for a related follow-up. Omit for a new independent investigation. A caller-chosen ID is also accepted. Reuse is bounded: conversation.status reports resumed, fresh, or unknown. Supply the current question and necessary context, especially after a fresh start. The handle stays bound to its selected repository."},
                        "known_context": {"type":"string", "description":"Context already known to the parent; unverified until checked."},
                        "target_paths": {"type":"array", "maxItems":32, "items":{"type":"string", "description":"Repository-relative file or directory hint. Relative nonexistent paths are allowed as search hints. Use '.' for the repository root. An absolute path is accepted only when it canonicalizes inside this repository and is returned relative. Examples: 'crates/core/src/lib.rs', '.', or '/work/repo/crates/core' when '/work/repo' is this repository. Paths containing '..', outside paths, and symlink escapes are rejected."}}
                    }
                }
            },
            "required": ["query"]
        }
    })
}

fn repo_scout_prompt_def() -> Value {
    json!({
        "name": "repo_scout",
        "description": "Delegate a repository question or a failed targeted lookup to RepoTracer.",
        "arguments": [{
            "name": "query",
            "description": "Precise semantic repository question or flow to trace.",
            "required": true
        }]
    })
}

fn repo_scout_prompt(params: Value) -> Result<Value, Value> {
    if params.get("name").and_then(Value::as_str) != Some("repo_scout") {
        return Err(rpc_error(-32602, "Unknown prompt".into()));
    }
    let query = params
        .get("arguments")
        .and_then(|value| value.get("query"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if query.is_empty() {
        return Err(rpc_error(-32602, "`query` is required".into()));
    }
    Ok(json!({
        "description": "Explore the repository with RepoTracer, then use its explanation and embedded source context to inform the decision.",
        "messages": [{
            "role": "user",
            "content": {
                "type": "text",
                "text": format!("Call repo_scout with this question. Add context or an investigation intent when useful. Use its explanation and source context, and check any unresolved questions or limitations: {query}")
            }
        }]
    }))
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
}

fn rpc_error(code: i64, message: String) -> Value {
    json!({ "code": code, "message": message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn slow_git_selection_does_not_block_runtime() {
        use std::time::{Duration, Instant};
        const CHILD: &str = "REPOTRACER_SLOW_GIT_TEST";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let git = dir.path().join("git");
            std::fs::write(&git, "#!/bin/sh\nsleep 1\nprintf '%s\\n' \"$2\"\n").unwrap();
            std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::slow_git_selection_does_not_block_runtime",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        dir.path().display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let startup = tempfile::tempdir().unwrap();
        let selected = tempfile::tempdir().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(None));
        let server = McpServer::new(
            Arc::new(CapturingScout {
                request: captured.clone(),
            }),
            startup.path().to_owned(),
        );
        let started = Instant::now();
        let (response, follow_up, elapsed) = tokio::join!(
            server.tools_call(json!({"name": "repo_scout", "arguments": {
                "query": "first", "focus": selected.path().canonicalize().unwrap(),
                "investigation": {"conversation_id": "same"}
            }})),
            server.tools_call(json!({"name": "repo_scout", "arguments": {
                "query": "follow-up", "repository": selected.path(),
                "investigation": {"conversation_id": "same"}
            }})),
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                started.elapsed()
            }
        );
        assert!(!response.unwrap()["isError"].as_bool().unwrap_or(false));
        assert!(!follow_up.unwrap()["isError"].as_bool().unwrap_or(false));
        assert_eq!(
            captured.lock().unwrap().as_ref().unwrap().query,
            "follow-up"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "Git blocked the runtime for {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn malformed_investigation_explains_retry_without_starting_scout() {
        let root = tempfile::tempdir().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(None));
        let server = McpServer::new(
            Arc::new(CapturingScout {
                request: captured.clone(),
            }),
            root.path().to_owned(),
        );
        let error = server
            .tools_call(json!({"name":"repo_scout", "arguments": {
                "query":"trace failures", "investigation":"<parameter name=\"intent\">diagnose"
            }}))
            .await
            .unwrap_err();
        assert_eq!(error["code"], -32602);
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("Supply a JSON object"));
        assert!(captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn selected_repository_and_returned_handle_follow_the_actual_checkout() {
        let startup = tempfile::tempdir().unwrap();
        let selected = tempfile::tempdir().unwrap();
        std::fs::write(startup.path().join("src.rs"), "WRONG CHECKOUT\n").unwrap();
        std::fs::write(selected.path().join("src.rs"), "SELECTED CHECKOUT\n").unwrap();
        let captured = Arc::new(std::sync::Mutex::new(None));
        let server = McpServer::new(
            Arc::new(CapturingScout {
                request: captured.clone(),
            }),
            startup.path().to_owned(),
        );
        let first = server
            .tools_call(json!({"name":"repo_scout", "arguments": {
                "query":"trace selected", "repository":selected.path(), "focus":"src.rs"
            }}))
            .await
            .unwrap();
        let id = first["structuredContent"]["conversation"]["id"]
            .as_str()
            .unwrap();
        assert!(id.starts_with("rt-"));
        assert_eq!(
            first["structuredContent"]["repository"],
            selected
                .path()
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(
            first["structuredContent"]["conversation"]["status"],
            "unknown"
        );
        assert!(first["content"][0]["text"].as_str().unwrap().contains(id));
        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(request.root, selected.path().canonicalize().unwrap());
        assert_eq!(request.investigation.conversation_id.as_deref(), Some(id));
        server
            .tools_call(json!({"name":"repo_scout", "arguments": {
                "query":"now trace its caller", "investigation":{"conversation_id":id}
            }}))
            .await
            .unwrap();
        assert_eq!(
            captured.lock().unwrap().as_ref().unwrap().root,
            request.root
        );
        let wrong = server.tools_call(json!({"name":"repo_scout", "arguments": {
            "query":"different root", "repository":startup.path(), "investigation":{"conversation_id":id}
        }})).await.unwrap();
        assert_eq!(wrong["isError"], true);
        assert!(wrong["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("conversation belongs"));
    }

    #[tokio::test]
    async fn explicit_root_evidence_is_read_from_selected_checkout_not_startup() {
        struct SourceScout;
        #[async_trait::async_trait]
        impl ScoutBackend for SourceScout {
            async fn scout(&self, _: ScoutRequest) -> anyhow::Result<ScoutResult> {
                let mut result = scout_result(0);
                result.citations.push(ValidatedCitation {
                    path: "src.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    reason: None,
                });
                result.stats.thread_turn = 1;
                Ok(result)
            }
        }
        let startup = tempfile::tempdir().unwrap();
        let selected = tempfile::tempdir().unwrap();
        std::fs::write(startup.path().join("src.rs"), "WRONG\n").unwrap();
        std::fs::write(selected.path().join("src.rs"), "RIGHT\n").unwrap();
        let server = McpServer::new(Arc::new(SourceScout), startup.path().to_owned());
        let response = server
            .tools_call(json!({"name":"repo_scout","arguments":{
                "query":"read", "repository":selected.path()
            }}))
            .await
            .unwrap();
        assert_eq!(
            response["structuredContent"]["evidence"][0]["text"],
            "1: RIGHT"
        );
        assert_eq!(
            response["structuredContent"]["conversation"]["status"],
            "fresh"
        );
    }

    #[tokio::test]
    async fn unknown_generated_handle_requires_explicit_repository() {
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = McpServer::new(
            Arc::new(CountingScout {
                calls: calls.clone(),
            }),
            root.path().to_owned(),
        );
        let response = server
            .tools_call(json!({"name":"repo_scout","arguments":{
                "query":"continue", "investigation":{"conversation_id":"rt-expired"}
            }}))
            .await
            .unwrap();
        assert_eq!(response["isError"], true);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn parent_resume_status_uses_first_attempt_not_adaptive_second_turn() {
        let mut stats = repotracer_core::ScoutStats {
            thread_turn: 2,
            attempts: vec![
                repotracer_core::ScoutAttemptStats {
                    thread_turn: 1,
                    ..Default::default()
                },
                repotracer_core::ScoutAttemptStats {
                    thread_turn: 2,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        set_conversation(&mut stats, "test", Path::new("/repo"));
        assert_eq!(stats.conversation.as_ref().unwrap().status, "fresh");
        stats.attempts[0].thread_turn = 2;
        set_conversation(&mut stats, "test", Path::new("/repo"));
        assert_eq!(stats.conversation.as_ref().unwrap().status, "resumed");
    }

    fn embedded_text<'a>(_response: &Value, text: &'a Value) -> &'a str {
        text.as_str().unwrap()
    }

    #[test]
    fn standalone_structured_answer_preserves_unicode_and_repeated_source_text() {
        let root = tempfile::tempdir().unwrap();
        for path in ["α.rs", "β.rs"] {
            std::fs::write(root.path().join(path), "let 設定 = \"λ\";\n").unwrap();
        }
        let mut result = scout_result(0);
        result.summary = "Préface λ with repeated source: 1: let 設定 = \"λ\";".into();
        result.citations = ["α.rs", "β.rs"]
            .iter()
            .map(|path| ValidatedCitation {
                path: (*path).into(),
                start_line: 1,
                end_line: 1,
                reason: None,
            })
            .collect();
        let response = handoff_response(root.path(), result);
        let evidence = response["structuredContent"]["evidence"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), 2);
        for span in evidence {
            assert_eq!(
                embedded_text(&response, &span["text"]),
                "1: let 設定 = \"λ\";"
            );
        }
        assert_eq!(evidence[0]["text"], evidence[1]["text"]);
        assert!(
            embedded_text(&response, &response["structuredContent"]["report"])
                .contains("Préface λ")
        );
    }

    #[test]
    fn compatibility_fallback_does_not_halve_source_space() {
        let root = tempfile::tempdir().unwrap();
        let mut result = scout_result(0);
        result.summary = "Useful narrative. ".repeat(120);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        for path in ["parser.rs", "main.rs", "variables.rs"] {
            std::fs::write(
                root.path().join(path),
                (1..=100)
                    .map(|line| format!("{path} line {line} {}\n", "x".repeat(60)))
                    .collect::<String>(),
            )
            .unwrap();
            result.citations.push(ValidatedCitation {
                path: path.into(),
                start_line: 1,
                end_line: 100,
                reason: None,
            });
        }
        let response = handoff_response(root.path(), result);
        let structured = &response["structuredContent"];
        assert_eq!(structured["evidence"].as_array().unwrap().len(), 3);
        assert_eq!(structured["evidence_omissions"]["explicit"], false);
        assert!(embedded_text(&response, &structured["evidence"][2]["text"])
            .contains("100: variables.rs"));
        assert!(response_size(&response) <= MAX_HANDOFF_BYTES);
        let text = response["content"][0]["text"].as_str().unwrap();
        assert!(text.len() * 2 > MAX_HANDOFF_BYTES);
        assert!(serde_json::to_vec(&response).unwrap().len() <= 2 * MAX_HANDOFF_BYTES + 64);
    }

    #[test]
    fn important_large_implementation_survives_smaller_background() {
        let root = tempfile::tempdir().unwrap();
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        for (path, bytes) in [
            ("implementation.rs", 18_000),
            ("regression.rs", 4_000),
            ("background.md", 8_000),
            ("extra.md", 8_000),
        ] {
            std::fs::write(root.path().join(path), "x".repeat(bytes)).unwrap();
            result.citations.push(ValidatedCitation {
                path: path.into(),
                start_line: 1,
                end_line: 1,
                reason: None,
            });
        }
        let response = handoff_response(root.path(), result);
        let structured = &response["structuredContent"];
        let evidence = structured["evidence"].as_array().unwrap();
        assert!(evidence
            .iter()
            .any(|span| span["path"] == "implementation.rs"));
        assert!(evidence.iter().any(|span| span["path"] == "regression.rs"));
        assert!(!evidence.iter().any(|span| span["path"] == "extra.md"));
        assert!(response_size(&response) <= MAX_HANDOFF_BYTES);
        assert_eq!(structured["investigation"]["status"], "complete");
    }

    #[test]
    fn merged_ranges_retain_their_earliest_task_priority() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        let citations = [(3, 4), (1, 2)].map(|(start_line, end_line)| ValidatedCitation {
            path: "lib.rs".into(),
            start_line,
            end_line,
            reason: None,
        });
        let bundle = evidence_excerpts(root.path(), &citations);
        assert_eq!(bundle.spans.len(), 1);
        assert_eq!(bundle.spans[0].priority, 0);
        assert_eq!(bundle.spans[0].citation_count, 2);
    }

    #[test]
    fn an_oversized_first_span_does_not_evict_a_small_later_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("large.rs"), "x".repeat(MAX_HANDOFF_BYTES)).unwrap();
        std::fs::write(root.path().join("deciding.rs"), "fn decides() {}\n").unwrap();
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.citations = ["large.rs", "deciding.rs"]
            .iter()
            .map(|path| ValidatedCitation {
                path: (*path).into(),
                start_line: 1,
                end_line: 1,
                reason: None,
            })
            .collect();
        let response = handoff_response(root.path(), result);
        let structured = &response["structuredContent"];
        assert_eq!(structured["evidence"].as_array().unwrap().len(), 1);
        assert_eq!(structured["evidence"][0]["path"], "deciding.rs");
        assert_eq!(structured["evidence_omissions"]["omitted_spans"], 1);
        assert_eq!(structured["investigation"]["status"], "complete");
        assert_eq!(structured["citations"].as_array().unwrap().len(), 2);
        assert_eq!(structured["citations"][0]["source_status"], "omitted");
        assert_eq!(structured["citations"][1]["source_status"], "included");
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("large.rs:1-1 (omitted)"));
    }

    #[test]
    fn repo_scout_prompt_requires_query_and_enforces_handoff() {
        let error =
            repo_scout_prompt(json!({ "name": "repo_scout", "arguments": {} })).unwrap_err();
        assert_eq!(error["code"], -32602);

        let result = repo_scout_prompt(json!({
            "name": "repo_scout",
            "arguments": { "query": "trace refresh-token rotation" }
        }))
        .unwrap();
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("trace refresh-token rotation"));
        assert!(text.contains("unresolved questions"));
        assert!(!text.contains("MUST answer immediately"));
        assert!(repo_scout_prompt_def()["description"]
            .as_str()
            .unwrap()
            .contains("repository question"));
        assert!(!repo_scout_prompt_def()["description"]
            .as_str()
            .unwrap()
            .contains("broad"));
        assert_eq!(repo_scout_prompt_def()["arguments"][0]["required"], true);
    }

    #[test]
    fn repo_scout_description_matches_routing_contract() {
        let tool = repo_scout_tool_def();
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("read-only colleague"));
        assert!(description.contains("line-numbered repository source"));
        assert!(description.contains("conclusions are scout judgments"));
        assert!(description.contains("Use either representation, not both"));
    }

    #[test]
    fn repo_scout_is_declared_read_only() {
        let annotations = &repo_scout_tool_def()["annotations"];
        assert_eq!(annotations["readOnlyHint"], true);
        assert_eq!(annotations["destructiveHint"], false);
        assert_eq!(annotations["openWorldHint"], false);
    }

    #[test]
    fn confidence_cannot_bypass_the_handoff_size_bound() {
        let root = tempfile::tempdir().unwrap();
        let mut result = scout_result(0);
        result.investigation.confidence = repotracer_core::InvestigationConfidence {
            level: repotracer_core::ConfidenceLevel::High,
            basis: "λ".repeat(MAX_HANDOFF_BYTES),
        };
        let response = handoff_response(root.path(), result);
        assert!(response_size(&response) <= MAX_HANDOFF_BYTES);
        assert_eq!(
            response["structuredContent"]["investigation"]["confidence"]["level"],
            "unknown"
        );
        assert_eq!(
            response["structuredContent"]["investigation"]["status"],
            "partial"
        );
    }

    fn scout_result(citation_count: usize) -> ScoutResult {
        ScoutResult {
            investigation: Default::default(),
            summary: "Focused evidence".into(),
            citations: (0..citation_count)
                .map(|index| repotracer_core::ValidatedCitation {
                    path: format!("src/{index}.rs"),
                    start_line: 1,
                    end_line: 2,
                    reason: Some("relevant".into()),
                })
                .collect(),
            stats: repotracer_core::ScoutStats::default(),
            raw_final: None,
        }
    }

    struct CountingScout {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ScoutBackend for CountingScout {
        async fn scout(&self, _request: ScoutRequest) -> anyhow::Result<ScoutResult> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(scout_result(1))
        }
    }

    struct CapturingScout {
        request: Arc<std::sync::Mutex<Option<ScoutRequest>>>,
    }

    struct TerminalFailureScout;

    #[async_trait::async_trait]
    impl ScoutBackend for TerminalFailureScout {
        async fn scout(&self, _request: ScoutRequest) -> anyhow::Result<ScoutResult> {
            let stats = repotracer_core::ScoutStats {
                model: "fixture".into(),
                usage_status: repotracer_core::UsageStatus::Partial,
                reported_cost_usd: Some(1.25),
                usage: repotracer_core::UsageStats {
                    input_tokens: Some(100),
                    cached_input_tokens: Some(60),
                    cache_write_input_tokens: Some(10),
                    output_tokens: Some(19),
                    ..Default::default()
                },
                ..Default::default()
            };
            Err(anyhow::Error::new(ScoutBackendError::new(
                "Claude investigation failed: \"error_max_turns\" after 7 turns and 0 tool calls; reported usage (partial): {}",
                stats,
            )))
        }
    }

    struct OrdinaryFailureScout;

    #[async_trait::async_trait]
    impl ScoutBackend for OrdinaryFailureScout {
        async fn scout(&self, _request: ScoutRequest) -> anyhow::Result<ScoutResult> {
            Err(anyhow::anyhow!("ordinary backend failure"))
        }
    }

    fn valid_tool_call() -> Value {
        json!({
            "name": "repo_scout",
            "arguments": { "query": "trace" }
        })
    }

    #[tokio::test]
    async fn terminal_backend_failure_keeps_stats_in_mcp_tool_result() {
        let root = tempfile::tempdir().unwrap();
        let server = McpServer::new(Arc::new(TerminalFailureScout), root.path().to_path_buf());

        let response = server.tools_call(valid_tool_call()).await.unwrap();

        assert_eq!(response["isError"], true);
        assert_eq!(response["structuredContent"]["stats"]["model"], "fixture");
        assert_eq!(
            response["structuredContent"]["stats"]["reported_cost_usd"],
            1.25
        );
        assert_eq!(
            response["structuredContent"]["stats"]["usage_status"],
            "partial"
        );
        let usage = &response["structuredContent"]["stats"]["usage"];
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["cached_input_tokens"], 60);
        assert_eq!(usage["cache_write_input_tokens"], 10);
        assert_eq!(usage["output_tokens"], 19);
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("error_max_turns"));
    }

    #[tokio::test]
    async fn ordinary_backend_failure_remains_json_rpc_error() {
        let root = tempfile::tempdir().unwrap();
        let server = McpServer::new(Arc::new(OrdinaryFailureScout), root.path().to_path_buf());

        let error = server.tools_call(valid_tool_call()).await.unwrap_err();

        assert_eq!(error["code"], -32000);
        assert_eq!(error["message"], "ordinary backend failure");
    }

    #[async_trait::async_trait]
    impl ScoutBackend for CapturingScout {
        async fn scout(&self, request: ScoutRequest) -> anyhow::Result<ScoutResult> {
            *self.request.lock().unwrap() = Some(request);
            Ok(scout_result(0))
        }
    }

    #[tokio::test]
    async fn mcp_normalizes_safe_absolute_paths_before_backend_validation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "fn start() {}\n").unwrap();
        let captured = Arc::new(std::sync::Mutex::new(None));
        let server = McpServer::new(
            Arc::new(CapturingScout {
                request: captured.clone(),
            }),
            root.path().to_path_buf(),
        );

        server
            .tools_call(json!({
                "name": "repo_scout",
                "arguments": {
                    "query": "trace",
                    "focus": root.path().join("src").display().to_string(),
                    "investigation": {
                        "target_paths": [root.path().join("src/lib.rs").display().to_string()]
                    }
                }
            }))
            .await
            .unwrap();

        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(request.focus, Some(PathBuf::from("src")));
        assert_eq!(request.investigation.target_paths, ["src/lib.rs"]);
    }

    #[tokio::test]
    async fn mcp_keeps_a_nonexistent_relative_focus_as_a_hint() {
        let root = tempfile::tempdir().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(None));
        let server = McpServer::new(
            Arc::new(CapturingScout {
                request: captured.clone(),
            }),
            root.path().to_path_buf(),
        );
        let focus = ".codex-worktrees/studio-terminal-preparation-20260906-e9cca8516/studio/backend/core/inference/windows_sandbox";

        server
            .tools_call(json!({
                "name": "repo_scout",
                "arguments": {"query": "trace sandbox setup", "focus": focus}
            }))
            .await
            .unwrap();

        let request = captured.lock().unwrap().clone().unwrap();
        assert_eq!(request.focus, Some(PathBuf::from(focus)));
    }

    #[tokio::test]
    async fn mcp_rejects_outside_paths_before_starting_backend() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let outside = parent.path().join("secret.rs");
        std::fs::write(&outside, "secret\n").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = McpServer::new(
            Arc::new(CountingScout {
                calls: calls.clone(),
            }),
            root,
        );

        let response = server
            .tools_call(json!({
                "name": "repo_scout",
                "arguments": {
                    "query": "trace",
                    "investigation": {"target_paths": [outside.display().to_string()]}
                }
            }))
            .await
            .unwrap();

        assert_eq!(response["isError"], true);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outside"));
    }

    #[tokio::test]
    async fn explicit_investigation_runs_even_in_a_tiny_repository() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = McpServer::new(
            Arc::new(CountingScout {
                calls: calls.clone(),
            }),
            root.path().to_path_buf(),
        );

        let response = server
            .tools_call(json!({
                "name": "repo_scout",
                "arguments": { "query": "Find the shared root of this narrow bug" }
            }))
            .await
            .unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            response["structuredContent"]["investigation"]["status"],
            "partial"
        );
    }

    #[tokio::test]
    async fn larger_repository_still_starts_scout() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=40 {
            std::fs::write(root.path().join(format!("{index}.rs")), "// source\n").unwrap();
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = McpServer::new(
            Arc::new(CountingScout {
                calls: calls.clone(),
            }),
            root.path().to_path_buf(),
        );

        let response = server
            .tools_call(json!({
                "name": "repo_scout",
                "arguments": { "query": "Trace a broad cross-component flow" }
            }))
            .await
            .unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(response["structuredContent"]["next_action"]
            .as_str()
            .unwrap()
            .contains("Partial investigation"));
    }

    #[test]
    fn handoff_keeps_more_than_twelve_citations_without_intent_caps() {
        let root = tempfile::tempdir().unwrap();
        let citations = (0..13)
            .map(|index| {
                let path = format!("{index}.rs");
                std::fs::write(
                    root.path().join(&path),
                    format!("const VALUE_{index}: u8 = {index};\n"),
                )
                .unwrap();
                ValidatedCitation {
                    path,
                    start_line: 1,
                    end_line: 1,
                    reason: Some("direct implementation".into()),
                }
            })
            .collect::<Vec<_>>();
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.citations = citations;
        let response = handoff_response(root.path(), result);
        assert_eq!(
            response["structuredContent"]["citations"]
                .as_array()
                .unwrap()
                .len(),
            13
        );
        assert_eq!(
            response["structuredContent"]["evidence"]
                .as_array()
                .unwrap()
                .len(),
            13
        );
        assert_eq!(response["structuredContent"]["omitted_citations"], 0);
        assert!(response_size(&response) <= MAX_HANDOFF_BYTES);
    }

    #[test]
    fn full_large_source_span_is_not_capped_at_legacy_excerpt_size() {
        let root = tempfile::tempdir().unwrap();
        let source = (1..=100)
            .map(|line| format!("source line {line}\n"))
            .collect::<String>();
        std::fs::write(root.path().join("lib.rs"), &source).unwrap();
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.citations = vec![ValidatedCitation {
            path: "lib.rs".into(),
            start_line: 1,
            end_line: 100,
            reason: Some("entire function context".into()),
        }];
        let response = handoff_response(root.path(), result);
        let evidence = &response["structuredContent"]["evidence"][0];
        let evidence_text = embedded_text(&response, &evidence["text"]);
        assert!(evidence_text.contains("1: source line 1"));
        assert!(evidence_text.contains("100: source line 100"));
        assert_eq!(evidence["truncated"], false);
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("100: source line 100"));
    }

    #[test]
    fn overlapping_and_contained_spans_are_one_source_context_block() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("lib.rs"),
            (1..=12)
                .map(|line| format!("line {line}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.citations = vec![
            ValidatedCitation {
                path: "lib.rs".into(),
                start_line: 1,
                end_line: 5,
                reason: None,
            },
            ValidatedCitation {
                path: "lib.rs".into(),
                start_line: 3,
                end_line: 8,
                reason: None,
            },
            ValidatedCitation {
                path: "lib.rs".into(),
                start_line: 4,
                end_line: 4,
                reason: None,
            },
            ValidatedCitation {
                path: "lib.rs".into(),
                start_line: 9,
                end_line: 9,
                reason: None,
            },
        ];
        let response = handoff_response(root.path(), result);
        let evidence = response["structuredContent"]["evidence"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["start_line"], 1);
        assert_eq!(evidence[0]["end_line"], 9);
        assert!(embedded_text(&response, &evidence[0]["text"]).contains("9: line 9"));
    }

    #[test]
    fn each_rendering_is_self_contained_and_has_one_copy_of_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn answer() { return 42; }\n").unwrap();
        let mut result = scout_result(0);
        result.summary = "The answer is returned by the leaf function.".into();
        result.investigation.confidence = repotracer_core::InvestigationConfidence {
            level: repotracer_core::ConfidenceLevel::High,
            basis: "Read the leaf function; no execution was needed for this location question."
                .into(),
        };
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.investigation.findings = vec![repotracer_core::Finding {
            question: "where is the answer?".into(),
            answer: "The leaf function returns the value.".into(),
            citations: vec![ValidatedCitation {
                path: "lib.rs".into(),
                start_line: 1,
                end_line: 1,
                reason: Some("leaf".into()),
            }],
        }];
        result.citations = result.investigation.findings[0].citations.clone();
        let response = handoff_response(root.path(), result);
        let text = response["content"][0]["text"].as_str().unwrap();
        let structured = &response["structuredContent"];
        assert!(text.contains("Scout-reported confidence: High"));
        assert!(text.contains("Read the leaf function"));
        assert_eq!(structured["investigation"]["confidence"]["level"], "high");
        assert!(text.contains("The leaf function returns the value."));
        assert!(text.contains("Sources: lib.rs:1-1"));
        assert!(text.contains("1: fn answer() { return 42; }"));
        assert_eq!(structured["handoff_version"], 3);
        let report = embedded_text(&response, &structured["report"]);
        assert!(report.contains("The answer is returned"));
        assert!(report.contains("The leaf function returns the value."));
        assert!(embedded_text(&response, &structured["evidence"][0]["text"]).contains("return 42"));
        assert!(structured.get("summary").is_none());
        assert!(structured["investigation"].get("findings").is_none());
        assert!(structured.get("report_ref").is_none());
        assert!(structured["evidence"][0].get("text_ref").is_none());
        assert!(structured.to_string().contains("return 42"));
        assert_eq!(
            structured
                .to_string()
                .matches("The leaf function returns the value.")
                .count(),
            1
        );
    }

    #[test]
    fn source_budget_omissions_are_explicit_and_do_not_create_unresolved_questions() {
        let root = tempfile::tempdir().unwrap();
        let source = (1..=40_000)
            .map(|line| format!("line {line}: λ{}\n", "x".repeat(12)))
            .collect::<String>();
        std::fs::write(root.path().join("huge.rs"), source).unwrap();
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.investigation.findings = vec![repotracer_core::Finding {
            question: "what is in the function?".into(),
            answer: "The source span contains the function.".into(),
            citations: vec![ValidatedCitation {
                path: "huge.rs".into(),
                start_line: 1,
                end_line: 40_000,
                reason: Some("large source".into()),
            }],
        }];
        result.citations = result.investigation.findings[0].citations.clone();
        let response = handoff_response(root.path(), result);
        assert!(response_size(&response) <= MAX_HANDOFF_BYTES);
        assert_eq!(
            response["structuredContent"]["evidence_omissions"]["explicit"],
            true
        );
        assert_eq!(
            response["structuredContent"]["evidence"][0]["truncated"],
            true
        );
        assert_eq!(
            response["structuredContent"]["citations"][0]["source_status"],
            "truncated"
        );
        assert!(response["structuredContent"]["investigation"]["unresolved"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            response["structuredContent"]["investigation"]["status"],
            "complete"
        );
        assert!(!response["structuredContent"]["next_action"]
            .as_str()
            .unwrap()
            .contains("Partial investigation"));
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("truncated"));
    }

    #[test]
    fn malformed_or_outside_citations_never_embed_outside_source() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            parent.path().join("secret.rs"),
            "DO NOT EMBED THIS SECRET\n",
        )
        .unwrap();
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.citations = vec![
            ValidatedCitation {
                path: "../secret.rs".into(),
                start_line: 1,
                end_line: 1,
                reason: None,
            },
            ValidatedCitation {
                path: "missing.rs".into(),
                start_line: 0,
                end_line: 1,
                reason: None,
            },
        ];
        let response = handoff_response(&root, result);
        assert!(!response.to_string().contains("DO NOT EMBED THIS SECRET"));
        assert_eq!(
            response["structuredContent"]["evidence"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            response["structuredContent"]["evidence_omissions"]["explicit"],
            true
        );
    }

    #[test]
    fn oversized_report_keeps_explicit_budget_warning_and_fits_transport() {
        let mut result = scout_result(1);
        result.summary = "x".repeat(100_000);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.investigation.limitations = vec!["y".repeat(100_000)];
        let response = handoff_response(Path::new("."), result);
        assert!(response_size(&response) <= MAX_HANDOFF_BYTES);
        assert_eq!(
            response["structuredContent"]["investigation"]["status"],
            "partial"
        );
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("model-authored report exceeded"));
    }

    #[test]
    fn oversized_model_label_keeps_usage_inside_result_budget() {
        let mut result = scout_result(0);
        result.stats.model = "custom".repeat(MAX_HANDOFF_BYTES);
        result.stats.usage.input_tokens = Some(123);
        let response = handoff_response(Path::new("."), result);
        assert!(response_size(&response) <= MAX_HANDOFF_BYTES);
        assert_eq!(
            response["structuredContent"]["stats"]["usage"]["input_tokens"],
            123
        );
        assert_eq!(
            response["structuredContent"]["stats"]["model"],
            "[oversized model label omitted]"
        );
    }

    #[test]
    fn removed_source_span_does_not_claim_successful_truncation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn main() {}\n").unwrap();
        let mut result = scout_result(0);
        result.citations.push(ValidatedCitation {
            path: "lib.rs".into(),
            start_line: 1,
            end_line: 1,
            reason: None,
        });
        result.stats.model = "custom".repeat(MAX_HANDOFF_BYTES);
        let response = handoff_response(root.path(), result);
        let structured = &response["structuredContent"];
        assert_eq!(structured["evidence_omissions"]["omitted_spans"], 1);
        assert_eq!(structured["evidence_omissions"]["truncated_spans"], 0);
        assert!(!structured["handoff_limitations"]
            .to_string()
            .contains("A source span was truncated"));
    }

    #[test]
    fn duplicate_locations_do_not_remove_distinct_evidence() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn main() {}\n").unwrap();
        let citation = ValidatedCitation {
            path: "lib.rs".into(),
            start_line: 1,
            end_line: 1,
            reason: None,
        };
        let mut result = scout_result(0);
        result.investigation.status = repotracer_core::InvestigationStatus::Complete;
        result.investigation.findings = vec![repotracer_core::Finding {
            question: "where?".into(),
            answer: "lib.rs".into(),
            citations: vec![citation.clone()],
        }];
        result.citations = vec![citation; 10];
        let response = handoff_response(root.path(), result);
        assert_eq!(response["structuredContent"]["omitted_citations"], 0);
        assert_eq!(
            response["structuredContent"]["citations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            response["structuredContent"]["investigation"]["status"],
            "complete"
        );
    }

    #[test]
    fn report_citation_omissions_are_counted_when_report_is_bounded() {
        let mut result = scout_result(3);
        let mut omissions = HandoffOmissions::default();
        assert!(bound_report_and_track(&mut result, 0, &mut omissions));
        assert!(result.citations.is_empty());
        assert_eq!(omissions.report_omitted_citations, 3);
    }

    #[test]
    fn empty_handoff_explicitly_allows_normal_exploration() {
        let response = handoff_response(Path::new("."), scout_result(0));
        assert_eq!(response["structuredContent"]["next_action"], EMPTY_HANDOFF);
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("No validated citations"));
    }
}
