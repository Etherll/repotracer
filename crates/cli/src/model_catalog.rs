//! Model discovery for the settings picker.
//!
//! Native subscriptions are queried through their provider CLIs. Custom
//! OpenAI-compatible endpoints use the read-only `GET /models` contract and
//! never make a completion request during setup.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout, Command},
};

/// A model that a subscription CLI reports as selectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub provider: String,
    pub id: String,
    pub label: String,
}

/// Results from both native subscription CLIs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub models: Vec<ModelChoice>,
    pub warnings: Vec<String>,
    /// Capability evidence keyed by the provider:model pair. An absent entry
    /// means capability is unknown, not that every effort is supported.
    pub reasoning_efforts: BTreeMap<String, Vec<String>>,
}

/// Stable key used for capability evidence. Provider is part of the key so a
/// Codex and Claude model with the same identifier cannot inherit each other's
/// supported effort levels.
pub fn model_key(provider: &str, model: &str) -> String {
    format!("{}:{model}", provider.to_ascii_lowercase())
}

/// A model advertised by an OpenAI-compatible `/models` endpoint.
pub async fn discover_openai_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<(Vec<ModelChoice>, BTreeMap<String, Vec<String>>)> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(timeout())
        .build()
        .context("build model discovery client")?;
    let mut request = client.get(&endpoint);
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|_| anyhow!("custom model endpoint could not be reached"))?;
    let status = response.status();
    if !status.is_success() {
        // Do not include the response body: proxies occasionally echo request
        // headers and setup diagnostics must never become a secret sink.
        bail!("custom model endpoint returned HTTP {status}");
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|_| anyhow!("custom model endpoint returned invalid JSON"))?;
    let (models, efforts) = parse_openai_models(&payload)?;
    Ok((models, efforts))
}

/// Parse the OpenAI model-list envelope without retaining any provider secret.
/// Kept separate from transport so malformed/partial provider responses are
/// covered without a live endpoint.
pub type DiscoveredModels = (Vec<ModelChoice>, BTreeMap<String, Vec<String>>);

pub fn parse_openai_models(payload: &Value) -> Result<DiscoveredModels> {
    let entries = payload
        .get("data")
        .or_else(|| payload.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("custom model endpoint returned no model list"))?;
    let mut models = Vec::new();
    let mut efforts = BTreeMap::new();
    let mut seen = HashSet::new();
    for entry in entries.iter().take(MAX_MODELS) {
        let id = entry
            .get("id")
            .or_else(|| entry.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && !id.chars().any(char::is_control));
        let Some(id) = id else { continue };
        if !seen.insert(id.to_owned()) {
            continue;
        }
        let id = id.to_owned();
        let label = entry
            .get("name")
            .or_else(|| entry.get("display_name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Custom API — {id}"));
        if let Some(levels) = supported_reasoning_efforts(entry) {
            efforts.insert(model_key("openai-compatible", &id), levels);
        }
        models.push(ModelChoice {
            provider: "openai-compatible".into(),
            id,
            label,
        });
    }
    Ok((models, efforts))
}

/// Discover Codex capabilities for the exact executable selected by the
/// active profile. Unlike the settings catalog this does not invoke Claude's
/// picker, and it makes no model turn or provider API call.
pub fn discover_codex_capabilities(executable: &Path) -> BTreeMap<String, Vec<String>> {
    let executable = executable.to_path_buf();
    let thread = std::thread::Builder::new()
        .name("repotracer-codex-capabilities".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime
                .block_on(async { discover_codex(&executable).await.ok() })
                .map(|(_, efforts)| efforts)
        });
    thread
        .ok()
        .and_then(|thread| thread.join().ok())
        .flatten()
        .unwrap_or_default()
}

