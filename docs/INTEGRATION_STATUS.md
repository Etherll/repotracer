# v2 integration status

RepoTracer v2 is a release candidate. It supports independent MCP profiles for
Codex and Claude Code, with native subscription CLIs providing authentication
and model execution.

## Included

- Codex and Claude Code parent integrations, installed separately or together.
- A full-screen, two-step terminal wizard with searchable models and visible
  mappings, built with Ratatui.
- Defaults of `codex:gpt-5.6-luna` for Codex and `claude:sonnet` for Claude
  Code.
- Inline advanced selection for independent per-parent mappings.
- Native model discovery, custom `provider:model-id` entries, dry runs, and
  cancellation without partial settings writes.
- Independent profiles and bounded process/conversation reuse.
- The `repo_scout` MCP tool with repository and focus hints, investigation
  intents, optional follow-up handles, structured results, and text fallback.
- Read-only native repository tools and path/line citation validation.

## Verification

Local release-candidate checks currently cover Linux. They include the Rust
workspace and npm launcher checks used by this repository. The exact commands
belong to the release pipeline and may change with the candidate.

macOS and Windows CI verification is pending. Until those jobs complete, the
release candidate should not be described as end-to-end verified on those
platforms.

The defaults describe the configuration shipped by v2; they do not predict an
outcome. This status page does not make cost, latency, or quality claims.

## Behavior and limits

RepoTracer validates that returned citation paths and line ranges resolve
inside the selected repository. Source validation checks locations, not the
truth of model-authored findings. Important conclusions still require local
review and relevant tests.

Warm provider processes may be retained for related calls. `session.idle_secs`
retires an inactive retained process; it is an idle-retention setting, not a
total request or investigation timeout. `model.timeout_ms` measures native
stream inactivity. `explorer.timeout_seconds` is an optional whole-run limit
only for the generic OpenAI-compatible engine, not native scouts. Both default
to zero.

Claude scouts use Read, Grep, and Glob. Codex scouts use their native read-only
shell and optional Symbols lookup. Editing and external MCP tools are disabled.
Repository-root and symlink checks are application
controls, not an operating-system filesystem sandbox. Provider behavior,
account limits, model availability, and parent-agent decisions remain outside
RepoTracer's control.

The native parent CLIs remain required. Their supported flags and model
catalogs can vary by installed version. Unavailable current models remain
selectable only when the settings screen labels them as availability
unverified.
