# RepoTracer

RepoTracer adds a read-only repository scout to Codex and Claude Code. The
`repo_scout` MCP tool investigates a repository with native read tools and
returns an explanation with source excerpts and file citations. The parent
agent decides what to do with that context and remains responsible for edits.

<p align="center">
  <a href="https://www.npmjs.com/package/repotracer"><img src="https://img.shields.io/npm/v/repotracer?color=0E9488&label=npm" alt="npm version"></a>
  <a href="https://github.com/repotracer/repotracer/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://modelcontextprotocol.io"><img src="https://img.shields.io/badge/MCP-compatible-purple" alt="MCP compatible"></a>
</p>

## Install

Install from npm:

```bash
npx repotracer@latest setup
```

In an interactive terminal, setup opens a two-step wizard:

1. Choose Codex, Claude Code, or both.
2. Keep the recommended models and Save, or select a scout to search other models.

The v2 defaults are Codex parent -> Codex scout with `gpt-5.6-luna`, and
Claude Code parent -> Claude scout with `sonnet`. These are configuration
defaults, not quality or cost guarantees. RepoTracer uses the login owned by
the selected native CLI. It does not ask for a second subscription key.

For a scripted install, choose the parent integrations explicitly:

```bash
npx repotracer@latest setup --agents codex
npx repotracer@latest setup --agents claude
npx repotracer@latest setup --agents both
npx repotracer@latest setup --agents both --dry-run
```

The Codex parent requires the `codex` CLI. The Claude Code parent requires the
`claude` CLI. Setup checks that the required executable is available, but
authentication happens when a scout runs. Restart the parent agent after
setup or after changing settings.

Node.js 18 or newer is required for the npm launcher. Published native
packages target macOS arm64 and x64, Linux arm64 and x64, and Windows x64.
The v2 release candidate has passed GitHub CI on Linux, macOS, and Windows,
including the native Codex app-server checks.

## Choose or change models

Run the same terminal wizard later with:

```bash
repotracer settings
```

`repotracer reconfigure` is an alias. The wizard keeps already configured
parents selected. Unchecking an existing parent leaves it unchanged; it does
not uninstall it. Nothing is written until Save. Esc goes back; Ctrl-C cancels
from anywhere.

Setup recommends Luna for Codex and Sonnet for Claude Code when available.
Settings keeps saved choices. Mappings stay visible at the top; select one
with Enter, or press A for advanced selection, to search both providers.
Tab moves between mappings and the bottom buttons. Ctrl-S saves. There is
no separate review screen.

The Custom API form accepts a base URL, model ID, and optional API key. Keys
require HTTPS; unauthenticated local endpoints can use HTTP. Use the explicit
flags below for native model IDs. A saved model that the current CLI does not
report remains selectable with an
"unverified" label. Missing or signed-out native CLIs have their
models hidden with a short reason. Explicit native wrappers can supply their
own route; if login status cannot verify that route, the catalog is labelled
unverified. Model discovery does not start an investigation.

For automation, use explicit flags instead of the TUI:

```bash
repotracer settings --agents both \
  --codex-scout codex --codex-model gpt-5.6-luna \
  --claude-scout claude --claude-model sonnet
```

Use `--dry-run` to preview settings without writing them. Each parent gets an
independent profile, so changing one mapping does not silently change the
other.

## How the scout works

The MCP server exposes `repo_scout` over stdio. It gives the native scout a
question and optional repository hints. Claude reads with Read, Glob, and
Grep. Codex uses its native read-only shell and the optional Symbols lookup.
Edits, network tools, and external MCP tools are disabled. The parent agent
receives a structured result and a readable text
fallback.

The query does not include the parent conversation automatically. Include the
requirements and compatibility constraints that affect the investigation:

```json
{
  "query": "Trace configuration loading and identify the callers that depend on precedence.",
  "repository": "/work/project",
  "focus": "src/config",
  "investigation": {
    "intent": "change_impact",
    "reasoning_effort": "high",
    "target_paths": ["src/config", "tests"]
  }
}
```