/// Discover native capability evidence for one configured provider/model pair.
///
/// The selected provider is the only provider queried.  Unknown providers,
/// unavailable capability metadata, and mismatched model identifiers all
/// return `None` so callers never escalate from an assumption.
pub fn discover_efforts(
    provider: &str,
    executable: Option<&str>,
    model: &str,
) -> Option<Vec<String>> {
    let provider = provider.to_ascii_lowercase();
    let executable = match provider.as_str() {
        "codex" | "codex-cli" => executable
            .map(PathBuf::from)
            .unwrap_or_else(codex_executable),
        "claude" | "claude-cli" => executable
            .map(PathBuf::from)
            .unwrap_or_else(claude_executable),
        _ => return None,
    };
    if model.is_empty() {
        return None;
    }
    if matches!(provider.as_str(), "codex" | "codex-cli") {
        let mut capabilities = discover_codex_capabilities(&executable);
        return capabilities
            .remove(&model_key("codex", model))
            .or_else(|| capabilities.remove(model));
    }

    let model = model.to_owned();
    let thread = std::thread::Builder::new()
        .name("repotracer-model-efforts".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(async move {
                match provider.as_str() {
                    "claude" | "claude-cli" => discover_claude_efforts(&executable, &model)
                        .await
                        .ok()
                        .flatten(),
                    _ => unreachable!("provider was validated before spawning discovery"),
                }
            })
        });
    thread.ok().and_then(|thread| thread.join().ok()).flatten()
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PAGES: usize = 16;
const MAX_MODELS: usize = 512;
const MAX_PROTOCOL_LINE_BYTES: usize = 1024 * 1024;
const MAX_CLAUDE_OUTPUT_BYTES: usize = 1024 * 1024;
const CLAUDE_API_ENVIRONMENT: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

// These are aliases accepted in native Claude model-picker output. They do
// not form a fallback catalog. A model is selectable only when the native CLI
// reports it for this account.
const DOCUMENTED_CLAUDE_ALIASES: &[&str] = &[
    "default",
    "best",
    "fable",
    "sonnet",
    "opus",
    "haiku",
    "sonnet[1m]",
    "opus[1m]",
    "fable[1m]",
    "opusplan",
];

/// Cancellable model discovery for the settings screen. Dropping the worker
/// cancels its native probes and joins the thread before the CLI exits.
pub struct Discovery {
    pub receiver: std::sync::mpsc::Receiver<Catalog>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Discovery {
    pub fn start() -> std::io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("repotracer-model-catalog".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = sender.send(Catalog {
                            warnings: vec![format!("Model discovery could not start: {error}")],
                            ..Catalog::default()
                        });
                        return;
                    }
                };
                runtime.block_on(async {
                    let providers = ["codex".to_owned(), "claude".to_owned()];
                    tokio::select! {
                        catalog = discover_async(&providers) => { let _ = sender.send(catalog); }
                        _ = stopped => {}
                    }
                    // Give Tokio's child reaper a turn after kill_on_drop.
                    tokio::task::yield_now().await;
                });
            })?;
        Ok(Self {
            receiver,
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn discover_async(providers: &[String]) -> Catalog {
    let codex = codex_executable();
    let claude = claude_executable();
    let wants_codex = providers.iter().any(|provider| provider == "codex");
    let wants_claude = providers.iter().any(|provider| provider == "claude");
    let mut catalog = Catalog::default();
    if wants_codex {
        let auth = native_auth_status("codex", &codex).await;
        if auth != AuthStatus::Authenticated && !explicit_native_route("codex") {
            catalog.warnings.push(auth_warning("Codex", auth));
        } else {
            match discover_codex(&codex).await {
                Ok((models, efforts)) => {
                    catalog.models.extend(models);
                    catalog.reasoning_efforts.extend(
                        efforts
                            .into_iter()
                            .map(|(id, levels)| (model_key("codex", &id), levels)),
                    );
                    if auth != AuthStatus::Authenticated {
                        catalog.warnings.push(
                            "Codex models: configured native route; authentication unverified"
                                .into(),
                        );
                    }
                }
                Err(error) => catalog
                    .warnings
                    .push(discovery_warning("Codex", &error, auth)),
            }
        }
    }
    if wants_claude {
        let auth = native_auth_status("claude", &claude).await;
        if auth != AuthStatus::Authenticated && !explicit_native_route("claude") {
            catalog.warnings.push(auth_warning("Claude Code", auth));
        } else {
            match discover_claude(&claude).await {
                Ok(models) => {
                    for model in &models {
                        if let Ok(Some(levels)) = discover_claude_efforts(&claude, &model.id).await
                        {
                            catalog
                                .reasoning_efforts
                                .insert(model_key("claude", &model.id), levels);
                        }
                    }
                    catalog.models.extend(models);
                    if auth != AuthStatus::Authenticated {
                        catalog.warnings.push(
                            "Claude Code models: configured native route; authentication unverified"
                                .into(),
                        );
                    }
                }
                Err(error) => catalog
                    .warnings
                    .push(discovery_warning("Claude Code", &error, auth)),
            }
        }
    }
    catalog
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthStatus {
    Authenticated,
    NotAuthenticated,
    Unavailable,
    MissingCli,
}

fn auth_warning(provider: &str, status: AuthStatus) -> String {
    let reason = match status {
        AuthStatus::MissingCli => "CLI not found",
        AuthStatus::NotAuthenticated => "native CLI is not logged in",
        _ => "native authentication could not be checked",
    };
    format!("{provider} models hidden: {reason}")
}

async fn native_auth_status(provider: &str, executable: &Path) -> AuthStatus {
    let args: &[&str] = match provider {
        "codex" => &["login", "status"],
        "claude" => &["auth", "status", "--json"],
        _ => return AuthStatus::Unavailable,
    };
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout(), command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return AuthStatus::MissingCli
        }
        _ => return AuthStatus::Unavailable,
    };
    if !output.status.success() {
        return AuthStatus::NotAuthenticated;
    }
    if provider == "claude" {
        return serde_json::from_slice::<Value>(&output.stdout)
            .ok()
            .and_then(|value| value.get("loggedIn").and_then(Value::as_bool))
            .map_or(AuthStatus::Unavailable, |logged_in| {
                if logged_in {
                    AuthStatus::Authenticated
                } else {
                    AuthStatus::NotAuthenticated
                }
            });
    }
    // `codex login status` intentionally has no JSON output. A successful
    // exit is the native CLI's authentication signal. Its text is discarded
    // so credentials can never reach the wizard output.
    AuthStatus::Authenticated
}

fn explicit_native_route(provider: &str) -> bool {
    let executable = match provider {
        "codex" => "REPOTRACER_CODEX_EXECUTABLE",
        "claude" => "REPOTRACER_CLAUDE_EXECUTABLE",
        _ => return false,
    };
    std::env::var_os(executable).is_some()
}

fn claude_api_configuration_reason() -> Option<&'static str> {
    CLAUDE_API_ENVIRONMENT
        .iter()
        .find(|name| std::env::var_os(name).is_some())
        .copied()
}

