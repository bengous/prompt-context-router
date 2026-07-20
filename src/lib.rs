use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use jsonc_parser::{ParseOptions, parse_to_serde_value};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const COMPACT_WARNING: &str =
    "Prompt context could not be restored after compaction: transcript unavailable.";
const COMPACT_PARTIAL_WARNING: &str =
    "Prompt context restore may be incomplete: some transcript lines were malformed.";
const CONTEXT_ATTACHMENT_TYPE: &str = "hook_additional_context";
const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not resolve project root `{path}`: {source}")]
    ProjectRoot {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read config `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSONC in `{path}`: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: jsonc_parser::errors::ParseError,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("could not resolve route file `{path}`: {source}")]
    RouteFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not compile matcher for route `{route_id}` keyword `{keyword}`: {source}")]
    Matcher {
        route_id: String,
        keyword: String,
        #[source]
        source: regex::Error,
    },
}

#[derive(Debug, Error)]
enum HookError {
    #[error("invalid hook input: {0}")]
    Input(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    routes: Vec<RawRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    id: String,
    keywords: Vec<String>,
    file: String,
}

#[derive(Debug)]
pub struct Config {
    routes: Vec<Route>,
}

#[derive(Debug)]
struct Route {
    id: String,
    file: String,
    marker: String,
    matcher: Regex,
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
    system_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput {
    hook_event_name: &'static str,
    additional_context: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TranscriptRouteState {
    /// Routes whose marker appears after the last compact boundary: still in
    /// the live context window, so a matching prompt stays silent.
    live_route_ids: HashSet<String>,
    /// Routes whose marker appears anywhere in the transcript history; the
    /// compact hook restores these into the fresh window.
    known_route_ids: HashSet<String>,
    /// Transcript missing or unreadable: nothing could be recovered.
    unavailable: bool,
    /// Some transcript lines were unparseable: recovery may be incomplete.
    partially_malformed: bool,
}

/// Prompt scans skip the per-line malformed bookkeeping that only the compact
/// warning consumes; the transcript can be megabytes on the blocking path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    Prompt,
    Compact,
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

/// Loads a router configuration and runs its filesystem-free validations.
///
/// Route files are checked on demand ([`validate_route_files`] checks them
/// all) so one broken route cannot disable the others at hook time.
///
/// # Errors
///
/// Returns a precise configuration or matcher error.
pub fn load_config(config_path: &Path) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(config_path).map_err(|source| ConfigError::Read {
        path: config_path.to_path_buf(),
        source,
    })?;
    let raw = parse_config(&text).map_err(|source| ConfigError::Parse {
        path: config_path.to_path_buf(),
        source,
    })?;

    validate_config(raw)
}

/// Validates every route file against `project_root`, for `check`.
///
/// # Errors
///
/// Returns the first filesystem or containment error.
pub fn validate_route_files(config: &Config, project_root: &Path) -> Result<(), ConfigError> {
    let canonical_root = canonical_project_root(project_root)?;
    for route in &config.routes {
        validate_route_file(route, &canonical_root)?;
    }
    Ok(())
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

fn parse_config(text: &str) -> Result<RawConfig, jsonc_parser::errors::ParseError> {
    parse_to_serde_value(
        text,
        &ParseOptions {
            allow_comments: true,
            allow_loose_object_property_names: false,
            allow_trailing_commas: true,
            allow_missing_commas: false,
            allow_single_quoted_strings: false,
            allow_hexadecimal_numbers: false,
            allow_unary_plus_numbers: false,
        },
    )
}

fn validate_config(raw: RawConfig) -> Result<Config, ConfigError> {
    let mut route_ids = HashSet::new();
    let mut routes = Vec::with_capacity(raw.routes.len());

    for (index, raw_route) in raw.routes.into_iter().enumerate() {
        let route_number = index + 1;
        if raw_route.id.is_empty()
            || !raw_route
                .id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(ConfigError::Invalid(format!(
                "route {route_number} id `{}` must match [a-z0-9-]+",
                raw_route.id
            )));
        }
        if !route_ids.insert(raw_route.id.clone()) {
            return Err(ConfigError::Invalid(format!(
                "route {route_number} duplicates id `{}`",
                raw_route.id
            )));
        }
        if raw_route.keywords.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "route {route_number} (`{}`) must contain at least one keyword",
                raw_route.id
            )));
        }

        let mut escaped_keywords = Vec::with_capacity(raw_route.keywords.len());
        for (keyword_index, keyword) in raw_route.keywords.iter().enumerate() {
            let escaped = regex::escape(keyword);
            let single = Regex::new(&format!(r"(?iu)\b{escaped}\b")).map_err(|source| {
                ConfigError::Matcher {
                    route_id: raw_route.id.clone(),
                    keyword: keyword.clone(),
                    source,
                }
            })?;
            // Each keyword must match itself standalone; edge characters
            // outside the regex `\w` class (spaces, punctuation, but also
            // Unicode No-category digits like `²`) invert `\b` anchoring.
            if !single.is_match(keyword) {
                return Err(ConfigError::Invalid(format!(
                    "route {route_number} (`{}`) keyword {} `{keyword}` must start and end with a word character",
                    raw_route.id,
                    keyword_index + 1
                )));
            }
            escaped_keywords.push(escaped);
        }
        // One alternation per route: a single prompt scan instead of one per
        // keyword on the blocking UserPromptSubmit path.
        let pattern = format!(r"(?iu)\b(?:{})\b", escaped_keywords.join("|"));
        let matcher = Regex::new(&pattern).map_err(|source| ConfigError::Matcher {
            route_id: raw_route.id.clone(),
            keyword: raw_route.keywords.join(", "),
            source,
        })?;

        let route_path = Path::new(&raw_route.file);
        if route_path.as_os_str().is_empty()
            || route_path.is_absolute()
            || route_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ConfigError::Invalid(format!(
                "route {route_number} (`{}`) file `{}` must be a relative path without `..`",
                raw_route.id, raw_route.file
            )));
        }

        routes.push(Route {
            marker: route_marker(&raw_route.id),
            id: raw_route.id,
            file: raw_route.file,
            matcher,
        });
    }

    Ok(Config { routes })
}

fn canonical_project_root(project_root: &Path) -> Result<PathBuf, ConfigError> {
    fs::canonicalize(project_root).map_err(|source| ConfigError::ProjectRoot {
        path: project_root.to_path_buf(),
        source,
    })
}

fn validate_route_file(route: &Route, canonical_root: &Path) -> Result<(), ConfigError> {
    let joined_path = canonical_root.join(&route.file);
    let canonical_file =
        fs::canonicalize(&joined_path).map_err(|source| ConfigError::RouteFile {
            path: joined_path,
            source,
        })?;
    if !canonical_file.starts_with(canonical_root) {
        return Err(ConfigError::Invalid(format!(
            "route `{}` file `{}` resolves outside the project",
            route.id, route.file
        )));
    }
    if !canonical_file
        .metadata()
        .map_err(|source| ConfigError::RouteFile {
            path: canonical_file.clone(),
            source,
        })?
        .is_file()
    {
        return Err(ConfigError::Invalid(format!(
            "route `{}` file `{}` is not a regular file",
            route.id, route.file
        )));
    }

    Ok(())
}

fn partition_valid_routes<'a>(
    routes: &[&'a Route],
    canonical_root: &Path,
) -> (Vec<&'a Route>, Vec<String>) {
    let mut valid = Vec::new();
    let mut warnings = Vec::new();
    for route in routes {
        match validate_route_file(route, canonical_root) {
            Ok(()) => valid.push(*route),
            Err(error) => warnings.push(router_warning(error)),
        }
    }
    (valid, warnings)
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

fn detect_routes<'a>(prompt: &str, config: &'a Config) -> Vec<&'a Route> {
    config
        .routes
        .iter()
        .filter(|route| route.matcher.is_match(prompt))
        .collect()
}

fn load_transcript_state(
    transcript_path: Option<&Path>,
    config: &Config,
    mode: ScanMode,
) -> TranscriptRouteState {
    match transcript_path.map(fs::read_to_string) {
        Some(Ok(transcript)) => read_transcript_route_state(&transcript, config, mode),
        Some(Err(_)) | None => TranscriptRouteState {
            unavailable: true,
            ..TranscriptRouteState::default()
        },
    }
}

fn read_transcript_route_state(
    transcript: &str,
    config: &Config,
    mode: ScanMode,
) -> TranscriptRouteState {
    let mut state = TranscriptRouteState::default();

    for line in transcript.lines().filter(|line| !line.trim().is_empty()) {
        // Only attachment entries can hold injected context and only boundary
        // entries reset the live window; for every other line an
        // allocation-free syntax check preserves malformed detection, and only
        // for compact, which alone consumes the flag. A valid JSON document
        // starting with `{` is necessarily an object.
        if !line.contains(CONTEXT_ATTACHMENT_TYPE) && !line.contains(COMPACT_BOUNDARY_SUBTYPE) {
            if mode == ScanMode::Compact
                && (!line.trim_start().starts_with('{')
                    || serde_json::from_str::<serde::de::IgnoredAny>(line).is_err())
            {
                state.partially_malformed = true;
            }
            continue;
        }

        let Ok(entry) = serde_json::from_str::<serde_json::Map<String, Value>>(line) else {
            state.partially_malformed = true;
            continue;
        };

        if entry.get("type").and_then(Value::as_str) == Some("system")
            && entry.get("subtype").and_then(Value::as_str) == Some(COMPACT_BOUNDARY_SUBTYPE)
        {
            // Everything injected before a compaction left the live context
            // window; only markers reinjected after it deduplicate prompts.
            state.live_route_ids.clear();
            continue;
        }

        let Some(content) = entry
            .get("attachment")
            .and_then(Value::as_object)
            .filter(|attachment| {
                attachment.get("type").and_then(Value::as_str) == Some(CONTEXT_ATTACHMENT_TYPE)
            })
            .and_then(|attachment| attachment.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        for text in content.iter().filter_map(Value::as_str) {
            for route in &config.routes {
                if text.contains(&route.marker) {
                    state.live_route_ids.insert(route.id.clone());
                    state.known_route_ids.insert(route.id.clone());
                }
            }
        }
    }

    state
}

fn route_marker(route_id: &str) -> String {
    format!("[prompt-context-route:{route_id}]")
}

fn render_route_context(routes: &[&Route]) -> String {
    routes
        .iter()
        .map(|route| {
            format!(
                "{}\nRead {} before making related claims or changes.",
                route.marker, route.file
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
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const CONFIG: &str = r#"
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

    fn test_project() -> Result<TempDir, Box<dyn Error>> {
        let project = tempfile::tempdir()?;
        fs::create_dir_all(project.path().join(".claude/rules"))?;
        fs::write(project.path().join(".claude/rules/claudex.md"), "# Claudex")?;
        fs::write(project.path().join(".claude/rules/beta.md"), "# Beta")?;
        Ok(project)
    }

    fn write_config(project: &Path, config: &str) -> Result<PathBuf, Box<dyn Error>> {
        let path = project.join(".claude/prompt-context-router.jsonc");
        fs::write(&path, config)?;
        Ok(path)
    }

    fn load_test_config(project: &Path, config: &str) -> Result<Config, Box<dyn Error>> {
        let path = write_config(project, config)?;
        let config = load_config(&path)?;
        validate_route_files(&config, project)?;
        Ok(config)
    }

    fn attachment(context: &str) -> String {
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

    fn compact_boundary() -> String {
        json!({
            "type": "system",
            "subtype": "compact_boundary",
            "content": "Conversation compacted",
            "compactMetadata": {"trigger": "auto"}
        })
        .to_string()
    }

    fn hook_input(event: &str, prompt: Option<&str>, transcript: Option<&Path>) -> String {
        json!({
            "hook_event_name": event,
            "prompt": prompt,
            "transcript_path": transcript
        })
        .to_string()
    }

    fn context(output: &HookOutput) -> Option<&str> {
        output
            .hook_specific_output
            .as_ref()
            .map(|specific| specific.additional_context.as_str())
    }

    #[test]
    fn config_accepts_comments_and_trailing_commas() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = load_test_config(project.path(), CONFIG)?;

        assert_eq!(config.routes.len(), 1);
        Ok(())
    }

    #[test]
    fn config_rejects_invalid_id() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace("claudex\"", "Claude_X\"");
        let error = load_test_config(project.path(), &config).err();

        assert!(error.is_some_and(|error| error.to_string().contains("must match [a-z0-9-]+")));
        Ok(())
    }

    #[test]
    fn config_rejects_duplicate_ids() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = r#"{
          "routes": [
            {"id":"same","keywords":["one"],"file":".claude/rules/claudex.md"},
            {"id":"same","keywords":["two"],"file":".claude/rules/beta.md"}
          ]
        }"#;
        let error = load_test_config(project.path(), config).err();

        assert!(error.is_some_and(|error| error.to_string().contains("duplicates id `same`")));
        Ok(())
    }

    #[test]
    fn config_rejects_empty_keyword_list() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace("[\"claudex\"]", "[]");
        let error = load_test_config(project.path(), &config).err();

        assert!(error.is_some_and(|error| error.to_string().contains("at least one keyword")));
        Ok(())
    }

    #[test]
    fn config_rejects_keyword_without_alphanumeric_edges() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace("[\"claudex\"]", "[\" claudex!\"]");
        let error = load_test_config(project.path(), &config).err();

        assert!(error.is_some_and(|error| error.to_string().contains("must start and end")));
        Ok(())
    }

    #[test]
    fn config_rejects_keyword_with_non_word_alphanumeric_edges() -> Result<(), Box<dyn Error>> {
        // `²` is alphanumeric (Unicode No) but outside regex `\w`, so `\b`
        // anchoring would never match the keyword standalone.
        let project = test_project()?;
        let config = CONFIG.replace("[\"claudex\"]", "[\"c\u{b2}\"]");
        let error = load_test_config(project.path(), &config).err();

        assert!(error.is_some_and(|error| error.to_string().contains("must start and end")));
        Ok(())
    }

    #[test]
    fn config_rejects_parent_path() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace(".claude/rules/claudex.md", "../claudex.md");
        let error = load_test_config(project.path(), &config).err();

        assert!(
            error.is_some_and(|error| error.to_string().contains("relative path without `..`"))
        );
        Ok(())
    }

    #[test]
    fn config_rejects_non_regular_file() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace(".claude/rules/claudex.md", ".claude/rules");
        let error = load_test_config(project.path(), &config).err();

        assert!(error.is_some_and(|error| error.to_string().contains("is not a regular file")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn config_rejects_symlink_resolving_outside_project() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let outside = tempfile::NamedTempFile::new()?;
        symlink(
            outside.path(),
            project.path().join(".claude/rules/outside.md"),
        )?;
        let config = CONFIG.replace("claudex.md", "outside.md");
        let error = load_test_config(project.path(), &config).err();

        assert!(
            error.is_some_and(|error| error.to_string().contains("resolves outside the project"))
        );
        Ok(())
    }

    #[test]
    fn matching_is_unicode_case_insensitive_and_uses_word_boundaries() -> Result<(), Box<dyn Error>>
    {
        let project = test_project()?;
        let config = CONFIG.replace("claudex\"]", "éclair\"]");
        let config = load_test_config(project.path(), &config)?;

        assert_eq!(
            detect_routes("Un ÉCLAIR, pas prééclairci.", &config).len(),
            1
        );
        Ok(())
    }

    #[test]
    fn matching_supports_phrases_and_repeated_occurrences() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace("claudex\"]", "Claude Code\"]");
        let config = load_test_config(project.path(), &config)?;

        assert_eq!(
            detect_routes("claude code puis CLAUDE CODE", &config).len(),
            1
        );
        Ok(())
    }

    #[test]
    fn matching_preserves_config_order_for_multiple_routes() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = r#"{
          "routes": [
            {"id":"beta","keywords":["beta"],"file":".claude/rules/beta.md"},
            {"id":"claudex","keywords":["alpha"],"file":".claude/rules/claudex.md"}
          ]
        }"#;
        let config = load_test_config(project.path(), config)?;
        let matches = detect_routes("alpha then beta", &config);
        let ids: Vec<_> = matches.iter().map(|route| route.id.as_str()).collect();

        assert_eq!(ids, ["beta", "claudex"]);
        Ok(())
    }

    #[test]
    fn transcript_accepts_markers_only_from_structured_context() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = load_test_config(project.path(), CONFIG)?;
        let transcript = [
            json!({"type":"user","message":"[prompt-context-route:claudex]"}).to_string(),
            json!({"type":"tool_result","content":"[prompt-context-route:claudex]"}).to_string(),
            attachment("unrelated context"),
        ]
        .join("\n");
        let state = read_transcript_route_state(&transcript, &config, ScanMode::Compact);

        assert!(state.live_route_ids.is_empty() && state.known_route_ids.is_empty());
        Ok(())
    }

    #[test]
    fn transcript_treats_non_object_lines_as_malformed() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = load_test_config(project.path(), CONFIG)?;

        assert!(
            read_transcript_route_state("null", &config, ScanMode::Compact).partially_malformed
        );
        Ok(())
    }

    #[test]
    fn transcript_flags_valid_json_non_object_lines_as_malformed() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = load_test_config(project.path(), CONFIG)?;
        let transcript = ["[1,2,3]", "\"hook_additional_context\""].join("\n");

        assert!(
            read_transcript_route_state(&transcript, &config, ScanMode::Compact)
                .partially_malformed
        );
        Ok(())
    }

    #[test]
    fn prompt_scan_skips_malformed_bookkeeping() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = load_test_config(project.path(), CONFIG)?;

        assert!(
            !read_transcript_route_state("null", &config, ScanMode::Prompt).partially_malformed
        );
        Ok(())
    }

    #[test]
    fn compact_boundary_resets_live_but_not_known_routes() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = load_test_config(project.path(), CONFIG)?;
        let transcript = [
            attachment("[prompt-context-route:claudex]"),
            compact_boundary(),
        ]
        .join("\n");
        let state = read_transcript_route_state(&transcript, &config, ScanMode::Compact);

        assert!(state.live_route_ids.is_empty());
        assert!(state.known_route_ids.contains("claudex"));
        Ok(())
    }

    #[test]
    fn boundary_detection_requires_a_system_entry() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = load_test_config(project.path(), CONFIG)?;
        let transcript = [
            attachment("[prompt-context-route:claudex]"),
            json!({"type":"user","message":"talking about compact_boundary"}).to_string(),
        ]
        .join("\n");
        let state = read_transcript_route_state(&transcript, &config, ScanMode::Prompt);

        assert!(state.live_route_ids.contains("claudex"));
        Ok(())
    }

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
