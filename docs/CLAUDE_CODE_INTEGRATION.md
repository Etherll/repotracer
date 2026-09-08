# Claude Code and Codex integration

RepoTracer v2 can install an MCP server for Codex, Claude Code, or both. Each
parent has its own scout profile. A parent model does not silently change the
scout mapping during a session.

## Requirements

Install and sign in to the native parent CLI you want to use:

- Codex integration: the `codex` executable.
- Claude Code integration: the `claude` executable with native MCP
  registration support.

RepoTracer keeps provider authentication with the native CLI. It does not
extract credentials or implement a second provider login flow.

## Install and configure

The npm launcher forwards setup and settings to the native binary:

```sh
npx repotracer@latest setup --agents both
npx repotracer@latest settings
npx repotracer@latest settings --agents both --dry-run
```

The interactive wizard has two steps. First select Codex, Claude Code, or
both. Existing integrations are selected by default. Unchecking an existing
parent leaves it unchanged. Next accept recommended models or keep saved
choices. Select a mapping with Enter, or press A for advanced selection, to
search the native model catalog on the same screen.

The v2 defaults are:

| Parent agent | Scout provider | Scout model |
| --- | --- | --- |
| Codex | Codex | `gpt-5.6-luna` |
| Claude Code | Claude | `sonnet` |

The defaults are convenient starting values when available. Edit each parent
independently, even with only one parent selected. The current choices stay
visible above the model search; Save and Back are at the bottom. The custom model
format is `codex:model-id` or `claude:model-id`.

Nothing is written until Save or Ctrl-S. Esc goes back; Ctrl-C cancels without
changing settings. `--dry-run` previews a configuration. After
saving, restart both parent agents so their MCP processes reload the profiles.

For scripted per-parent mappings:

```sh
npx repotracer@latest settings --agents both \
  --codex-scout codex --codex-model gpt-5.6-luna \
  --claude-scout claude --claude-model sonnet
```

`--tracer-model provider:model-id` applies one subscription model to every
selected parent. For example, `--agents both --tracer-model claude:sonnet`
uses Claude Sonnet for both parent integrations. Use the per-parent flags when
the two mappings should differ.

## Parent profiles

RepoTracer stores independent configuration for each selected parent and
starts the MCP server with the matching profile. A Codex setting change cannot
rewrite the Claude scout profile, and a Claude setting change cannot rewrite
the Codex profile.

The Claude installer delegates MCP registration to Claude Code's native
`claude mcp add-json` command at user scope. It adds a managed RepoTracer block
to Claude's instruction file and preserves other instructions. An existing
untracked MCP entry is not overwritten. If registration fails, the changed
profile is restored when possible.

## Scout boundaries and result handling

The Claude scout exposes only native Read, Grep, and Glob operations. It does
not receive Bash, Edit, Write, network, delegation, plugins, skills, or other
external MCP servers. Repository path and citation checks are enforced by
RepoTracer, but they are application-level checks rather than an operating
system filesystem sandbox.

The scout receives the query and any supplied requirements, not the full parent
conversation. Its result contains a model-authored explanation and source
context. Returned citations are checked against the selected repository's
files and line ranges. This proves that a location resolves; it does not prove
that the explanation or conclusion is correct. Review important findings and
run relevant tests before changing code.

Related calls can pass the previous result's
`conversation.id` as `investigation.conversation_id`. The handle is bound to its repository and
native provider configuration. `conversation.status` reports `resumed`,
`fresh`, or `unknown`. Omit the handle for an independent question.

## Session and timeout behavior

Warm native provider processes are enabled by default. `session.idle_secs`
retires a retained process after inactivity, 300 seconds by default. This is an
idle-retention setting, not a total request or investigation timeout.

`model.timeout_ms` is an optional stream-inactivity limit. Native stream
activity resets it, and zero disables it. `explorer.timeout_seconds` is an
whole-investigation limit only for the generic OpenAI-compatible engine;
native Codex and Claude scouts do not use it. The parent or MCP
caller may have its own request allowance. These limits are separate from the
native model's turn and tool behavior.

Related conversations may reuse a retained process when the repository,
provider, model, and effort match. Independent questions start a new
conversation. A process is retired after errors, cancellation, idle expiry,
or configured session bounds. No disk conversation history is stored by the
RepoTracer session layer.

## Limitations and verification

RepoTracer does not promise lower cost, faster completion, or a particular
quality level. Results depend on the native CLI, selected model, account
limits, repository, and parent-agent decisions. The Claude backend does not
provide the optional RepoTracer Symbols operation as a native scout tool;
`repotracer symbols` remains a separate local CLI command.

The v2 release candidate has local Linux verification. macOS and Windows CI
verification is pending. Native CLI versions can expose different model
catalog entries or flags. Unsupported native options fail closed rather than
being silently replaced.
