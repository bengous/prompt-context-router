use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::TempDir;

use crate::{Config, HookOutput, load_config, validate_route_files};

pub const CONFIG: &str = r#"
        {
          // Comments and trailing commas are intentional.
          "routes": [
            {
              "id": "claudex",
              "keywords": ["claudex"],
              "file": ".claude/rules/claudex.md",
            },
          ],
        }
    "#;

pub fn test_project() -> Result<TempDir, Box<dyn Error>> {
    let project = tempfile::tempdir()?;
    fs::create_dir_all(project.path().join(".claude/rules"))?;
    fs::write(project.path().join(".claude/rules/claudex.md"), "# Claudex")?;
    fs::write(project.path().join(".claude/rules/beta.md"), "# Beta")?;
    Ok(project)
}

pub fn write_config(project: &Path, config: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = project.join(".claude/prompt-context-router.jsonc");
    fs::write(&path, config)?;
    Ok(path)
}

pub fn load_test_config(project: &Path, config: &str) -> Result<Config, Box<dyn Error>> {
    let path = write_config(project, config)?;
    let config = load_config(&path)?;
    validate_route_files(&config, project)?;
    Ok(config)
}

pub fn attachment(context: &str) -> String {
    json!({
        "type": "attachment",
        "attachment": {
            "type": "hook_additional_context",
            "content": [context],
            "hookName": "UserPromptSubmit",
            "hookEvent": "UserPromptSubmit"
        }
    })
    .to_string()
}

pub fn compact_boundary() -> String {
    json!({
        "type": "system",
        "subtype": "compact_boundary",
        "content": "Conversation compacted",
        "compactMetadata": {"trigger": "auto"}
    })
    .to_string()
}

pub fn hook_input(event: &str, prompt: Option<&str>, transcript: Option<&Path>) -> String {
    json!({
        "hook_event_name": event,
        "prompt": prompt,
        "transcript_path": transcript
    })
    .to_string()
}

pub fn context(output: &HookOutput) -> Option<&str> {
    output
        .hook_specific_output
        .as_ref()
        .map(|specific| specific.additional_context.as_str())
}
