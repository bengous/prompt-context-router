use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use prompt_context_router::{
    Config, ConfigError, HookOutput, load_config, process_hook, router_warning,
    validate_route_files,
};
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

const INVALID_CONFIG: &str = r#"{
  "routes": [
    {
      "id": "claudex",
      "keywords": ["claudex"],
      "file": ".claude/rules/claudex.md",
      "note": "Never echo [prompt-context-route: here"
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

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn prompt_input(prompt: &str, transcript_path: Option<&Path>) -> String {
    json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": prompt,
        "transcript_path": transcript_path
    })
    .to_string()
}

fn compact_input(transcript_path: &Path) -> String {
    json!({
        "hook_event_name": "SessionStart",
        "source": "compact",
        "transcript_path": transcript_path
    })
    .to_string()
}

fn serialized(output: Option<HookOutput>) -> Result<Value, Box<dyn Error>> {
    let Some(output) = output else {
        return Err("expected hook output".into());
    };
    Ok(serde_json::to_value(output)?)
}

#[test]
fn matching_prompt_serializes_camel_case_contract() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let config: Config = load_config(&config_path)?;
    validate_route_files(&config, project.path())?;
    let output = serialized(process_hook(
        &config_path,
        project.path(),
        &prompt_input("check claudex", None),
    ))?;

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
fn live_marker_deduplicates_prompt() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let transcript_path = fixture_path("live-marker.jsonl");
    let output = process_hook(
        &config_path,
        project.path(),
        &prompt_input("check claudex", Some(&transcript_path)),
    );

    assert!(output.is_none());
    Ok(())
}

#[test]
fn pre_compact_marker_allows_prompt_reinjection() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let transcript_path = fixture_path("marker-before-compact.jsonl");
    let output = serialized(process_hook(
        &config_path,
        project.path(),
        &prompt_input("check claudex", Some(&transcript_path)),
    ))?;

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
fn compact_restores_known_route_without_system_message() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let transcript_path = fixture_path("marker-before-compact.jsonl");
    let output = serialized(process_hook(
        &config_path,
        project.path(),
        &compact_input(&transcript_path),
    ))?;

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
fn transcript_warning_surfaces_are_serialized() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let torn_path = fixture_path("torn-line.jsonl");
    let partial = serialized(process_hook(
        &config_path,
        project.path(),
        &compact_input(&torn_path),
    ))?;

    assert_eq!(
        partial,
        json!({
            "systemMessage": "Prompt context restore may be incomplete: some transcript lines were malformed.",
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": DEFAULT_CONTEXT
            }
        })
    );

    let missing_path = project.path().join("missing.jsonl");
    let unavailable = serialized(process_hook(
        &config_path,
        project.path(),
        &compact_input(&missing_path),
    ))?;

    assert_eq!(
        unavailable,
        json!({
            "systemMessage": "Prompt context could not be restored after compaction: transcript unavailable."
        })
    );
    Ok(())
}

#[test]
fn marker_noise_does_not_deduplicate_prompt() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(CONFIG)?;
    let transcript_path = fixture_path("noise-marker.jsonl");
    let output = serialized(process_hook(
        &config_path,
        project.path(),
        &prompt_input("check claudex", Some(&transcript_path)),
    ))?;

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
fn invalid_config_returns_error_and_fail_open_warning() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(INVALID_CONFIG)?;
    let result: Result<Config, ConfigError> = load_config(&config_path);
    let Err(error) = result else {
        return Err("invalid config unexpectedly loaded".into());
    };
    assert!(error.to_string().contains("[prompt-context-route:"));

    let expected_warning = router_warning(&error);
    let output = serialized(process_hook(
        &config_path,
        project.path(),
        &prompt_input("check claudex", None),
    ))?;

    assert_eq!(output, json!({"systemMessage": expected_warning}));
    Ok(())
}

#[test]
fn custom_note_serializes_as_second_context_line() -> Result<(), Box<dyn Error>> {
    let (project, config_path) = test_project(NOTE_CONFIG)?;
    let output = serialized(process_hook(
        &config_path,
        project.path(),
        &prompt_input("check claudex", None),
    ))?;

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