fn discovery_warning(provider: &str, error: &anyhow::Error, auth: AuthStatus) -> String {
    let reason = error.to_string();
    let reason = if reason.contains("No such file")
        || reason.contains("os error 2")
        || reason.contains("start `")
    {
        "CLI not found"
    } else if reason.contains("closed its output")
        || reason.contains("closed its output before")
        || reason.contains("initialize request failed")
    {
        "not signed in or native catalog unavailable"
    } else if auth == AuthStatus::NotAuthenticated {
        "native auth unavailable or configured provider rejected the catalog request"
    } else {
        "native catalog unavailable"
    };
    format!("{provider} models unavailable: {reason}")
}

fn timeout() -> Duration {
    std::env::var("REPOTRACER_MODEL_DISCOVERY_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT)
}

fn codex_executable() -> PathBuf {
    std::env::var_os("REPOTRACER_CODEX_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"))
}

fn claude_executable() -> PathBuf {
    std::env::var_os("REPOTRACER_CLAUDE_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("claude"))
}

fn claude_choice(id: String) -> ModelChoice {
    ModelChoice {
        provider: "claude".into(),
        label: format!("Claude Code — {id}"),
        id,
    }
}

fn codex_choice(id: String, display_name: Option<String>) -> ModelChoice {
    ModelChoice {
        provider: "codex".into(),
        label: display_name.unwrap_or_else(|| format!("Codex — {id}")),
        id,
    }
}

async fn discover_codex(
    executable: &PathBuf,
) -> Result<(Vec<ModelChoice>, BTreeMap<String, Vec<String>>)> {
    let mut command = Command::new(executable);
    command
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("start `{}`", executable.display()))?;
    let result = discover_codex_child(&mut child).await;
    finish_child(&mut child).await;
    result
}

