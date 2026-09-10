# repotracer

RepoTracer is an MCP server for read-only repository investigations in Codex
and Claude Code. Its `repo_scout` tool returns a model-authored report with
source excerpts and validated file citations. Citation validation checks that
locations exist inside the selected repository; it does not prove the report's
conclusions.

## Setup

```bash
npx repotracer@latest setup
```

In an interactive terminal, the v2 setup wizard lets you choose Codex, Claude
Code, or both, then choose a scout model. Defaults are:

- Codex parent -> Codex scout: `gpt-5.6-luna`
- Claude Code parent -> Claude scout: `sonnet`

These are defaults, not cost or quality promises. RepoTracer uses each native
CLI's existing login. It does not require a second subscription key.

For scripted setup:

```bash
npx repotracer@latest setup --agents codex
npx repotracer@latest setup --agents claude
npx repotracer@latest setup --agents both --dry-run
```

The Codex integration requires the `codex` executable. The Claude Code
integration requires `claude`. Setup checks that the executable is available;
the native CLI handles authentication when a scout runs. Restart the parent
agent after setup.

## Settings

```bash
npx repotracer@latest settings
```

The TUI uses two steps. First choose the parent integrations. Then accept
recommended models or keep saved choices. Open `Advanced` to search and
review a different provider/model for each parent. The
Custom API form accepts a base URL, model ID, and optional API key. Keys
require HTTPS; unauthenticated local endpoints can use HTTP. Use the flags
below for native model IDs. Esc and Ctrl-C cancel, and nothing is written
before the final save.

For automation, use explicit settings flags:

```bash
npx repotracer@latest settings --agents both \
  --codex-scout codex --codex-model gpt-5.6-luna \
  --claude-scout claude --claude-model sonnet
```

Use `--dry-run` to preview changes. Parent profiles are independent.

## Commands

```bash
npx repotracer@latest "where is authentication handled?"
npx repotracer@latest scout "trace token refresh" --intent diagnose
npx repotracer@latest symbols "Config" --mode references
npx repotracer@latest serve
npx repotracer@latest doctor
npx repotracer@latest status
npx repotracer@latest update
npx repotracer@latest uninstall --yes
```

`serve` runs the MCP server over stdio. `symbols` performs a local syntax
lookup without a model call. Add `--json` to `scout`, `doctor`, or `status`.

The MCP tool requires `query`; `repository`, `focus`, and `investigation` are
optional. Related follow-ups can pass the returned
`conversation.id` as `investigation.conversation_id`. Claude uses Read, Grep,
and Glob; Codex uses a read-only shell and optional Symbols lookup. Neither
scout receives editing tools or external MCP servers.

## Timeouts and support

Warm provider processes may be retained for related work. `session.idle_secs`
retires an inactive warm process; it is not a total request timeout. The
optional `model.timeout_ms` limit measures stream inactivity, while
`explorer.timeout_seconds` limits a whole investigation only in the generic
OpenAI-compatible engine, not the native scouts. Both default to zero.

Node.js 18 or newer is required for the launcher. Published native packages
target macOS arm64 and x64, Linux arm64 and x64, and Windows x64. The v2
release candidate has passed GitHub CI on Linux, macOS, and Windows.

RepoTracer does not promise lower cost, faster completion, or a particular
answer quality. Provider and parent-agent behavior affect each result.

MIT licensed. See the [repository documentation](https://github.com/repotracer/repotracer).
