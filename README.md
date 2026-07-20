# prompt-context-router

Prompt-triggered project context for Claude Code.

`prompt-context-router` is a Claude Code hook binary that routes prompt keywords to project context files. When a submitted prompt matches a route, it injects the file guidance or a custom `note` as `additionalContext`, prefixed by a `[prompt-context-route:{id}]` marker.

It keeps topic-specific context out of always-loaded `CLAUDE.md` files. Context loads only when that topic appears in a prompt.

## Requirements

- Claude Code with `UserPromptSubmit` and `SessionStart` hooks.
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

Define routes in `.claude/prompt-context-router.jsonc`. Comments and trailing commas are allowed:

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

## Behavior

- A matching prompt injects the route marker and guidance as `hookSpecificOutput.additionalContext`.
- A route whose marker already appears after the last compact boundary is live and is not injected again.
- `SessionStart` after compaction restores routes whose markers appear anywhere in transcript history.
- Only structured `hook_additional_context` attachments count for deduplication; marker text in messages or tool results is ignored.
- The hook fails open. Configuration errors, unreadable transcripts, and malformed lines become a visible warning or silent no-op; they never block a prompt.

## Known limitations

- Deduplication relies on Claude Code's undocumented transcript JSONL schema. The handled attachment and compact-boundary forms were verified against Claude Code v2.1.215. On schema drift, the router fails open with a visible warning; the worst case is duplicate context injection, never a blocked prompt.
- Route files are confined to the project root. Absolute paths and paths containing `..` are rejected.

## Development

The toolchain is pinned to Rust 1.96.0 in `rust-toolchain.toml` and `.mise.toml`.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
```
