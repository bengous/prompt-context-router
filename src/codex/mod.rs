pub mod state;

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use self::state::{LoadedState, load_state, save_state};
use crate::config::{Config, ConfigError, Route, detect_routes, load_config};
use crate::routing::{
    HookOutput, HookSpecificOutput, render_loaded_routes_message, router_warning,
    validated_injection,
};

const STATE_RESET_MESSAGE: &str = "Codex session state was reset.";

#[derive(Debug, Deserialize)]
struct EventEnvelope {
    hook_event_name: String,
}

#[derive(Debug, Deserialize)]
struct UserPromptSubmitInput {
    #[serde(rename = "hook_event_name")]
    _hook_event_name: String,
    session_id: String,
    prompt: String,
    #[serde(rename = "cwd")]
    _cwd: PathBuf,
    #[serde(rename = "transcript_path")]
    _transcript_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct PostCompactInput {
    #[serde(rename = "hook_event_name")]
    _hook_event_name: String,
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct SessionStartInput {
    #[serde(rename = "hook_event_name")]
    _hook_event_name: String,
    session_id: String,
    source: String,
}

#[derive(Debug, Error)]
enum CodexHookError {
    #[error("invalid hook input: {0}")]
    Input(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// One processed Codex hook event whose state update is not yet persisted.
///
/// Call [`Self::commit`] only after any output has been serialized and flushed.
#[derive(Debug)]
#[must_use = "Codex hook state must be committed after output is flushed"]
pub struct CodexHookResult {
    output: Option<HookOutput>,
    loaded_state: Option<LoadedState>,
    should_persist: bool,
}

impl CodexHookResult {
    const fn output_only(output: Option<HookOutput>) -> Self {
        Self {
            output,
            loaded_state: None,
            should_persist: false,
        }
    }

    const fn with_state(
        output: Option<HookOutput>,
        loaded_state: LoadedState,
        changed: bool,
    ) -> Self {
        Self {
            output,
            loaded_state: Some(loaded_state),
            should_persist: changed,
        }
    }

    /// Returns the optional hook output without committing the state update.
    #[must_use]
    pub const fn output(&self) -> Option<&HookOutput> {
        self.output.as_ref()
    }

    /// Persists the state update, if any, and releases the session lock.
    pub fn commit(mut self) {
        if !self.should_persist {
            return;
        }
        if let Some(loaded_state) = self.loaded_state.take()
            && !save_state(loaded_state)
        {
            eprintln!("prompt context router could not save Codex session state");
        }
    }
}

/// Processes one Codex hook input without persisting its state update.
///
/// The caller must serialize and flush [`CodexHookResult::output`] before
/// calling [`CodexHookResult::commit`]. Input, configuration, and state
/// failures remain non-blocking.
pub fn process_codex_hook(
    config_path: &Path,
    project_root: &Path,
    cache_root: &Path,
    input: &str,
) -> CodexHookResult {
    match process_codex_hook_inner(config_path, project_root, cache_root, input) {
        Ok(result) => result,
        Err(error) => {
            CodexHookResult::output_only(Some(HookOutput::warning(router_warning(error))))
        }
    }
}

fn process_codex_hook_inner(
    config_path: &Path,
    project_root: &Path,
    cache_root: &Path,
    input: &str,
) -> Result<CodexHookResult, CodexHookError> {
    let envelope: EventEnvelope = parse_input(input)?;
    match envelope.hook_event_name.as_str() {
        "UserPromptSubmit" => {
            let input: UserPromptSubmitInput = parse_input(input)?;
            let config = load_config(config_path)?;
            output_for_prompt(&input, &config, project_root, cache_root)
        }
        "PostCompact" => {
            let input: PostCompactInput = parse_input(input)?;
            load_config(config_path)?;
            Ok(output_for_post_compact(
                &input.session_id,
                project_root,
                cache_root,
            ))
        }
        "SessionStart" => {
            let input: SessionStartInput = parse_input(input)?;
            match input.source.as_str() {
                "compact" | "resume" | "clear" => {
                    let config = load_config(config_path)?;
                    output_for_session_start(&input, &config, project_root, cache_root)
                }
                _ => Ok(CodexHookResult::output_only(None)),
            }
        }
        _ => Ok(CodexHookResult::output_only(None)),
    }
}

fn parse_input<T>(input: &str) -> Result<T, CodexHookError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(input).map_err(|error| CodexHookError::Input(error.to_string()))
}

fn output_for_prompt(
    input: &UserPromptSubmitInput,
    config: &Config,
    project_root: &Path,
    cache_root: &Path,
) -> Result<CodexHookResult, CodexHookError> {
    let mut loaded = load_state(cache_root, project_root, &input.session_id);
    let mut messages = reset_messages(&loaded);
    let matching_routes = detect_routes(&input.prompt, config);
    let candidates: Vec<_> = matching_routes
        .into_iter()
        .filter(|route| !loaded.state.live_route_ids.contains(&route.id))
        .collect();
    let injection = validated_injection("UserPromptSubmit", &candidates, project_root)?;

    if !injection.routes.is_empty() {
        messages.insert(0, render_loaded_routes_message(&injection.routes));
    }
    messages.extend(injection.warnings);

    let mut changed = loaded.was_reset;
    for route in &injection.routes {
        changed |= loaded.state.known_route_ids.insert(route.id.clone());
        changed |= loaded.state.live_route_ids.insert(route.id.clone());
    }
    let output = assemble_output(&messages, injection.specific);
    Ok(CodexHookResult::with_state(output, loaded, changed))
}

fn output_for_post_compact(
    session_id: &str,
    project_root: &Path,
    cache_root: &Path,
) -> CodexHookResult {
    let mut loaded = load_state(cache_root, project_root, session_id);
    let messages = reset_messages(&loaded);
    let changed = loaded.was_reset || !loaded.state.live_route_ids.is_empty();
    loaded.state.live_route_ids.clear();
    CodexHookResult::with_state(assemble_output(&messages, None), loaded, changed)
}

fn output_for_session_start(
    input: &SessionStartInput,
    config: &Config,
    project_root: &Path,
    cache_root: &Path,
) -> Result<CodexHookResult, CodexHookError> {
    let mut loaded = load_state(cache_root, project_root, &input.session_id);
    let messages = reset_messages(&loaded);

    match input.source.as_str() {
        "clear" => {
            let changed = loaded.was_reset
                || !loaded.state.known_route_ids.is_empty()
                || !loaded.state.live_route_ids.is_empty();
            loaded.state.known_route_ids.clear();
            loaded.state.live_route_ids.clear();
            Ok(CodexHookResult::with_state(
                assemble_output(&messages, None),
                loaded,
                changed,
            ))
        }
        "resume"
            if !loaded.state.live_route_ids.is_empty()
                || loaded.state.known_route_ids.is_empty() =>
        {
            let changed = loaded.was_reset;
            Ok(CodexHookResult::with_state(
                assemble_output(&messages, None),
                loaded,
                changed,
            ))
        }
        "compact" | "resume" => restore_known_routes(config, project_root, loaded, messages),
        _ => Ok(CodexHookResult::output_only(None)),
    }
}

fn restore_known_routes(
    config: &Config,
    project_root: &Path,
    mut loaded: LoadedState,
    mut messages: Vec<String>,
) -> Result<CodexHookResult, CodexHookError> {
    let candidates: Vec<&Route> = config
        .routes
        .iter()
        .filter(|route| loaded.state.known_route_ids.contains(&route.id))
        .collect();
    let injection = validated_injection("SessionStart", &candidates, project_root)?;
    messages.extend(injection.warnings);

    let emitted_ids = injection
        .routes
        .iter()
        .map(|route| route.id.clone())
        .collect();
    let changed = loaded.was_reset || loaded.state.live_route_ids != emitted_ids;
    loaded.state.live_route_ids = emitted_ids;

    Ok(CodexHookResult::with_state(
        assemble_output(&messages, injection.specific),
        loaded,
        changed,
    ))
}

fn reset_messages(loaded: &LoadedState) -> Vec<String> {
    if loaded.was_reset {
        vec![router_warning(STATE_RESET_MESSAGE)]
    } else {
        Vec::new()
    }
}

fn assemble_output(
    messages: &[String],
    specific: Option<HookSpecificOutput>,
) -> Option<HookOutput> {
    if messages.is_empty() && specific.is_none() {
        return None;
    }
    Some(HookOutput {
        system_message: (!messages.is_empty()).then(|| messages.join("\n")),
        hook_specific_output: specific,
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};

    use super::*;
    use crate::test_support::{CONFIG, test_project, write_config};

    fn prompt_input(project_root: &Path, session_id: &str, prompt: &str) -> String {
        json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": session_id,
            "prompt": prompt,
            "cwd": project_root,
            "transcript_path": "/unused/codex-rollout.jsonl",
            "future_codex_field": true
        })
        .to_string()
    }

    fn event_input(event: &str, session_id: &str, source: Option<&str>) -> String {
        json!({
            "hook_event_name": event,
            "session_id": session_id,
            "source": source,
            "trigger": "auto",
            "future_codex_field": true
        })
        .to_string()
    }

    fn process(
        config_path: &Path,
        project_root: &Path,
        cache_root: &Path,
        input: &str,
    ) -> Result<Option<Value>, Box<dyn Error>> {
        let result = process_codex_hook(config_path, project_root, cache_root, input);
        let output = result.output().map(serde_json::to_value).transpose()?;
        result.commit();
        Ok(output)
    }

    fn saved_state_file(cache_root: &Path, session_id: &str) -> Result<PathBuf, Box<dyn Error>> {
        let namespace = fs::read_dir(cache_root)?
            .next()
            .transpose()?
            .ok_or("cache namespace was not created")?;
        Ok(namespace.path().join(format!("{session_id}.json")))
    }

    #[test]
    fn prompt_match_injects_camel_case_output_and_system_message() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let input = prompt_input(project.path(), "prompt-match", "Check CLAUDEX");

        let output = process(&config_path, project.path(), cache.path(), &input)?;

        assert_eq!(
            output,
            Some(json!({
                "systemMessage": "Prompt context loaded: .claude/rules/claudex.md",
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": concat!(
                        "[prompt-context-route:claudex]\n",
                        "Read .claude/rules/claudex.md before making related claims or changes."
                    )
                }
            }))
        );
        Ok(())
    }

    #[test]
    fn second_matching_prompt_in_same_session_is_silent() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let input = prompt_input(project.path(), "dedup", "claudex");

        if process(&config_path, project.path(), cache.path(), &input)?.is_none() {
            return Err("first prompt did not inject context".into());
        }
        assert_eq!(
            process(&config_path, project.path(), cache.path(), &input)?,
            None
        );
        Ok(())
    }

    #[test]
    fn post_compact_clears_live_and_keeps_known_routes() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let prompt = prompt_input(project.path(), "post-compact", "claudex");
        let post_compact = event_input("PostCompact", "post-compact", None);
        let compact_start = event_input("SessionStart", "post-compact", Some("compact"));

        process(&config_path, project.path(), cache.path(), &prompt)?;
        assert_eq!(
            process(&config_path, project.path(), cache.path(), &post_compact)?,
            None
        );
        let Some(restored) = process(&config_path, project.path(), cache.path(), &compact_start)?
        else {
            return Err("known route was not restored after compaction".into());
        };

        assert_eq!(
            restored["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        Ok(())
    }

    #[test]
    fn compact_session_start_restores_without_system_message() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let mut loaded = load_state(cache.path(), project.path(), "compact-restore");
        loaded.state.known_route_ids.insert("claudex".to_owned());
        if !save_state(loaded) {
            return Err("could not seed Codex state".into());
        }
        let input = event_input("SessionStart", "compact-restore", Some("compact"));

        let Some(output) = process(&config_path, project.path(), cache.path(), &input)? else {
            return Err("compact start did not restore known context".into());
        };

        assert!(output.get("systemMessage").is_none());
        assert_eq!(
            output["hookSpecificOutput"]["additionalContext"],
            concat!(
                "[prompt-context-route:claudex]\n",
                "Read .claude/rules/claudex.md before making related claims or changes."
            )
        );
        Ok(())
    }

    #[test]
    fn clear_session_start_resets_known_and_live_routes() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let prompt = prompt_input(project.path(), "clear", "claudex");
        let clear = event_input("SessionStart", "clear", Some("clear"));
        let compact = event_input("SessionStart", "clear", Some("compact"));

        process(&config_path, project.path(), cache.path(), &prompt)?;
        assert_eq!(
            process(&config_path, project.path(), cache.path(), &clear)?,
            None
        );
        assert_eq!(
            process(&config_path, project.path(), cache.path(), &compact)?,
            None
        );
        Ok(())
    }

    #[test]
    fn resume_restores_only_when_live_routes_are_empty() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let prompt = prompt_input(project.path(), "resume", "claudex");
        let resume = event_input("SessionStart", "resume", Some("resume"));
        let post_compact = event_input("PostCompact", "resume", None);

        process(&config_path, project.path(), cache.path(), &prompt)?;
        assert_eq!(
            process(&config_path, project.path(), cache.path(), &resume)?,
            None
        );
        process(&config_path, project.path(), cache.path(), &post_compact)?;
        let Some(restored) = process(&config_path, project.path(), cache.path(), &resume)? else {
            return Err("resume did not restore known routes with an empty live set".into());
        };

        assert!(restored.get("systemMessage").is_none());
        assert_eq!(
            restored["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        Ok(())
    }

    #[test]
    fn corrupted_state_still_injects_and_warns_that_state_was_reset() -> Result<(), Box<dyn Error>>
    {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let input = prompt_input(project.path(), "corrupt", "claudex");

        process(&config_path, project.path(), cache.path(), &input)?;
        fs::write(saved_state_file(cache.path(), "corrupt")?, "not json")?;
        let Some(output) = process(&config_path, project.path(), cache.path(), &input)? else {
            return Err("prompt did not inject after corrupted state".into());
        };

        assert!(
            output["systemMessage"]
                .as_str()
                .is_some_and(|message| message.contains("session state was reset"))
        );
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        Ok(())
    }

    #[test]
    fn unknown_hook_event_is_silent() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let cache = tempfile::tempdir()?;
        let config_path = write_config(project.path(), CONFIG)?;
        let input = json!({"hook_event_name": "FutureCodexEvent"}).to_string();

        assert_eq!(
            process(&config_path, project.path(), cache.path(), &input)?,
            None
        );
        Ok(())
    }
}
