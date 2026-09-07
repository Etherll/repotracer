//! Shared investigation contract. Citation locations do not prove claim truth.
use crate::{parse_citations, validate_citation, Citation, ScoutRequest, ValidatedCitation};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InvestigationIntent {
    #[default]
    Locate,
    Explain,
    ChangeImpact,
    Diagnose,
    Inventory,
}

impl InvestigationIntent {
    pub fn strategy(self) -> &'static str {
        match self {
            Self::Locate => "Use the query to decide the depth needed. Find the relevant implementation and explain the requested behavior, with surrounding code, callers, or tests where useful.",
            Self::Explain => "Trace the requested behavior through relevant entry points, implementations, callers, and tests. Explain the material transitions and distinguish resolved relationships from textual matches.",
            Self::ChangeImpact => "Follow the target contract through relevant consumers, configuration, compatibility constraints, and tests. Include downstream effects that matter and identify dynamic or unresolved consumers.",
            Self::Diagnose => "Investigate plausible causes and gather evidence that distinguishes them. Separate observations from hypotheses and identify what remains uncertain.",
            Self::Inventory => "Build the requested inventory across the relevant scope. Note coverage limits, exclusions, and whether the result is exhaustive or partial.",
        }
    }

    /// The configured ceiling applies to every investigation intent.
    pub fn turn_limit(self, ceiling: u32) -> u32 {
        ceiling
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvestigationSpec {
    /// Per-request native subscription effort. Omit to use the user's configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Opt in to bounded subscription conversation reuse. Omit for independent questions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub intent: InvestigationIntent,
    /// Optional questions to clarify the objective. If omitted, the query is the sole question.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Context supplied by the parent, not independently verified evidence.
    #[serde(default)]
    pub known_context: String,
    #[serde(default)]
    pub target_paths: Vec<String>,
    /// Internal context for the one bounded adaptive continuation. It is not
    /// part of the caller's serialized request or the validation budget; the
    /// wrapper fills it with the first report and a narrow evidence gap.
    #[serde(skip)]
    pub continuation_context: Option<String>,
    /// Internal native capability evidence used to constrain the first
    /// report's continuation request. Empty means no escalation is permitted.
    #[serde(skip)]
    pub continuation_efforts: Option<Vec<String>>,
}

/// Normalize MCP-facing investigation paths to repository-relative paths.
///
/// Relative paths may name a file or directory that does not exist yet. In
/// that case the nearest existing parent must still resolve inside the
/// canonical repository. Existing paths are canonicalized, so symlink aliases
/// inside the repository become stable relative paths and symlink escapes are
/// rejected. Absolute paths are accepted only when canonicalization proves
/// that they are inside the repository.
fn normalize_request_paths(request: &mut ScoutRequest) -> anyhow::Result<()> {
    let root = canonical_repo_root(&request.root)?;

    if let Some(focus) = request.focus.take() {
        let normalized = normalize_repository_path(&root, &focus.to_string_lossy())?;
        request.focus = (normalized != ".").then(|| PathBuf::from(normalized));
    }

    for target in &mut request.investigation.target_paths {
        *target = normalize_repository_path(&root, target)?;
    }
    Ok(())
}

impl ScoutRequest {
    /// Normalize focus and target paths before validating a scout request.
    pub fn normalize_paths(&mut self) -> anyhow::Result<()> {
        normalize_request_paths(self)
    }
}

pub fn validate_request(request: &ScoutRequest) -> anyhow::Result<()> {
    if let Some(effort) = &request.investigation.reasoning_effort {
        anyhow::ensure!(
            matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max"),
            "unsupported reasoning_effort `{effort}`; use low, medium, high, xhigh, or max"
        );
    }
    if let Some(id) = &request.investigation.conversation_id {
        anyhow::ensure!(
            !id.trim().is_empty() && id.len() <= 128,
            "conversation_id must be nonempty and at most 128 bytes"
        );
    }
    anyhow::ensure!(!request.query.trim().is_empty(), "query is required");
    anyhow::ensure!(request.query.len() <= 16_384, "query exceeds 16 KiB");
    anyhow::ensure!(
        request.investigation.questions.len() <= 24,
        "at most 24 questions are supported"
    );
    anyhow::ensure!(
        request.investigation.known_context.len() <= 16_384,
        "known_context exceeds 16 KiB"
    );
    anyhow::ensure!(
        request.investigation.target_paths.len() <= 32,
        "at most 32 target paths are supported"
    );
    for question in &request.investigation.questions {
        anyhow::ensure!(
            !question.trim().is_empty() && question.len() <= 2_048,
            "questions must be nonempty and at most 2 KiB"
        );
    }

    // Validation accepts safe absolute inputs for callers that do not pass
    // through MCP. The MCP boundary also stores the normalized values before
    // it invokes this function.
    let mut normalized = request.clone();
    normalize_request_paths(&mut normalized)
        .context("target paths and focus must resolve inside the repository")?;
    Ok(())
}

fn canonical_repo_root(root: &Path) -> anyhow::Result<PathBuf> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("repository root does not exist: {}", root.display()))?;
    anyhow::ensure!(
        canonical.is_dir(),
        "repository root is not a directory: {}",
        root.display()
    );
    Ok(canonical)
}

