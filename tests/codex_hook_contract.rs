use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use prompt_context_router::{CodexHookResult, process_codex_hook};
use serde_json::{Value, json};
use tempfile::TempDir;

const CONFIG: &str = r#"{
  "routes": [
    {
      "id": "claudex",
      "keywords": ["claudex"],
      "file": ".claude/rules/claudex.md"
    }
  ]
}"#;

const NOTE_CONFIG: &str = r#"{
  "routes": [
    {
      "id": "claudex",
      "keywords": ["claudex"],
      "file": ".claude/rules/claudex.md",
      "note": "Follow the Claudex project rules."
    }
  ]
}"#;

const BROKEN_ROUTE_CONFIG: &str = r#"{
  "routes": [
    {
      "id": "claudex",
      "keywords": ["claudex"],
      "file": ".claude/rules/claudex.md"
    },
    {
      "id": "broken",
      "keywords": ["broken"],
      "file": ".claude/rules/missing.md"
    }
  ]
}"#;

const DEFAULT_CONTEXT: &str = "[prompt-context-route:claudex]\nRead .claude/rules/claudex.md before making related claims or changes.";
const NOTE_CONTEXT: &str = "[prompt-context-route:claudex]\nFollow the Claudex project rules.";

fn test_project(config: &str) -> Result<(TempDir, PathBuf), Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    fs::create_dir_all(project.path().join(".claude/rules"))?;
    fs::write(project.path().join(".claude/rules/claudex.md"), "# Claudex")?;
    let config_path = project.path().join("prompt-context-router.jsonc");
    fs::write(&config_path, config)?;
    Ok((project, config_path))
}

fn prompt_input(
    project_root: &Path,
    session_id: &str,
    prompt: &str,
    transcript_path: Option<&Path>,
) -> String {
    json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session_id,
        "prompt": prompt,
        "cwd": project_root,
        "transcript_path": transcript_path,
        "future_codex_field": {"nested": true}
    })
    .to_string()
}

fn event_input(project_root: &Path, event: &str, session_id: &str, source: Option<&str>) -> String {
    json!({
        "hook_event_name": event,
        "session_id": session_id,
        "cwd": project_root,
        "transcript_path": null,
        "source": source,
        "trigger": "auto"
    })
    .to_string()
}

fn process_and_commit(
    config_path: &Path,
    project_root: &Path,
    cache_root: &Path,
    input: &str,
) -> Result<Option<Value>, Box<dyn Error>> {
    let result: CodexHookResult = process_codex_hook(config_path, project_root, cache_root, input);
    let output = result.output().map(serde_json::to_value).transpose()?;
    result.commit();
    Ok(output)
}

fn serialized(
    config_path: &Path,
    project_root: &Path,
    cache_root: &Path,
    input: &str,
) -> Result<Value, Box<dyn Error>> {
    process_and_commit(config_path, project_root, cache_root, input)?
        .ok_or_else(|| "expected Codex hook output".into())
}

fn saved_state_file(cache_root: &Path, session_id: &str) -> Result<PathBuf, Box<dyn Error>> {
    let namespace = fs::read_dir(cache_root)?
        .next()
        .transpose()?
        .ok_or("cache namespace was not created")?;
    Ok(namespace.path().join(format!("{session_id}.json")))
}

#[test]
fn initial_injection_serializes_exact_codex_contract() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let input = prompt_input(project.path(), "initial", "check claudex", None);
    let output = serialized(&config_path, project.path(), cache.path(), &input)?;

    assert_eq!(
        output,
        json!({
            "systemMessage": "Prompt context loaded: .claude/rules/claudex.md",
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": DEFAULT_CONTEXT
            }
        })
    );
    Ok(())
}

#[test]
fn same_session_deduplicates_after_first_commit() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let input = prompt_input(project.path(), "dedup", "claudex", None);

    serialized(&config_path, project.path(), cache.path(), &input)?;
    let second = process_and_commit(&config_path, project.path(), cache.path(), &input)?;

    assert_eq!(second, None);
    Ok(())
}

#[test]
fn different_session_ids_inject_independently() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let first = serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), "session-a", "claudex", None),
    )?;
    let second = serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), "session-b", "claudex", None),
    )?;
    let expected = json!({
        "systemMessage": "Prompt context loaded: .claude/rules/claudex.md",
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": DEFAULT_CONTEXT
        }
    });

    assert_eq!([first, second], [expected.clone(), expected]);
    Ok(())
}

#[test]
fn post_compact_clears_live_and_preserves_known_route() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let prompt = prompt_input(project.path(), "post-compact", "claudex", None);
    let post_compact = event_input(project.path(), "PostCompact", "post-compact", None);
    let compact = event_input(
        project.path(),
        "SessionStart",
        "post-compact",
        Some("compact"),
    );

    serialized(&config_path, project.path(), cache.path(), &prompt)?;
    process_and_commit(&config_path, project.path(), cache.path(), &post_compact)?;
    let restored = serialized(&config_path, project.path(), cache.path(), &compact)?;

    assert_eq!(
        restored["hookSpecificOutput"]["additionalContext"],
        DEFAULT_CONTEXT
    );
    Ok(())
}

