# Contributing

```bash
git clone https://github.com/repotracer/repotracer
cd repotracer
cargo test --workspace
cargo run -p repotracer -- doctor
cargo run -p repotracer -- scout "where is config loaded?" --mock
```

CI must pass without a GPU, a local model runtime, or API keys — use the mock backend.

## Native CLI smoke checks

Two opt-in scripts exercise a real native CLI instead of the mock backend.
They are functional checks of transport, session reuse, and returned evidence,
not prompt-quality benchmarks.

`scripts/codex-app-server-smoke.py` drives the Codex app-server protocol and
runs in CI, which installs the Codex CLI. `scripts/subscription-smoke.py` drives
the MCP server end to end over stdio and is the only coverage that puts a real
native CLI behind `repo_scout`, including the Claude backend. Run it locally:

```bash
cargo build -p repotracer
python3 scripts/subscription-smoke.py \
  --binary target/debug/repotracer \
  --output /tmp/repotracer-smoke

# Claude backend, using the signed-in `claude` CLI
python3 scripts/subscription-smoke.py \
  --binary target/debug/repotracer \
  --output /tmp/repotracer-smoke \
  --backend claude-cli
```

Other options: `--model` overrides the per-backend default (`gpt-5.6-luna` for
`codex-cli`, `sonnet` for `claude-cli`), `--native-executable` points at a
specific native CLI or wrapper, and `--impact-effort` sets the reasoning effort
for the change-impact case (default `high`).

The script copies `fixtures/investigation` into a temporary repository, starts
`repotracer serve`, and makes three live scout calls: locate, a continuation on
the same conversation handle after the fixture changes on disk, and an
independent change-impact investigation. It checks citations, source text, conversation
status, warm-process reuse, and that the text and structured renderings agree.
Each run writes its responses and a `summary.json` into a fresh subdirectory of
`--output`; earlier runs are never overwritten.

Run it before a release, and after changing the native session layer, the MCP
handoff format, or either backend. It is deliberately not wired into
`.github/workflows/ci.yml`: it needs an interactive native CLI login that CI
does not have, and each run spends real subscription quota.

## Before publishing

```bash
scripts/verify-release.sh              # local build
scripts/verify-release.sh --published  # what npx actually serves
```

Installs into a throwaway HOME, drives the MCP server over stdio the way Codex
does, makes one live scout call, checks every returned citation resolves, then
uninstalls. Your real `~/.codex` is never touched. Costs about a cent of Codex
quota for the one live call.

## Good first areas

- Agent integrations
- Windows path edge cases
- Benchmark tasks + verifiers
- Doctor diagnostics
- Docs clarity

## Style

- Prefer boring Rust
- No new crates for one struct
- Tests for path security and concurrency regressions