fn normalize_repository_path(root: &Path, input: &str) -> anyhow::Result<String> {
    let path = Path::new(input);
    let is_dot = path.as_os_str().is_empty()
        || path
            .components()
            .all(|component| matches!(component, Component::CurDir));
    if is_dot {
        return Ok(".".into());
    }

    anyhow::ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "path traversal is not allowed: {input}"
    );

    // On Unix, Path does not recognize a Windows drive or rooted path. Reject
    // those spellings rather than accidentally treating them as repository
    // filenames. On Windows, the normal Component checks below handle them.
    let windows_absolute = looks_windows_absolute(input);
    let is_absolute = path.is_absolute() || windows_absolute;
    if is_absolute {
        anyhow::ensure!(
            !windows_absolute || path.is_absolute(),
            "absolute path is not valid on this host: {input}"
        );
        let canonical = path
            .canonicalize()
            .with_context(|| format!("absolute path does not exist: {input}"))?;
        anyhow::ensure!(
            canonical.starts_with(root),
            "path is outside the repository: {input}"
        );
        return repository_relative(root, &canonical);
    }

    anyhow::ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::Prefix(_))),
        "path has an unsupported prefix: {input}"
    );

    let candidate = root.join(path);
    let resolved = resolve_with_existing_parent(root, &candidate, input)?;
    repository_relative(root, &resolved)
}

fn resolve_with_existing_parent(
    root: &Path,
    candidate: &Path,
    input: &str,
) -> anyhow::Result<PathBuf> {
    let mut current = candidate.to_path_buf();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(_) => {
                let canonical = current
                    .canonicalize()
                    .with_context(|| format!("cannot resolve path: {input}"))?;
                anyhow::ensure!(
                    canonical.starts_with(root),
                    "path resolves outside the repository: {input}"
                );
                let mut resolved = canonical;
                for component in missing.iter().rev() {
                    resolved.push(component.as_os_str());
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = current
                    .file_name()
                    .with_context(|| format!("invalid repository path: {input}"))?;
                missing.push(name.to_os_string());
                current = current
                    .parent()
                    .with_context(|| format!("invalid repository path: {input}"))?
                    .to_path_buf();
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect path: {input}"));
            }
        }
    }
}