#[test]
fn compact_session_start_restores_without_system_message() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let session_id = "compact";

    serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), session_id, "claudex", None),
    )?;
    process_and_commit(
        &config_path,
        project.path(),
        cache.path(),
        &event_input(project.path(), "PostCompact", session_id, None),
    )?;
    let output = serialized(
        &config_path,
        project.path(),
        cache.path(),
        &event_input(project.path(), "SessionStart", session_id, Some("compact")),
    )?;

    assert_eq!(
        output,
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": DEFAULT_CONTEXT
            }
        })
    );
    Ok(())
}

#[test]
fn resume_with_live_context_is_silent() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let session_id = "resume-live";

    serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), session_id, "claudex", None),
    )?;
    let resumed = process_and_commit(
        &config_path,
        project.path(),
        cache.path(),
        &event_input(project.path(), "SessionStart", session_id, Some("resume")),
    )?;

    assert_eq!(resumed, None);
    Ok(())
}

#[test]
fn resume_restores_when_live_is_empty_and_known_is_not() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let session_id = "resume-empty";

    serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), session_id, "claudex", None),
    )?;
    process_and_commit(
        &config_path,
        project.path(),
        cache.path(),
        &event_input(project.path(), "PostCompact", session_id, None),
    )?;
    let output = serialized(
        &config_path,
        project.path(),
        cache.path(),
        &event_input(project.path(), "SessionStart", session_id, Some("resume")),
    )?;

    assert_eq!(
        output,
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": DEFAULT_CONTEXT
            }
        })
    );
    Ok(())
}

#[test]
fn clear_session_start_allows_fresh_prompt_injection() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let session_id = "clear";
    let prompt = prompt_input(project.path(), session_id, "claudex", None);

    serialized(&config_path, project.path(), cache.path(), &prompt)?;
    process_and_commit(
        &config_path,
        project.path(),
        cache.path(),
        &event_input(project.path(), "SessionStart", session_id, Some("clear")),
    )?;
    let fresh = serialized(&config_path, project.path(), cache.path(), &prompt)?;

    assert_eq!(
        fresh,
        json!({
            "systemMessage": "Prompt context loaded: .claude/rules/claudex.md",
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": DEFAULT_CONTEXT
            }
        })
    );
    Ok(())
}

#[test]
fn corrupt_state_reinjects_and_surfaces_reset_warning() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let session_id = "corrupt";
    let prompt = prompt_input(project.path(), session_id, "claudex", None);

    serialized(&config_path, project.path(), cache.path(), &prompt)?;
    fs::write(saved_state_file(cache.path(), session_id)?, "not json")?;
    let output = serialized(&config_path, project.path(), cache.path(), &prompt)?;
    let system_message = output["systemMessage"].as_str().unwrap_or_default();

    assert!(
        output["hookSpecificOutput"]["additionalContext"] == DEFAULT_CONTEXT
            && system_message.contains("Prompt context router warning:")
            && system_message.contains("session state was reset")
    );
    Ok(())
}

#[test]
fn broken_route_warns_without_disabling_valid_route() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(BROKEN_ROUTE_CONFIG)?;
    let cache = tempfile::tempdir()?;
    let input = prompt_input(project.path(), "broken-route", "claudex and broken", None);
    let output = serialized(&config_path, project.path(), cache.path(), &input)?;
    let system_message = output["systemMessage"].as_str().unwrap_or_default();

    assert!(
        output["hookSpecificOutput"]["additionalContext"] == DEFAULT_CONTEXT
            && system_message.contains("Prompt context loaded: .claude/rules/claudex.md")
            && system_message.contains("Prompt context router warning:")
            && system_message.contains(".claude/rules/missing.md")
    );
    Ok(())
}

#[test]
fn transcript_path_is_ignored_for_noise_null_and_missing_files() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let cache = tempfile::tempdir()?;
    let noisy = project.path().join("rollout.jsonl");
    fs::write(&noisy, "[prompt-context-route:claudex]\n")?;
    let missing = project.path().join("missing-rollout.jsonl");

    let noisy_output = serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), "noise", "claudex", Some(&noisy)),
    )?;
    let null_output = serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), "null", "claudex", None),
    )?;
    let missing_output = serialized(
        &config_path,
        project.path(),
        cache.path(),
        &prompt_input(project.path(), "missing", "claudex", Some(&missing)),
    )?;
    let expected = json!({
        "systemMessage": "Prompt context loaded: .claude/rules/claudex.md",
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": DEFAULT_CONTEXT
        }
    });

    assert_eq!(
        [noisy_output, null_output, missing_output],
        [expected.clone(), expected.clone(), expected]
    );
    Ok(())
}

#[test]
fn custom_note_is_second_context_line_on_codex_path() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(NOTE_CONFIG)?;
    let cache = tempfile::tempdir()?;
    let input = prompt_input(project.path(), "note", "claudex", None);
    let output = serialized(&config_path, project.path(), cache.path(), &input)?;

    assert_eq!(
        output,
        json!({
            "systemMessage": "Prompt context loaded: .claude/rules/claudex.md",
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": NOTE_CONTEXT
            }
        })
    );
    Ok(())
}
