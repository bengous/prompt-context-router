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
- `src/transcript.rs`: the only module coupled to Claude Code's undocumented
  transcript JSONL schema; handled forms were verified against v2.1.215.
- `src/hook.rs`: hook input parsing, routing flow, warnings, and output assembly.
- `src/test_support.rs`: shared unit-test helpers.
- `tests/hook_contract.rs`: public-API contract tests using synthetic fixtures
  under `tests/fixtures/`.

## Invariants

- Fail open: the hook must never block a prompt. Errors degrade to a visible
  warning or silent no-op.
- The `[prompt-context-route:{id}]` marker is not configurable; transcript
  deduplication depends on it.
- Live routes have a marker after the last compact boundary and deduplicate matching prompts.
- Known routes have a marker anywhere in transcript history and are restored after compaction.
- Deduplication counts only structured `hook_additional_context` attachments,
  never marker text in messages or tool results.