fn repository_relative(root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("path is outside the repository: {}", path.display()))?;
    if relative.as_os_str().is_empty() {
        return Ok(".".into());
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn looks_windows_absolute(input: &str) -> bool {
    let bytes = input.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || input.starts_with('\\')
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InvestigationStatus {
    Complete,
    #[default]
    Partial,
    NotFound,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub question: String,
    pub answer: String,
    pub citations: Vec<ValidatedCitation>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    High,
    Medium,
    Low,
    #[default]
    Unknown,
}

/// Scout-reported evidence assessment, not a calibrated probability or verifier verdict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InvestigationConfidence {
    pub level: ConfidenceLevel,
    pub basis: String,
}

/// A single, explicit request for a deeper follow-up pass.
///
/// This is intentionally a narrow output contract.  The scout must identify a
/// concrete evidence gap and a question that the follow-up can answer; a
/// confidence label, query wording, or broad complexity heuristic is not
/// enough to trigger another model turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningContinuation {
    pub effort: String,
    pub question: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InvestigationReport {
    pub intent: InvestigationIntent,
    pub status: InvestigationStatus,
    pub findings: Vec<Finding>,
    pub unresolved: Vec<String>,
    pub searched_scope: Vec<String>,
    pub limitations: Vec<String>,
    #[serde(default)]
    pub confidence: InvestigationConfidence,
    /// At most one targeted continuation may be requested by a report. The
    /// CLI wrapper validates the effort against native model capability before
    /// issuing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ReasoningContinuation>,
}

#[derive(Deserialize)]
struct ModelFinding {
    question: String,
    answer: String,
    citations: Vec<Citation>,
}

#[derive(Deserialize)]
struct ModelReport {
    answer: String,
    status: InvestigationStatus,
    findings: Vec<ModelFinding>,
    unresolved: Vec<String>,
    searched_scope: Vec<String>,
    limitations: Vec<String>,
    #[serde(default)]
    confidence: InvestigationConfidence,
    #[serde(default, alias = "reasoning_continuation")]
    continuation: Option<ReasoningContinuation>,
}

pub fn questions(request: &ScoutRequest) -> Vec<String> {
    if request.investigation.questions.is_empty() {
        vec![request.query.clone()]
    } else {
        request.investigation.questions.clone()
    }
}

pub fn investigation_prompt(request: &ScoutRequest) -> String {
    let context = json!({
        "objective": request.query,
        "intent": request.investigation.intent,
        "questions": questions(request),
        "known_context_unverified": request.investigation.known_context,
        "target_paths": request.investigation.target_paths,
        "focus": request.focus,
        "prior_findings_unverified": request.investigation.continuation_context,
        "supported_continuation_efforts": request.investigation.continuation_efforts,
    });
    format!(
        "{}\n\nInvestigation input:\n{}\n\nUse the query as the primary objective. Optional fields are hints; choose useful related reads and follow leads when they help answer the request. Return one JSON object matching this schema. Give useful explanation and code context in the answer and findings. Label findings in concise words of your choice, and state unresolved questions or material missing facts plainly. Attach direct source citations to factual findings. Use partial when questions remain unresolved; not_found means only that the stated search scope yielded no supported answer. Never invent citations. If supported_continuation_efforts lists a higher effort, you may request one continuation for a specific reasoning problem that remains after following useful leads. Name the unresolved relationship and why more reasoning may help. For example, two traced override paths still conflict. Missing ordinary reads, user choices, and transport-trimmed source need those missing inputs, not higher effort. Otherwise return continuation: null. In a continuation, revise the full original report, retaining supported findings and resolving or carrying forward each material gap. Preserve the original objective, requirements, and evidence scope.\n{}",
        request.investigation.intent.strategy(), context, investigation_output_schema()
    )
}

pub fn investigation_output_schema() -> Value {
    let citation = json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "path": {"type": "string", "description": "Repository-relative source path, for example 'crates/core/src/lib.rs'. Absolute paths are not returned. Only cite files inside the repository."}, "start_line": {"type": "integer", "minimum": 1},
            "end_line": {"type": "integer", "minimum": 1}, "reason": {"type": "string"}
        }, "required": ["path", "start_line", "end_line", "reason"]
    });
    json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "answer": {"type": "string", "description": "Direct conclusion. Put supporting explanation and code relationships in findings so the two fields complement each other."},
            "status": {"type": "string", "enum": ["complete", "partial", "not_found", "failed"]},
            "confidence": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "level": {"type": "string", "enum": ["high", "medium", "low", "unknown"], "description": "Your assessment of evidence supporting the answer, not a probability. High: deciding behavior directly traced with no material conflicting evidence. Medium: supported answer with an important inferred relationship. Low: tentative explanation. Unknown: insufficient evidence to assess."},
                    "basis": {"type": "string", "description": "Explain what evidence justifies the assessment and which claims remain inferred or untested. Distinguish reading a test from running it. List actionable gaps in unresolved, not as a generic request to recheck everything."}
                },
                "required": ["level", "basis"]
            },
            "findings": {"type": "array", "items": {
                "type": "object", "additionalProperties": false,
                "properties": {"question": {"type": "string", "description": "Concise label for the question or finding; use your own wording."}, "answer": {"type": "string", "description": "The supported explanation or code context for this finding."},
                    "citations": {"type": "array", "description": "Order source by usefulness to the parent's next step; implementation and relevant tests before peripheral background. This order controls source retention when the reply is oversized.", "items": citation}},
                "required": ["question", "answer", "citations"]
            }},
            "unresolved": {"type": "array", "items": {"type": "string", "description": "Material gap preventing the requested investigation from being answered. Distinguish missing task requirements from unresolved existing-code behavior. Supplied requirements, expected absence of a proposed feature, and optional extensions are not unresolved questions."}},
            "searched_scope": {"type": "array", "items": {"type": "string"}},
            "limitations": {"type": "array", "items": {"type": "string"}},
            "continuation": {"anyOf": [{"type": "object", "additionalProperties": false,
                "description": "Optional single targeted higher-effort continuation. Use only for a concrete evidence gap after the first useful pass; never request it from a confidence label or query keyword.",
                "properties": {
                    "effort": {"type": "string", "description": "A higher effort listed in supported_continuation_efforts; the caller validates native support."},
                    "question": {"type": "string", "description": "The narrow missing fact or relationship to resolve."},
                    "reason": {"type": "string", "description": "Why the current evidence cannot resolve that question."}
                },
                "required": ["effort", "question", "reason"]
            }, {"type": "null"}]}
        },
        "required": ["answer", "status", "confidence", "findings", "unresolved", "searched_scope", "limitations", "continuation"]
    })
}