async fn discover_codex_child(
    child: &mut Child,
) -> Result<(Vec<ModelChoice>, BTreeMap<String, Vec<String>>)> {
    let mut stdin = child
        .stdin
        .take()
        .context("Codex app-server stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex app-server stdout unavailable")?;
    let mut lines = BufReader::new(stdout);
    let deadline = Instant::now() + timeout();

    write_json(
        &mut stdin,
        &json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "repotracer_model_catalog",
                    "title": "RepoTracer model catalog",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
        deadline,
    )
    .await?;
    read_response(&mut lines, 1, deadline).await?;

    write_json(
        &mut stdin,
        &json!({"method": "initialized", "params": {}}),
        deadline,
    )
    .await?;

    let mut models = Vec::new();
    let mut reasoning_efforts = BTreeMap::new();
    let mut seen_ids = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut page = 0;
    loop {
        page += 1;
        if page > MAX_PAGES {
            bail!("model/list pagination exceeded {MAX_PAGES} pages");
        }
        let request_id = 100 + page as u64;
        let mut params = json!({ "limit": 100, "includeHidden": true });
        if let Some(cursor) = &cursor {
            params["cursor"] = Value::String(cursor.clone());
        }
        write_json(
            &mut stdin,
            &json!({"method": "model/list", "id": request_id, "params": params}),
            deadline,
        )
        .await?;
        let response = read_response(&mut lines, request_id, deadline).await?;
        let result = response
            .get("result")
            .ok_or_else(|| anyhow!("Codex model/list response had no result"))?;
        let entries = result
            .get("data")
            .or_else(|| result.get("models"))
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Codex model/list response had no model data"))?;
        for entry in entries {
            if !native_model_is_available(entry) {
                continue;
            }
            let id = entry
                .get("model")
                .and_then(Value::as_str)
                .or_else(|| entry.get("id").and_then(Value::as_str))
                .map(str::trim)
                .filter(|id| !id.is_empty());
            let Some(id) = id else { continue };
            if seen_ids.insert(id.to_owned()) {
                let display = entry
                    .get("displayName")
                    .or_else(|| entry.get("display_name"))
                    .or_else(|| entry.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                let id = id.to_owned();
                if let Some(efforts) = supported_reasoning_efforts(entry) {
                    reasoning_efforts.insert(id.clone(), efforts);
                }
                models.push(codex_choice(id, display));
                if models.len() >= MAX_MODELS {
                    return Ok((models, reasoning_efforts));
                }
            }
        }

        let next = result
            .get("nextCursor")
            .or_else(|| result.get("next_cursor"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|cursor| !cursor.is_empty());
        if next.is_none() || next == cursor {
            break;
        }
        cursor = next;
    }
    Ok((models, reasoning_efforts))
}

fn native_model_is_available(entry: &Value) -> bool {
    if entry.get("hidden").and_then(Value::as_bool) == Some(true)
        || entry.get("disabled").and_then(Value::as_bool) == Some(true)
        || entry.get("unavailable").and_then(Value::as_bool) == Some(true)
        || entry.get("isAvailable").and_then(Value::as_bool) == Some(false)
        || entry.get("available").and_then(Value::as_bool) == Some(false)
        || entry.get("enabled").and_then(Value::as_bool) == Some(false)
    {
        return false;
    }
    for key in ["status", "availability", "state"] {
        let Some(value) = entry.get(key).and_then(Value::as_str) else {
            continue;
        };
        if matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "unavailable" | "disabled" | "blocked" | "not_available"
        ) {
            return false;
        }
    }
    true
}

fn supported_reasoning_efforts(entry: &Value) -> Option<Vec<String>> {
    let candidate = entry
        .get("supportedReasoningEfforts")
        .or_else(|| entry.get("supported_reasoning_efforts"))
        .or_else(|| entry.get("reasoningEfforts"))
        .or_else(|| entry.get("reasoning_efforts"))
        .or_else(|| entry.get("reasoning").and_then(|v| v.get("efforts")))
        .and_then(Value::as_array)?;
    let mut efforts = Vec::new();
    for value in candidate {
        let value = value
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                value
                    .get("reasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .or_else(|| {
                value
                    .get("reasoning_effort")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let Some(value) = value else { continue };
        let effort = value.trim().to_ascii_lowercase();
        if matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
            && !efforts.contains(&effort)
        {
            efforts.push(effort);
        }
    }
    (!efforts.is_empty()).then_some(efforts)
}

async fn write_json(
    stdin: &mut tokio::process::ChildStdin,
    value: &Value,
    deadline: Instant,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("timed out writing app-server request")
    }
    tokio::time::timeout(remaining, async {
        stdin.write_all(&bytes).await?;
        stdin.flush().await
    })
    .await
    .map_err(|_| anyhow!("timed out writing app-server request"))??;
    Ok(())
}

async fn read_response(
    lines: &mut tokio::io::BufReader<ChildStdout>,
    id: u64,
    deadline: Instant,
) -> Result<Value> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for app-server response")
        }
        let mut line = Vec::new();
        let bytes = tokio::time::timeout(remaining, read_limited_line(lines, &mut line))
            .await
            .map_err(|_| anyhow!("timed out waiting for app-server response"))??;
        if bytes == 0 {
            bail!("Codex app-server closed its output")
        }
        if line.len() > MAX_PROTOCOL_LINE_BYTES {
            bail!("Codex app-server response exceeded {MAX_PROTOCOL_LINE_BYTES} bytes")
        }
        let value: Value = serde_json::from_slice(line.strip_suffix(b"\n").unwrap_or(&line))
            .context("Codex app-server returned malformed JSON")?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            bail!("Codex app-server request failed: {message}")
        }
        return Ok(value);
    }
}

