use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{
    Config, ConfigError, Route, canonical_project_root, detect_routes, load_config,
    partition_valid_routes,
};
use crate::transcript::{ScanMode, load_transcript_state};

const COMPACT_WARNING: &str =
    "Prompt context could not be restored after compaction: transcript unavailable.";
const COMPACT_PARTIAL_WARNING: &str =
    "Prompt context restore may be incomplete: some transcript lines were malformed.";

#[derive(Debug, Error)]
enum HookError {
    #[error("invalid hook input: {0}")]
    Input(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

#[derive(Debug, Deserialize)]
struct HookInput {
    hook_event_name: String,
    transcript_path: Option<PathBuf>,
    prompt: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput {
    pub(crate) hook_event_name: &'static str,
    pub(crate) additional_context: String,
}

enum HookEvent<'a> {
    Prompt {
        prompt: &'a str,
        transcript_path: Option<&'a Path>,
    },
    Compact {
        transcript_path: Option<&'a Path>,
    },
}

/// Processes one Claude hook input. Configuration and input failures become a
/// visible warning response so the hook remains non-blocking.
#[must_use]
pub fn process_hook(config_path: &Path, project_root: &Path, input: &str) -> Option<HookOutput> {
    match process_hook_inner(config_path, project_root, input) {
        Ok(output) => output,
        Err(error) => Some(HookOutput::warning(router_warning(error))),
    }
}

/// Formats the uniform, greppable warning prefix used by every router message.
#[must_use]
pub fn router_warning(error: impl std::fmt::Display) -> String {
    format!("Prompt context router warning: {error}")
}

impl HookOutput {
    /// Builds a visible, non-blocking hook warning.
    #[must_use]
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            system_message: Some(message.into()),
            hook_specific_output: None,
        }
    }
}

#[derive(Debug)]
struct ValidatedInjection<'a> {
    routes: Vec<&'a Route>,
    warnings: Vec<String>,
    specific: Option<HookSpecificOutput>,
}

/// Canonicalizes the root and validates `candidates` individually, returning
/// the surviving routes, per-route warnings, and the injection payload shared
/// by the prompt and compact paths.
fn validated_injection<'a>(
    event_name: &'static str,
    candidates: &[&'a Route],
    project_root: &Path,
) -> Result<ValidatedInjection<'a>, ConfigError> {
    let canonical_root = canonical_project_root(project_root)?;
    let (routes, warnings) = partition_valid_routes(candidates, &canonical_root);
    let specific = (!routes.is_empty()).then(|| HookSpecificOutput {
        hook_event_name: event_name,
        additional_context: render_route_context(&routes),
    });
    Ok(ValidatedInjection {
        routes,
        warnings,
        specific,
    })
}

fn process_hook_inner(
    config_path: &Path,
    project_root: &Path,
    input: &str,
) -> Result<Option<HookOutput>, HookError> {
    let input: HookInput =
        serde_json::from_str(input).map_err(|error| HookError::Input(error.to_string()))?;
    let Some(event) = hook_event(&input)? else {
        return Ok(None);
    };

    let config = load_config(config_path)?;
    Ok(match event {
        HookEvent::Prompt {
            prompt,
            transcript_path,
        } => output_for_prompt(prompt, transcript_path, &config, project_root)?,
        HookEvent::Compact { transcript_path } => {
            output_for_compact(transcript_path, &config, project_root)?
        }
    })
}

/// Returns `Ok(None)` for events the router deliberately ignores.
fn hook_event(input: &HookInput) -> Result<Option<HookEvent<'_>>, HookError> {
    match input.hook_event_name.as_str() {
        // A missing prompt stays silent instead of warning: the hook is
        // advisory and an empty prompt simply matches no route.
        "UserPromptSubmit" => Ok(Some(HookEvent::Prompt {
            prompt: input.prompt.as_deref().unwrap_or_default(),
            transcript_path: input.transcript_path.as_deref(),
        })),
        "SessionStart" => match input.source.as_deref() {
            Some("compact") => Ok(Some(HookEvent::Compact {
                transcript_path: input.transcript_path.as_deref(),
            })),
            Some("startup" | "resume" | "clear") => Ok(None),
            Some(source) => Err(HookError::Input(format!(
                "unsupported SessionStart source `{source}`"
            ))),
            None => Err(HookError::Input(
                "SessionStart requires string field `source`".to_owned(),
            )),
        },
        event => Err(HookError::Input(format!(
            "unsupported hook event `{event}`"
        ))),
    }
}

