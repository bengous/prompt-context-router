# prompt-context-router

## Commands

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```

## Module map

- `src/lib.rs`: public API declarations and re-exports only.
- `src/config.rs`: config loading, validation, route matching, and file
  checks.
- `src/routing.rs`: shared output types, context rendering, and route-file
  validation flow.
- `src/claude/mod.rs`: Claude hook input parsing, event dispatch, and output
  assembly.
- `src/claude/transcript.rs`: the only module coupled to Claude Code's
  undocumented transcript JSONL schema; handled forms were verified against
  v2.1.215.
- `src/codex/mod.rs`: Codex hook input parsing, event dispatch, output assembly,
  and deferred state commit.
- `src/codex/state.rs`: locked, atomic, TTL-swept Codex session state.
- `src/test_support.rs`: shared unit-test helpers.
- `src/main.rs`: CLI dispatch and stdout-before-state-commit ordering.
- `tests/hook_contract.rs`: Claude public-API contract tests using synthetic
  fixtures under `tests/fixtures/`.
- `tests/codex_hook_contract.rs`: Codex public-API contract tests with hermetic
  state caches.

## Invariants

- Fail open is supreme: the hook must never block a prompt. Errors degrade to a
  visible warning or silent no-op, even when deduplication or persistence fails.
- The `[prompt-context-route:{id}]` marker is not configurable; Claude
  transcript deduplication depends on it.
- Claude live routes have a marker after the last compact boundary and
  deduplicate matching prompts. Known routes have a marker anywhere in
  transcript history and are restored after compaction.
- Claude deduplication counts only structured `hook_additional_context`
  attachments, never marker text in messages or tool results.
- The owned state file is the only Codex session state. Codex rollout and
  transcript files are never parsed.
- Codex state is committed only after hook stdout is flushed. Save failure
  stays fail-open and may cause a later duplicate injection.