async fn read_limited_line(
    lines: &mut tokio::io::BufReader<ChildStdout>,
    line: &mut Vec<u8>,
) -> std::io::Result<usize> {
    line.clear();
    loop {
        let chunk = lines.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(line.len());
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |position| position + 1);
        if line.len().saturating_add(take) > MAX_PROTOCOL_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "app-server response line exceeded configured limit",
            ));
        }
        line.extend_from_slice(&chunk[..take]);
        lines.consume(take);
        if newline.is_some() {
            return Ok(line.len());
        }
    }
}

async fn discover_claude_efforts(executable: &Path, model: &str) -> Result<Option<Vec<String>>> {
    if claude_api_configuration_reason().is_some() {
        return Ok(None);
    }
    let mut command = Command::new(executable);
    command
        .args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--safe-mode",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--tools",
            "",
            "--permission-mode",
            "dontAsk",
            "--model",
            model,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Do not let a direct API credential silently change the subscription
    // discovery path. Native Claude Code still owns OAuth/keychain login.
    for name in CLAUDE_API_ENVIRONMENT {
        command.env_remove(name);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start `{}`", executable.display()))?;
    let result = discover_claude_effort_child(&mut child, model).await;
    finish_child(&mut child).await;
    result
}

async fn discover_claude_effort_child(
    child: &mut Child,
    model: &str,
) -> Result<Option<Vec<String>>> {
    let mut stdin = child
        .stdin
        .take()
        .context("Claude Code stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Claude Code stdout unavailable")?;
    let mut lines = BufReader::new(stdout);
    let deadline = Instant::now() + timeout();
    write_json(
        &mut stdin,
        &json!({
            "type": "control_request",
            "request_id": "probe-init",
            "request": { "subtype": "initialize" },
        }),
        deadline,
    )
    .await?;

    let response = read_claude_control_response(&mut lines, "probe-init", deadline).await?;
    Ok(parse_claude_initialize_efforts(&response, model))
}

async fn read_claude_control_response(
    lines: &mut tokio::io::BufReader<ChildStdout>,
    request_id: &str,
    deadline: Instant,
) -> Result<Value> {
    let mut total_bytes = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for Claude Code initialize response")
        }
        let mut line = Vec::new();
        let bytes = tokio::time::timeout(remaining, read_limited_line(lines, &mut line))
            .await
            .map_err(|_| anyhow!("timed out waiting for Claude Code initialize response"))??;
        if bytes == 0 {
            bail!("Claude Code closed its output before initialize response")
        }
        total_bytes = total_bytes.saturating_add(bytes);
        if total_bytes > MAX_CLAUDE_OUTPUT_BYTES {
            bail!("Claude Code initialize output exceeded {MAX_CLAUDE_OUTPUT_BYTES} bytes")
        }
        let value: Value = serde_json::from_slice(line.strip_suffix(b"\n").unwrap_or(&line))
            .context("Claude Code returned malformed stream-json")?;
        if value.get("type").and_then(Value::as_str) != Some("control_response") {
            continue;
        }
        let response = value
            .get("response")
            .context("Claude Code control response had no response envelope")?;
        if response.get("request_id").and_then(Value::as_str) != Some(request_id) {
            continue;
        }
        if response.get("subtype").and_then(Value::as_str) == Some("error") {
            let message = response
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            bail!("Claude Code initialize request failed: {message}")
        }
        if response.get("subtype").and_then(Value::as_str) != Some("success") {
            bail!("Claude Code initialize response was not successful")
        }
        return Ok(value);
    }
}