/// Validate output integrity and citation locations. Status and unresolved
/// questions remain the scout's report; claim truth remains the caller's
/// responsibility.
pub fn assess_output(
    request: &ScoutRequest,
    raw: &str,
) -> (String, Vec<ValidatedCitation>, InvestigationReport) {
    let raw = raw
        .trim()
        .strip_prefix("```json")
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(raw)
        .trim();
    let mut report = InvestigationReport {
        intent: request.investigation.intent,
        ..Default::default()
    };
    let mut all_citations = Vec::new();
    let mut rejected_citation = false;
    let parsed = serde_json::from_str::<ModelReport>(raw);
    let summary = match parsed {
        Ok(model) => {
            report.status = model.status;
            report.unresolved = model.unresolved;
            report.searched_scope = model.searched_scope;
            report.limitations = model.limitations;
            report.confidence = model.confidence;
            report.continuation = model.continuation;
            for finding in model.findings {
                let mut citations = Vec::new();
                for citation in finding.citations {
                    if let Some(valid) = validate_citation(&request.root, &citation)
                        .filter(|valid| valid.end_line == citation.end_line)
                    {
                        if !all_citations.iter().any(|c: &ValidatedCitation| {
                            c.path == valid.path
                                && c.start_line == valid.start_line
                                && c.end_line == valid.end_line
                        }) {
                            all_citations.push(valid.clone());
                        }
                        citations.push(valid);
                    } else {
                        rejected_citation = true;
                        report.limitations.push(format!(
                            "Rejected citation {}:{}-{}",
                            citation.path, citation.start_line, citation.end_line
                        ));
                        report.status = InvestigationStatus::Partial;
                    }
                }
                if finding.answer.trim().is_empty() || citations.is_empty() {
                    report.unresolved.push(finding.question.clone());
                }
                report.findings.push(Finding {
                    question: finding.question,
                    answer: finding.answer,
                    citations,
                });
            }
            model.answer
        }
        Err(_) => {
            // Older/custom providers remain usable, but cannot certify coverage.
            #[derive(Deserialize)]
            struct Legacy {
                answer: String,
                citations: Vec<Citation>,
            }
            let (summary, citations) = match serde_json::from_str::<Legacy>(raw) {
                Ok(legacy) => (legacy.answer, legacy.citations),
                Err(_) => parse_citations(raw),
            };
            all_citations = crate::validate_citations(&request.root, &citations);
            report
                .limitations
                .push("Legacy or malformed output: question coverage was not established.".into());
            summary
        }
    };
    if report.status == InvestigationStatus::Complete
        && (!report.unresolved.is_empty() || all_citations.is_empty())
    {
        report.status = InvestigationStatus::Partial;
    }
    if report.status == InvestigationStatus::Complete
        && request.investigation.intent == InvestigationIntent::Inventory
        && report.searched_scope.is_empty()
    {
        report.status = InvestigationStatus::Partial;
        report
            .limitations
            .push("Inventory completeness requires an explicit searched scope.".into());
    }
    if report.status == InvestigationStatus::NotFound
        && (!all_citations.is_empty() || report.searched_scope.is_empty())
    {
        report.status = InvestigationStatus::Partial;
    }
    if rejected_citation || report.confidence.basis.trim().is_empty() {
        report.confidence = InvestigationConfidence::default();
    }
    (summary, all_citations, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_effort_is_optional_and_validated() {
        let root = tempfile::tempdir().unwrap();
        let mut request = request(root.path());
        validate_request(&request).unwrap();
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            request.investigation.reasoning_effort = Some(effort.into());
            validate_request(&request).unwrap();
        }
        request.investigation.reasoning_effort = Some("whatever".into());
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn confidence_requires_basis_and_survives_only_valid_citations() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn start() {}\n").unwrap();
        let request = request(root.path());
        let mut raw = json!({"answer":"start", "status":"complete", "findings":[{
            "question":"entry", "answer":"start is defined here", "citations":[
                {"path":"lib.rs", "start_line":1,"end_line":1,"reason":"entry"}]
        }], "unresolved":[], "searched_scope":["lib.rs"], "limitations":[],
        "confidence":{"level":"high","basis":"Read the definition. Runtime was not tested."}});
        let report = assess_output(&request, &raw.to_string()).2;
        assert_eq!(report.confidence.level, ConfidenceLevel::High);
        assert!(report.confidence.basis.contains("Runtime was not tested"));

        raw["confidence"]["basis"] = json!(" ");
        assert_eq!(
            assess_output(&request, &raw.to_string()).2.confidence.level,
            ConfidenceLevel::Unknown
        );
        raw["confidence"]["basis"] = json!("Read the definition.");
        raw["findings"][0]["citations"][0]["end_line"] = json!(10);
        let report = assess_output(&request, &raw.to_string()).2;
        assert_eq!(report.confidence.level, ConfidenceLevel::Unknown);
        assert_eq!(report.status, InvestigationStatus::Partial);
        raw.as_object_mut().unwrap().remove("confidence");
        assert_eq!(
            assess_output(&request, &raw.to_string()).2.confidence.level,
            ConfidenceLevel::Unknown
        );
    }

    fn request(root: &Path) -> ScoutRequest {
        ScoutRequest {
            query: "trace".into(),
            root: root.to_path_buf(),
            focus: None,
            timeout: None,
            max_turns: None,
            investigation: InvestigationSpec::default(),
        }
    }

    #[test]
    fn omitted_and_dot_focus_use_the_repository_root() {
        let root = tempfile::tempdir().unwrap();
        let mut omitted = request(root.path());
        normalize_request_paths(&mut omitted).unwrap();
        assert_eq!(omitted.focus, None);

        let mut explicit = request(root.path());
        explicit.focus = Some(PathBuf::from("."));
        normalize_request_paths(&mut explicit).unwrap();
        assert_eq!(explicit.focus, None);
    }

    #[test]
    fn absolute_paths_inside_root_become_relative() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "fn start() {}\n").unwrap();
        let mut request = request(root.path());
        request.focus = Some(root.path().join("src"));
        request.investigation.target_paths =
            vec![root.path().join("src/lib.rs").display().to_string()];

        normalize_request_paths(&mut request).unwrap();

        assert_eq!(request.focus, Some(PathBuf::from("src")));
        assert_eq!(request.investigation.target_paths, ["src/lib.rs"]);
        validate_request(&request).unwrap();
    }

    #[test]
    fn relative_nonexistent_target_remains_a_repository_hint() {
        let root = tempfile::tempdir().unwrap();
        let mut request = request(root.path());
        request.investigation.target_paths = vec!["src/new_module.rs".into()];

        normalize_request_paths(&mut request).unwrap();

        assert_eq!(request.investigation.target_paths, ["src/new_module.rs"]);
        validate_request(&request).unwrap();
    }

    #[test]
    fn traversal_and_outside_paths_are_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let outside = parent.path().join("secret.rs");
        std::fs::write(&outside, "secret\n").unwrap();

        let mut traversal = request(&root);
        traversal.focus = Some(PathBuf::from("src/../src"));
        assert!(normalize_request_paths(&mut traversal).is_err());

        let mut absolute_outside = request(&root);
        absolute_outside.investigation.target_paths = vec![outside.display().to_string()];
        assert!(normalize_request_paths(&mut absolute_outside).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_are_rejected_even_for_missing_targets() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let outside = parent.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();

        let mut focus = request(&root);
        focus.focus = Some(PathBuf::from("linked"));
        assert!(normalize_request_paths(&mut focus).is_err());

        let mut missing = request(&root);
        missing.investigation.target_paths = vec!["linked/new.rs".into()];
        assert!(normalize_request_paths(&mut missing).is_err());
    }

    #[test]
    fn explicit_unresolved_question_keeps_partial_status() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn start() {}\n").unwrap();
        let request = ScoutRequest {
            query: "trace".into(),
            root: root.path().into(),
            focus: None,
            timeout: None,
            max_turns: None,
            investigation: InvestigationSpec {
                questions: vec!["entry?".into(), "cleanup?".into()],
                ..Default::default()
            },
        };
        let raw = json!({"answer":"done", "status":"complete", "findings":[{
            "question":"entry?", "answer":"start", "citations":[{"path":"lib.rs", "start_line":1,"end_line":1,"reason":"entry"}]
        }], "unresolved":["cleanup?"], "searched_scope":["lib.rs"], "limitations":[]}).to_string();
        let (_, citations, report) = assess_output(&request, &raw);
        assert_eq!(citations.len(), 1);
        assert_eq!(report.status, InvestigationStatus::Partial);
        assert_eq!(report.unresolved, ["cleanup?"]);
    }

    #[test]
    fn negative_result_requires_explicit_scope() {
        let request = ScoutRequest {
            query: "missing".into(),
            root: ".".into(),
            focus: None,
            timeout: None,
            max_turns: None,
            investigation: InvestigationSpec::default(),
        };
        let raw = json!({"answer":"absent", "status":"not_found", "findings":[],
            "unresolved":[], "searched_scope":[], "limitations":[]})
        .to_string();
        assert_eq!(
            assess_output(&request, &raw).2.status,
            InvestigationStatus::Partial
        );
    }

    #[test]
    fn concise_finding_label_can_cover_multiple_questions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn start() {}\n").unwrap();
        let request = ScoutRequest {
            investigation: InvestigationSpec {
                questions: vec!["where is entry?".into(), "what does it do?".into()],
                ..Default::default()
            },
            ..request(root.path())
        };
        let raw = json!({"answer":"start is defined in lib.rs", "status":"complete", "findings":[{
            "question":"entry implementation", "answer":"The entry function is start.",
            "citations":[{"path":"lib.rs", "start_line":1,"end_line":1,"reason":"entry"}]
        }], "unresolved":[], "searched_scope":["lib.rs"], "limitations":[]})
        .to_string();

        let (_, _, report) = assess_output(&request, &raw);
        assert_eq!(report.status, InvestigationStatus::Complete);
        assert!(report.unresolved.is_empty());
    }

    #[test]
    fn invalid_citation_stays_partial_and_is_reported() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn start() {}\n").unwrap();
        let request = request(root.path());
        let raw = json!({"answer":"start", "status":"complete", "findings":[{
            "question":"entry", "answer":"The entry function is start.",
            "citations":[{"path":"lib.rs", "start_line":1,"end_line":2,"reason":"entry"}]
        }], "unresolved":[], "searched_scope":["lib.rs"], "limitations":[]})
        .to_string();

        let (_, citations, report) = assess_output(&request, &raw);
        assert!(citations.is_empty());
        assert_eq!(report.status, InvestigationStatus::Partial);
        assert!(report
            .limitations
            .iter()
            .any(|limitation| limitation.contains("Rejected citation lib.rs:1-2")));
        assert_eq!(report.unresolved, ["entry"]);
    }

    #[test]
    fn intent_does_not_reduce_the_configured_turn_ceiling() {
        for intent in [
            InvestigationIntent::Locate,
            InvestigationIntent::Explain,
            InvestigationIntent::ChangeImpact,
            InvestigationIntent::Diagnose,
            InvestigationIntent::Inventory,
        ] {
            assert_eq!(intent.turn_limit(9), 9);
        }
    }

    #[test]
    fn requirements_and_existing_code_assumptions_remain_distinct() {
        let root = tempfile::tempdir().unwrap();
        let mut request = request(root.path());
        request.query = "Find change points. Requirements: reject multi-file writes.".into();
        request.investigation.known_context = "I think writes use the shared loader.".into();
        let prompt = investigation_prompt(&request);
        assert!(prompt.contains(&request.query));
        assert!(prompt.contains("known_context_unverified"));
        assert!(prompt.contains(&request.investigation.known_context));
        let system = include_str!("../prompts/system.md");
        assert!(system.contains("not claims that the repository already implements it"));
        assert!(system.contains("Do not reopen decisions supplied in the request"));
        assert!(system.contains("Missing requirement:"));
    }

    #[test]
    fn explicit_continuation_is_preserved_as_a_targeted_gap() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn start() {}\n").unwrap();
        let request = request(root.path());
        let raw = json!({
            "answer": "The entry point is in lib.rs.",
            "status": "partial",
            "confidence": {"level": "medium", "basis": "The caller was not traced."},
            "findings": [{"question": "entry", "answer": "start is defined here.",
                "citations": [{"path": "lib.rs", "start_line": 1, "end_line": 1, "reason": "entry"}]}],
            "unresolved": ["Which override wins?"],
            "searched_scope": ["lib.rs"],
            "limitations": [],
            "continuation": {"effort": "max", "question": "Which override wins?", "reason": "Two traced override branches disagree at their merge boundary."}
        }).to_string();
        let report = assess_output(&request, &raw).2;
        assert_eq!(report.continuation.as_ref().unwrap().effort, "max");
        assert_eq!(
            report.continuation.as_ref().unwrap().question,
            "Which override wins?"
        );
    }

    #[test]
    fn completed_change_map_is_not_downgraded_for_an_unbuilt_feature() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "fn load_one() {}\n").unwrap();
        let request = request(root.path());
        let mut raw = json!({
            "answer": "Requirements specify later files win. Add a multi-file loader beside load_one.",
            "status": "complete",
            "confidence": {"level": "high", "basis": "Read the single-file loader; tests not executed."},
            "findings": [{"question": "change location", "answer": "load_one is the existing entry point.",
                "citations": [{"path": "lib.rs", "start_line": 1, "end_line": 1, "reason": "existing loader"}]}],
            "unresolved": [], "searched_scope": ["lib.rs"],
            "limitations": ["The proposed feature is not implemented. An optional multi-file writer is outside this request."]
        });
        assert_eq!(
            assess_output(&request, &raw.to_string()).2.status,
            InvestigationStatus::Complete
        );
        raw["unresolved"] =
            json!(["Existing behavior: generated caller precedence could not be inspected."]);
        assert_eq!(
            assess_output(&request, &raw.to_string()).2.status,
            InvestigationStatus::Partial
        );
        raw["unresolved"] =
            json!(["Missing requirement: which file may a multi-file write modify?"]);
        assert_eq!(
            assess_output(&request, &raw.to_string()).2.status,
            InvestigationStatus::Partial
        );
    }

    #[test]
    fn investigation_prompt_allows_concise_labels_and_query_only_input() {
        let root = tempfile::tempdir().unwrap();
        let prompt = investigation_prompt(&request(root.path()));
        assert!(prompt.contains("query as the primary objective"));
        assert!(prompt.contains("Label findings in concise words of your choice"));
        assert!(!prompt.contains("Copy question strings exactly"));
    }
}