Supported `intent` values are `locate`, `explain`, `change_impact`,
`diagnose`, and `inventory`. `reasoning_effort` accepts `low`, `medium`,
`high`, `xhigh`, or `max` when the selected provider and model support it.
Omit it to use the configured effort.

An investigation result includes a model-authored report, source blocks when
available, and citations. RepoTracer validates that cited paths and line
ranges resolve inside the selected repository. That validation checks
locations, not whether the scout's conclusions are true. Treat the report as
evidence to review, not as a proof of correctness.

For a related follow-up, pass the returned `conversation.id` as
`investigation.conversation_id`. The handle is tied to its repository.
`conversation.status` reports whether the native attempt resumed, started
fresh, or could not be determined. Omit the handle for an independent
question.

## Sessions and timeouts

Warm provider processes are enabled by default. A warm process may be retained
for related work and retired after `session.idle_secs` of inactivity, which is
300 seconds by default. This is an idle-retention limit, not a total request
or investigation timeout.

The main timeout settings are opt-in:

- `model.timeout_ms` is a native stream-inactivity limit. Incoming stream
  activity resets it. `0` disables it.
- `explorer.timeout_seconds` is a whole-investigation limit for the generic
  OpenAI-compatible engine only. Native Codex and Claude scouts do not use it.
- `explorer.tool_timeout_seconds` limits individual tools in that generic
  engine and defaults to 10 seconds.

The parent caller may also impose its own MCP request allowance. A caller
allowance does not change the scout's turn budget. `explorer.max_turns = 0`
leaves investigation depth to the native scout; a positive value is an
explicit ceiling for backends that support it.

## CLI and MCP commands

```bash
repotracer "where is authentication handled?"
repotracer scout "trace token refresh" --intent diagnose
repotracer symbols "Config" --mode references
repotracer serve
repotracer doctor
repotracer status
repotracer config --init
repotracer update
repotracer uninstall --yes
```

The first form is shorthand for `scout`. `serve` runs the MCP server over
stdio. `symbols` performs a local syntax lookup without a model call. Add
`--json` to `scout`, `doctor`, or `status` for machine-readable output.

The MCP tool requires only `query`:

```json
{
  "query": "Find the code that loads the application configuration."
}
```

`repository`, `focus`, and `investigation` are optional. Absolute repository
paths must identify the selected checkout. Parent-directory traversal and
symlink escapes are rejected. RepoTracer's boundary is an application-level
read-only check, not an operating-system filesystem sandbox.

## Configuration and updates

`repotracer config --init` writes the default configuration. The installed npm
launcher stores its configuration under the user's RepoTracer directory and
parent-specific profiles beside it. `REPOTRACER_CONFIG` selects another config
file. `REPOTRACER_API_KEY` is read only for an explicitly configured
OpenAI-compatible backend.

Automatic binary updates are enabled by default for the npm-installed copy.
The server verifies the published checksum before replacing that copy. Disable
updates with:

```toml
[updates]
automatic = false
```

or set `REPOTRACER_NO_UPDATE=1`. `repotracer update` performs an update on
request. Source builds and `cargo install` copies are not replaced by the
automatic updater.

## Limitations and verification

RepoTracer does not promise lower cost, faster completion, or a particular
answer quality. Provider pricing, model behavior, account limits, repository
size, and parent-agent decisions all affect the result.

The v2 release candidate has passed GitHub CI on Linux, macOS, and Windows.
The native Codex and Claude Code CLIs remain required
for their respective subscription backends, and their supported flags and
model catalog can vary by installed version.

See [the security notes](SECURITY.md), [the architecture reference](docs/ARCHITECTURE.md),
and [the npm package guide](packages/npm/README.md).

## Develop

Building from source requires Rust 1.90 or newer.

```bash
cargo test --workspace
cargo run -p repotracer -- doctor
cargo run -p repotracer -- scout "where is configuration loaded?" --mock
```

RepoTracer is MIT licensed. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