fn output_for_prompt(
    prompt: &str,
    transcript_path: Option<&Path>,
    config: &Config,
    project_root: &Path,
) -> Result<Option<HookOutput>, ConfigError> {
    let matching_routes = detect_routes(prompt, config);
    if matching_routes.is_empty() {
        return Ok(None);
    }

    // Dedup against the live window only: a marker stranded before a compact
    // boundary whose reinjection failed must not silence the route forever.
    let state = load_transcript_state(transcript_path, config, ScanMode::Prompt);
    let new_routes: Vec<_> = matching_routes
        .into_iter()
        .filter(|route| !state.live_route_ids.contains(&route.id))
        .collect();
    if new_routes.is_empty() {
        return Ok(None);
    }

    // Validate only the routes about to be injected, individually: a broken
    // route file warns without disabling the other routes.
    let injection = validated_injection("UserPromptSubmit", &new_routes, project_root)?;
    let mut messages = Vec::new();
    if !injection.routes.is_empty() {
        messages.push(render_loaded_routes_message(&injection.routes));
    }
    messages.extend(injection.warnings);

    Ok(Some(HookOutput {
        system_message: Some(messages.join("\n")),
        hook_specific_output: injection.specific,
    }))
}

fn output_for_compact(
    transcript_path: Option<&Path>,
    config: &Config,
    project_root: &Path,
) -> Result<Option<HookOutput>, ConfigError> {
    // Restore from the full marker history: everything before the boundary
    // left the live window and belongs in the fresh one.
    let state = load_transcript_state(transcript_path, config, ScanMode::Compact);
    let active_routes: Vec<_> = config
        .routes
        .iter()
        .filter(|route| state.known_route_ids.contains(&route.id))
        .collect();
    if active_routes.is_empty() && !state.unavailable && !state.partially_malformed {
        return Ok(None);
    }

    let (mut messages, specific) = if active_routes.is_empty() {
        (Vec::new(), None)
    } else {
        let injection = validated_injection("SessionStart", &active_routes, project_root)?;
        (injection.warnings, injection.specific)
    };
    if state.partially_malformed {
        messages.insert(0, COMPACT_PARTIAL_WARNING.to_owned());
    }
    if state.unavailable {
        messages.insert(0, COMPACT_WARNING.to_owned());
    }

    Ok(Some(HookOutput {
        system_message: (!messages.is_empty()).then(|| messages.join("\n")),
        hook_specific_output: specific,
    }))
}