fn parse_claude_initialize_efforts(value: &Value, model: &str) -> Option<Vec<String>> {
    let envelope = value.get("response")?;
    if envelope.get("subtype").and_then(Value::as_str) != Some("success") {
        return None;
    }
    let models = envelope
        .get("response")
        .and_then(|response| response.get("models"))
        .and_then(Value::as_array)?;
    let entry = models
        .iter()
        .find(|entry| entry.get("value").and_then(Value::as_str) == Some(model))
        .or_else(|| {
            models
                .iter()
                .find(|entry| entry.get("resolvedModel").and_then(Value::as_str) == Some(model))
        })?;
    parse_claude_effort_levels(entry)
}

fn parse_claude_effort_levels(entry: &Value) -> Option<Vec<String>> {
    if entry.get("supportsEffort").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let levels = entry.get("supportedEffortLevels")?.as_array()?;
    let mut efforts = Vec::new();
    for level in levels {
        let Some(level) = level.as_str().map(str::trim) else {
            continue;
        };
        if matches!(level, "low" | "medium" | "high" | "xhigh" | "max")
            && !efforts.iter().any(|effort| effort == level)
        {
            efforts.push(level.to_owned());
        }
    }
    (!efforts.is_empty()).then_some(efforts)
}

async fn discover_claude(executable: &PathBuf) -> Result<Vec<ModelChoice>> {
    if let Some(reason) = claude_api_configuration_reason() {
        bail!(reason);
    }
    let mut command = Command::new(executable);
    command
        .args([
            "--print",
            "--output-format",
            "json",
            "--no-session-persistence",
            "--safe-mode",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--tools",
            "",
            "--permission-mode",
            "dontAsk",
            "-p",
            "/model",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Do not let a direct API credential silently change the subscription
    // discovery path. Native Claude Code still owns OAuth/keychain login.
    for name in CLAUDE_API_ENVIRONMENT {
        command.env_remove(name);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start `{}`", executable.display()))?;
    let stdout = child
        .stdout
        .take()
        .context("Claude Code stdout unavailable")?;
    let stdout = BufReader::new(stdout);
    let mut output = Vec::new();
    let deadline = Instant::now() + timeout();
    let remaining = deadline.saturating_duration_since(Instant::now());
    let read_result = tokio::time::timeout(
        remaining,
        stdout
            .take((MAX_CLAUDE_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut output),
    )
    .await;
    match read_result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            kill_and_reap(&mut child).await;
            return Err(error).context("read Claude Code model picker output");
        }
        Err(_) => {
            kill_and_reap(&mut child).await;
            bail!("timed out waiting for Claude Code model picker")
        }
    }
    if output.len() > MAX_CLAUDE_OUTPUT_BYTES {
        kill_and_reap(&mut child).await;
        bail!("Claude Code model picker output exceeded {MAX_CLAUDE_OUTPUT_BYTES} bytes")
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let status = match tokio::time::timeout(remaining, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            kill_and_reap(&mut child).await;
            return Err(error).context("wait for Claude Code model picker");
        }
        Err(_) => {
            kill_and_reap(&mut child).await;
            bail!("timed out waiting for Claude Code model picker")
        }
    };
    if !status.success() {
        bail!("Claude Code model picker exited unsuccessfully")
    }
    let text = String::from_utf8_lossy(&output);
    let models = parse_claude_output(&text);
    if models.is_empty() {
        bail!("Claude Code model picker returned no selectable models")
    }
    Ok(models)
}

fn parse_claude_output(text: &str) -> Vec<ModelChoice> {
    let mut available = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("result") {
            continue;
        }
        let Some(result) = value.get("result").and_then(Value::as_str) else {
            continue;
        };
        let lower = result.to_ascii_lowercase();
        let Some(start) = lower.find("available:") else {
            continue;
        };
        available = Some(result[start + "available:".len()..].to_owned());
        break;
    }

    let Some(available) = available else {
        return Vec::new();
    };
    let mut choices = Vec::new();
    let mut seen = HashSet::new();
    for token in available.split_whitespace() {
        let token = token.trim_matches(|ch: char| {
            matches!(ch, '`' | '"' | '\'' | ',' | '.' | ':' | ';' | '(' | ')')
        });
        let alias = DOCUMENTED_CLAUDE_ALIASES
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(token))
            .copied();
        let id = alias.map(str::to_owned).or_else(|| {
            token
                .strip_prefix("claude-")
                .filter(|suffix| !suffix.is_empty())
                .map(|_| token.to_owned())
        });
        let Some(id) = id else { continue };
        if seen.insert(id.clone()) {
            choices.push(claude_choice(id));
        }
    }
    choices
}

