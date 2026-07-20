# prompt-context-router

Prompt-triggered project context for Claude Code and Codex CLI.

`prompt-context-router` is a hook binary for Claude Code and Codex CLI that routes prompt keywords to project context files. When a submitted prompt matches a route, it injects the file guidance or a custom `note` as `additionalContext`, prefixed by a `[prompt-context-route:{id}]` marker.

It keeps topic-specific context out of always-loaded `CLAUDE.md` files. Context loads only when that topic appears in a prompt.

## Requirements

- Claude Code with `UserPromptSubmit` and `SessionStart` hooks.
- Codex CLI with hooks, which are enabled by default in current Codex. Hook schemas were verified against Codex `rust-v0.144.6`; project hooks require approval through `/hooks`.
- A Rust toolchain to build and install the binary.

## Install

```sh
cargo install --locked --git https://github.com/bengous/prompt-context-router
```

For reproducible installs, pin a commit with `--rev <sha>`.

## Configure

Add both hooks to `.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "prompt-context-router hook --config \"$CLAUDE_PROJECT_DIR/.claude/prompt-context-router.jsonc\"",
            "timeout": 60
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "compact",
        "hooks": [
          {
            "type": "command",
            "command": "prompt-context-router hook --config \"$CLAUDE_PROJECT_DIR/.claude/prompt-context-router.jsonc\"",
            "timeout": 60
          }
        ]
      }
    ]
  }
}
```

The `SessionStart` `compact` matcher restores routed context after compaction.

Define the shared route table in `.claude/prompt-context-router.jsonc`. Comments and trailing commas are allowed:

```jsonc
{
  "routes": [
    {
      "id": "claudex",
      "keywords": ["claudex"],
      "file": ".claude/rules/claudex.md",
    },
    {
      "id": "deploy",
      "keywords": ["deploy", "release"],
      "file": "docs/deploy.md",
      "note": "Read docs/deploy.md and confirm the target environment first.",
    },
  ],
}
```

- `id`: unique route identifier used in the marker; must match `[a-z0-9-]+`.
- `keywords`: non-empty list matched case-insensitively at word boundaries.
- `file`: project-relative context file. It must exist and remain inside the project root; `check` validates it.
- `note`: optional replacement for `Read {file} before making related claims or changes.`

Validate configuration and every route file in CI or pre-commit:

```sh
prompt-context-router check --config "$CLAUDE_PROJECT_DIR/.claude/prompt-context-router.jsonc"
```

## Configure Codex CLI

Add these hooks to the project's `.codex/config.toml`:

```toml
[[hooks.UserPromptSubmit]]

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = 'root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0; exec prompt-context-router codex-hook --project-root "$root" --config "$root/.claude/prompt-context-router.jsonc"'
timeout = 10

[[hooks.PostCompact]]
matcher = "^(manual|auto)$"

[[hooks.PostCompact.hooks]]
type = "command"
command = 'root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0; exec prompt-context-router codex-hook --project-root "$root" --config "$root/.claude/prompt-context-router.jsonc"'
timeout = 10

[[hooks.SessionStart]]
matcher = "^(resume|clear|compact)$"

[[hooks.SessionStart.hooks]]
type = "command"
command = 'root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0; exec prompt-context-router codex-hook --project-root "$root" --config "$root/.claude/prompt-context-router.jsonc"'
timeout = 10
```

Review and approve project hooks with `/hooks` in Codex CLI.

For all repositories, put the equivalent guarded hooks in `~/.codex/config.toml`; repositories without a route config become a silent no-op:

```toml
[[hooks.UserPromptSubmit]]

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = 'root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0; config="$root/.claude/prompt-context-router.jsonc"; [ -f "$config" ] || exit 0; exec prompt-context-router codex-hook --project-root "$root" --config "$config"'
timeout = 10

[[hooks.PostCompact]]
matcher = "^(manual|auto)$"

[[hooks.PostCompact.hooks]]
type = "command"
command = 'root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0; config="$root/.claude/prompt-context-router.jsonc"; [ -f "$config" ] || exit 0; exec prompt-context-router codex-hook --project-root "$root" --config "$config"'
timeout = 10

[[hooks.SessionStart]]
matcher = "^(resume|clear|compact)$"

[[hooks.SessionStart.hooks]]
type = "command"
command = 'root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0; config="$root/.claude/prompt-context-router.jsonc"; [ -f "$config" ] || exit 0; exec prompt-context-router codex-hook --project-root "$root" --config "$config"'
timeout = 10
```

The route config remains `.claude/prompt-context-router.jsonc`: both harnesses use one route table, and Codex consuming the `.claude/` path is intentional.

## Behavior

- A matching prompt injects the route marker and guidance as `hookSpecificOutput.additionalContext`.
- The hook fails open. Configuration, transcript, and state errors become a visible warning or silent no-op; they never block a prompt.

### Claude Code sessions

- A route whose marker already appears after the last compact boundary is live and is not injected again.
- `SessionStart` after compaction restores routes whose markers appear anywhere in transcript history.
- Only structured `hook_additional_context` attachments count for deduplication; marker text in messages or tool results is ignored.

### Codex sessions

- Deduplication uses an owned state file at `${XDG_CACHE_HOME:-~/.cache}/prompt-context-router/<project-hash>/<session-id>.json`, not the Codex rollout. The `transcript_path` field is deserialized but never read because the transcript schema is not a stable hook API.
- `PostCompact` clears the live set. `SessionStart` with `compact` restores known routes, `resume` restores only when a prior restore was missed, and `clear` resets both sets.
- State is committed only after hook output reaches and flushes stdout. On failure, the router prefers a later duplicate injection over lost context. A seven-day TTL sweep replaces cleanup through the unavailable `SessionEnd` event.

## Known limitations

- Deduplication relies on Claude Code's undocumented transcript JSONL schema. The handled attachment and compact-boundary forms were verified against Claude Code v2.1.215. On schema drift, the router fails open with a visible warning; the worst case is duplicate context injection, never a blocked prompt.
- Codex hook handling was verified against Codex `rust-v0.144.6` only.
- After automatic Codex compaction, restoration is guaranteed before the next prompt, not midway through the compacted turn.
- Deleting the Codex state cache loses deduplication state; the worst case is duplicate context injection.
- Codex subagent hooks use the parent session's deduplication domain.
- Route files are confined to the project root. Absolute paths and paths containing `..` are rejected.

## Development

The toolchain is pinned to Rust 1.96.0 in `rust-toolchain.toml` and `.mise.toml`.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```