fn render_route_context(routes: &[&Route]) -> String {
    routes
        .iter()
        .map(|route| {
            route.note.as_deref().map_or_else(
                || {
                    format!(
                        "{}\nRead {} before making related claims or changes.",
                        route.marker, route.file
                    )
                },
                |note| format!("{}\n{note}", route.marker),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_loaded_routes_message(routes: &[&Route]) -> String {
    format!(
        "Prompt context loaded: {}",
        routes
            .iter()
            .map(|route| route.file.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::test_support::{
        CONFIG, attachment, compact_boundary, context, hook_input, test_project, write_config,
    };

    #[test]
    fn prompt_without_prompt_field_is_silent() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let input = json!({"hook_event_name":"UserPromptSubmit"}).to_string();

        assert_eq!(process_hook(&config_path, project.path(), &input), None);
        Ok(())
    }

    #[test]
    fn broken_route_file_warns_without_blocking_other_routes() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = r#"{
          "routes": [
            {"id":"claudex","keywords":["claudex"],"file":".claude/rules/claudex.md"},
            {"id":"beta","keywords":["beta"],"file":".claude/rules/beta.md"}
          ]
        }"#;
        let config_path = write_config(project.path(), config)?;
        fs::remove_file(project.path().join(".claude/rules/beta.md"))?;
        let input = hook_input("UserPromptSubmit", Some("claudex and beta"), None);
        let Some(output) = process_hook(&config_path, project.path(), &input) else {
            return Err("prompt did not inject context".into());
        };

        let message = output.system_message.as_deref().unwrap_or_default();
        assert!(message.contains("Prompt context loaded: .claude/rules/claudex.md"));
        assert!(message.contains("Prompt context router warning"));
        let context = context(&output).unwrap_or_default();
        assert!(context.contains("[prompt-context-route:claudex]"));
        assert!(!context.contains("[prompt-context-route:beta]"));
        Ok(())
    }

    #[test]
    fn prompt_injects_once_then_deduplicates() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let transcript_path = project.path().join("transcript.jsonl");
        fs::write(&transcript_path, "")?;
        let input = hook_input(
            "UserPromptSubmit",
            Some("Check Claudex"),
            Some(&transcript_path),
        );
        let Some(first) = process_hook(&config_path, project.path(), &input) else {
            return Err("first prompt did not inject context".into());
        };
        let Some(first_context) = context(&first) else {
            return Err("first prompt output had no context".into());
        };
        fs::write(&transcript_path, attachment(first_context))?;

        assert_eq!(
            (
                first.system_message.as_deref(),
                process_hook(&config_path, project.path(), &input)
            ),
            (
                Some("Prompt context loaded: .claude/rules/claudex.md"),
                None
            )
        );
        Ok(())
    }

    #[test]
    fn custom_note_replaces_default_guidance() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace(
            "\"file\": \".claude/rules/claudex.md\",",
            "\"file\": \".claude/rules/claudex.md\",\n              \"note\": \"Follow the Claudex project rules.\",",
        );
        let config_path = write_config(project.path(), &config)?;
        let input = hook_input("UserPromptSubmit", Some("claudex"), None);
        let Some(output) = process_hook(&config_path, project.path(), &input) else {
            return Err("prompt did not inject custom context".into());
        };

        assert_eq!(
            context(&output),
            Some("[prompt-context-route:claudex]\nFollow the Claudex project rules.")
        );
        Ok(())
    }

    #[test]
    fn custom_note_preserves_marker_deduplication() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace(
            "\"file\": \".claude/rules/claudex.md\",",
            "\"file\": \".claude/rules/claudex.md\",\n              \"note\": \"Follow the Claudex project rules.\",",
        );
        let config_path = write_config(project.path(), &config)?;
        let transcript_path = project.path().join("transcript.jsonl");
        fs::write(
            &transcript_path,
            attachment("[prompt-context-route:claudex]\nFollow the Claudex project rules."),
        )?;
        let input = hook_input("UserPromptSubmit", Some("claudex"), Some(&transcript_path));

        assert_eq!(process_hook(&config_path, project.path(), &input), None);
        Ok(())
    }

    #[test]
    fn compact_reinjects_active_routes_without_success_message() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let transcript_path = project.path().join("transcript.jsonl");
        fs::write(
            &transcript_path,
            attachment("[prompt-context-route:claudex]"),
        )?;
        let input = json!({
            "hook_event_name":"SessionStart",
            "source":"compact",
            "transcript_path":transcript_path
        })
        .to_string();
        let Some(output) = process_hook(&config_path, project.path(), &input) else {
            return Err("compact did not inject context".into());
        };

        assert_eq!(
            (output.system_message.as_deref(), context(&output)),
            (
                None,
                Some(
                    "[prompt-context-route:claudex]\nRead .claude/rules/claudex.md before making related claims or changes."
                )
            )
        );
        Ok(())
    }

    #[test]
    fn startup_resume_and_clear_are_silent() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let outputs: Vec<_> = ["startup", "resume", "clear"]
            .into_iter()
            .map(|source| {
                process_hook(
                    &config_path,
                    project.path(),
                    &json!({"hook_event_name":"SessionStart","source":source}).to_string(),
                )
            })
            .collect();

        assert_eq!(outputs, [None, None, None]);
        Ok(())
    }

    #[test]
    fn unavailable_or_malformed_transcript_does_not_block_prompt() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let malformed = project.path().join("malformed.jsonl");
        fs::write(&malformed, "{broken")?;
        let missing = project.path().join("missing.jsonl");
        let outputs: Vec<_> = [&missing, &malformed]
            .into_iter()
            .map(|transcript| {
                process_hook(
                    &config_path,
                    project.path(),
                    &hook_input("UserPromptSubmit", Some("claudex"), Some(transcript)),
                )
            })
            .collect();

        assert!(outputs.iter().all(|output| {
            output
                .as_ref()
                .and_then(context)
                .is_some_and(|context| context.contains("[prompt-context-route:claudex]"))
        }));
        Ok(())
    }

    #[test]
    fn unavailable_or_malformed_transcript_warns_after_compact() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let malformed = project.path().join("malformed.jsonl");
        fs::write(&malformed, "{broken")?;
        let missing = project.path().join("missing.jsonl");
        let outputs: Vec<_> = [&missing, &malformed]
            .into_iter()
            .map(|transcript_path| {
                process_hook(
                    &config_path,
                    project.path(),
                    &json!({
                        "hook_event_name":"SessionStart",
                        "source":"compact",
                        "transcript_path":transcript_path
                    })
                    .to_string(),
                )
            })
            .collect();

        let messages: Vec<_> = outputs
            .iter()
            .map(|output| {
                output
                    .as_ref()
                    .and_then(|output| output.system_message.as_deref())
            })
            .collect();
        assert_eq!(
            messages,
            [Some(COMPACT_WARNING), Some(COMPACT_PARTIAL_WARNING)]
        );
        Ok(())
    }

    #[test]
    fn prompt_reinjects_when_compact_restore_was_missed() -> Result<(), Box<dyn Error>> {
        // A marker stranded before a compact boundary with no reinjection
        // after it (the compact hook failed) must not silence the route.
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let transcript_path = project.path().join("transcript.jsonl");
        fs::write(
            &transcript_path,
            [
                attachment("[prompt-context-route:claudex]"),
                compact_boundary(),
            ]
            .join("\n"),
        )?;
        let input = hook_input(
            "UserPromptSubmit",
            Some("claudex again"),
            Some(&transcript_path),
        );
        let Some(output) = process_hook(&config_path, project.path(), &input) else {
            return Err("prompt after missed restore did not inject context".into());
        };

        assert!(
            context(&output)
                .is_some_and(|context| context.contains("[prompt-context-route:claudex]"))
        );
        Ok(())
    }

    #[test]
    fn compact_restores_routes_injected_before_the_boundary() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let transcript_path = project.path().join("transcript.jsonl");
        fs::write(
            &transcript_path,
            [
                attachment("[prompt-context-route:claudex]"),
                compact_boundary(),
            ]
            .join("\n"),
        )?;
        let input = json!({
            "hook_event_name":"SessionStart",
            "source":"compact",
            "transcript_path":transcript_path
        })
        .to_string();
        let Some(output) = process_hook(&config_path, project.path(), &input) else {
            return Err("compact did not restore pre-boundary context".into());
        };

        assert!(
            context(&output)
                .is_some_and(|context| context.contains("[prompt-context-route:claudex]"))
        );
        Ok(())
    }

    #[test]
    fn invalid_config_or_hook_input_returns_warning_without_context() -> Result<(), Box<dyn Error>>
    {
        let project = test_project()?;
        let invalid_config = write_config(project.path(), "{broken")?;
        let config_warning = process_hook(
            &invalid_config,
            project.path(),
            &hook_input("UserPromptSubmit", Some("claudex"), None),
        );
        let input_warning = process_hook(&invalid_config, project.path(), "{}");

        assert!(
            [config_warning, input_warning].iter().all(|output| output
                .as_ref()
                .is_some_and(|output| output.system_message.is_some()
                    && output.hook_specific_output.is_none()))
        );
        Ok(())
    }
}