async fn finish_child(child: &mut Child) {
    // Closing stdin lets a well-behaved app-server exit cleanly.  If a fake or
    // old server keeps running, kill_on_drop plus this explicit kill bounds the
    // cleanup and prevents settings from leaking a child process.
    let _ = child.stdin.take();
    if tokio::time::timeout(Duration::from_millis(200), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = tokio::time::timeout(Duration::from_millis(200), child.wait()).await;
    }
}

async fn kill_and_reap(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_catalog_filters_hidden_entries_and_uses_display_name() {
        let response = json!({
            "result": {"data": [
                {"id":"ui-visible","model":"gpt-visible","displayName":"Visible","hidden":false},
                {"model":"gpt-hidden","hidden":true},
                {"model":"gpt-old"}
            ]}
        });
        let mut actual = Vec::new();
        let entries = response["result"]["data"].as_array().unwrap();
        for entry in entries {
            if entry.get("hidden").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| entry.get("model").and_then(Value::as_str))
                .unwrap();
            actual.push(codex_choice(
                id.into(),
                entry
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ));
        }
        assert_eq!(actual[0].label, "Visible");
        assert_eq!(actual[1].id, "gpt-old");
        assert_eq!(actual.len(), 2);
    }

    #[test]
    fn codex_capability_evidence_reads_native_effort_objects() {
        let entry = json!({
            "model": "gpt-5.6-luna",
            "supportedReasoningEfforts": [
                {"reasoningEffort": "medium", "description": "balanced"},
                {"reasoningEffort": "max", "description": "quality first"},
                {"reasoningEffort": "not-a-level", "description": "ignore"}
            ]
        });
        assert_eq!(
            supported_reasoning_efforts(&entry).unwrap(),
            vec!["medium".to_string(), "max".to_string()]
        );
    }

    #[test]
    fn claude_picker_parses_aliases_and_full_ids() {
        let text = r#"{"type":"result","result":"Current model: Sonnet 5 (default)\nAvailable: sonnet, opus, haiku, claude-sonnet-5-20250101"}"#;
        let models = parse_claude_output(text);
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["sonnet", "opus", "haiku", "claude-sonnet-5-20250101"]);
    }

    #[test]
    fn native_catalog_hides_explicitly_unavailable_models() {
        assert!(native_model_is_available(&json!({"model": "gpt-ok"})));
        assert!(!native_model_is_available(
            &json!({"model": "gpt-hidden", "hidden": true})
        ));
        assert!(!native_model_is_available(
            &json!({"model": "gpt-disabled", "available": false})
        ));
        assert!(!native_model_is_available(
            &json!({"model": "gpt-unavailable", "status": "unavailable"})
        ));
    }

    #[test]
    fn openai_models_parser_keeps_arbitrary_ids_and_provider_scoped_efforts() {
        let (models, efforts) = parse_openai_models(&json!({
            "data": [
                {"id": "vendor/reasoner.v9", "name": "Private Reasoner", "supportedReasoningEfforts": ["medium", "max"]},
                {"id": "vendor/reasoner.v9"},
                {"id": "bad\nmodel"}
            ]
        }))
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "vendor/reasoner.v9");
        assert_eq!(models[0].label, "Private Reasoner");
        assert_eq!(
            efforts.get(&model_key("openai-compatible", "vendor/reasoner.v9")),
            Some(&vec!["medium".into(), "max".into()])
        );
    }

    #[test]
    fn claude_parser_ignores_current_model_and_other_result_strings() {
        let text = r#"{"type":"result","result":"Current model: claude-opus-5\nAvailable: sonnet, haiku"}"#;
        let models = parse_claude_output(text);
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["sonnet", "haiku"]);
    }

    #[test]
    fn claude_effort_parser_matches_exact_alias_or_resolved_id() {
        let response = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "probe-init",
                "response": {
                    "models": [
                        {
                            "value": "sonnet",
                            "resolvedModel": "claude-sonnet-4-5-20250929",
                            "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]
                        }
                    ]
                }
            }
        });
        assert_eq!(
            parse_claude_initialize_efforts(&response, "sonnet"),
            Some(vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
                "max".to_string()
            ])
        );
        assert_eq!(
            parse_claude_initialize_efforts(&response, "claude-sonnet-4-5-20250929"),
            Some(vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "xhigh".to_string(),
                "max".to_string()
            ])
        );
        assert_eq!(
            parse_claude_initialize_efforts(&response, "claude-sonnet"),
            None
        );
        assert_eq!(parse_claude_initialize_efforts(&response, "Sonnet"), None);
    }

    #[test]
    fn claude_effort_parser_rejects_absent_invalid_or_disabled_capabilities() {
        let response = |entry: Value| {
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "probe-init",
                    "response": {"models": [entry]}
                }
            })
        };
        assert_eq!(
            parse_claude_initialize_efforts(&response(json!({"value": "haiku"})), "haiku"),
            None
        );
        assert_eq!(
            parse_claude_initialize_efforts(
                &response(json!({
                    "value": "haiku",
                    "supportedEffortLevels": ["", "invalid", 42]
                })),
                "haiku"
            ),
            None
        );
        assert_eq!(
            parse_claude_initialize_efforts(
                &response(json!({
                    "value": "haiku",
                    "supportsEffort": false,
                    "supportedEffortLevels": ["high"]
                })),
                "haiku"
            ),
            None
        );
        let mut not_success = response(json!({
            "value": "haiku",
            "supportedEffortLevels": ["high"]
        }));
        not_success["response"]["subtype"] = Value::String("error".into());
        assert_eq!(parse_claude_initialize_efforts(&not_success, "haiku"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auth_detection_distinguishes_missing_signed_out_and_unknown() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("native-fake");
        assert_eq!(
            native_auth_status("claude", &executable).await,
            AuthStatus::MissingCli
        );
        for (body, expected) in [
            (
                "printf '%s\\n' '{\"loggedIn\":true}'",
                AuthStatus::Authenticated,
            ),
            (
                "printf '%s\\n' '{\"loggedIn\":false}'",
                AuthStatus::NotAuthenticated,
            ),
            ("printf '%s\\n' 'invalid'", AuthStatus::Unavailable),
        ] {
            fs::write(
                &executable,
                format!("#!/bin/sh\n[ \"$*\" = \"auth status --json\" ] || exit 9\n{body}\n"),
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(native_auth_status("claude", &executable).await, expected);
        }
        assert!(auth_warning("Claude Code", AuthStatus::MissingCli).contains("CLI not found"));
        assert!(auth_warning("Codex", AuthStatus::Unavailable).contains("could not be checked"));
    }

    #[cfg(unix)]
    #[test]
    fn fake_codex_app_server_stream_is_paginated_and_cleaned_up() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("codex-fake");
        fs::write(
            &executable,
            r##"#!/bin/sh
read line
printf '%s\n' '{"id":1,"result":{}}'
read line
read line
printf '%s\n' '{"id":101,"result":{"data":[{"id":"gpt-one","displayName":"One","hidden":false},{"id":"gpt-hidden","hidden":true}],"nextCursor":"page-2"}}'
read line
printf '%s\n' '{"id":102,"result":{"data":[{"model":"gpt-two"}],"nextCursor":null}}'
"##,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (models, efforts) = runtime
            .block_on(discover_codex(&executable))
            .expect("fake app-server protocol should succeed");
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["gpt-one", "gpt-two"]
        );
        assert_eq!(models[0].label, "One");
        assert!(efforts.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fake_claude_cli_picker_is_catalog_only() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("claude-fake");
        fs::write(
            &executable,
            r##"#!/bin/sh
printf '%s\n' '{"type":"result","result":"Current model: Sonnet 5 (default)\nAvailable: sonnet, opus, claude-sonnet-5-20250101"}'
"##,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let models = runtime
            .block_on(discover_claude(&executable))
            .expect("fake Claude picker should succeed");
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["sonnet", "opus", "claude-sonnet-5-20250101"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn fake_claude_stream_json_probe_sends_initialize_without_user_message() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("claude-fake-stream");
        fs::write(
            &executable,
            r##"#!/bin/sh
case " $* " in
  *" --model sonnet "*) ;;
  *) exit 2 ;;
esac
IFS= read -r request || exit 3
case "$request" in
  *'"type":"control_request"'*) ;;
  *) exit 4 ;;
esac
case "$request" in
  *'"subtype":"initialize"'*) ;;
  *) exit 5 ;;
esac
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"probe-init","response":{"models":[{"value":"sonnet","resolvedModel":"claude-sonnet-4-5-20250929","supportedEffortLevels":["low","medium","high"]}]}}}'
"##,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let efforts = discover_efforts("claude", Some(executable.to_str().unwrap()), "sonnet");
        assert_eq!(
            efforts,
            Some(vec!["low".into(), "medium".into(), "high".into()])
        );
    }
}
