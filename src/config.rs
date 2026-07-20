use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use jsonc_parser::{ParseOptions, parse_to_serde_value};
use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

use crate::routing::router_warning;

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
    note: Option<String>,
}

#[derive(Debug)]
pub struct Config {
    pub(crate) routes: Vec<Route>,
}

#[derive(Debug)]
pub struct Route {
    pub(crate) id: String,
    pub(crate) file: String,
    pub(crate) note: Option<String>,
    pub(crate) marker: String,
    matcher: Regex,
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
        if let Some(note) = &raw_route.note {
            if note.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "route {route_number} (`{}`) note must not be empty or whitespace-only",
                    raw_route.id
                )));
            }
            if note.contains("[prompt-context-route:") {
                return Err(ConfigError::Invalid(format!(
                    "route {route_number} (`{}`) note must not contain `[prompt-context-route:`",
                    raw_route.id
                )));
            }
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
            note: raw_route.note,
            matcher,
        });
    }

    Ok(Config { routes })
}

pub fn canonical_project_root(project_root: &Path) -> Result<PathBuf, ConfigError> {
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

pub fn partition_valid_routes<'a>(
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

pub fn detect_routes<'a>(prompt: &str, config: &'a Config) -> Vec<&'a Route> {
    config
        .routes
        .iter()
        .filter(|route| route.matcher.is_match(prompt))
        .collect()
}

fn route_marker(route_id: &str) -> String {
    format!("[prompt-context-route:{route_id}]")
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::test_support::{CONFIG, load_test_config, test_project};

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
    fn config_rejects_empty_or_whitespace_note() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        for note in ["", "   "] {
            let config = CONFIG.replace(
                "\"file\": \".claude/rules/claudex.md\",",
                &format!(
                    "\"file\": \".claude/rules/claudex.md\",\n              \"note\": \"{note}\","
                ),
            );
            let error = load_test_config(project.path(), &config).err();

            assert!(error.is_some_and(|error| {
                error
                    .to_string()
                    .contains("note must not be empty or whitespace-only")
            }));
        }
        Ok(())
    }

    #[test]
    fn config_rejects_note_containing_marker_prefix() -> Result<(), Box<dyn Error>> {
        let project = test_project()?;
        let config = CONFIG.replace(
            "\"file\": \".claude/rules/claudex.md\",",
            "\"file\": \".claude/rules/claudex.md\",\n              \"note\": \"Never echo [prompt-context-route: here\",",
        );
        let error = load_test_config(project.path(), &config).err();

        assert!(error.is_some_and(|error| {
            error
                .to_string()
                .contains("note must not contain `[prompt-context-route:`")
        }));
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
}
