use std::path::Path;

use serde::Serialize;

use crate::config::{ConfigError, Route, canonical_project_root, partition_valid_routes};

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
pub struct ValidatedInjection<'a> {
    pub routes: Vec<&'a Route>,
    pub warnings: Vec<String>,
    pub specific: Option<HookSpecificOutput>,
}

/// Canonicalizes the root and validates `candidates` individually, returning
/// the surviving routes, per-route warnings, and the injection payload shared
/// by the prompt and compact paths.
pub fn validated_injection<'a>(
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

pub fn render_loaded_routes_message(routes: &[&Route]) -> String {
    format!(
        "Prompt context loaded: {}",
        routes
            .iter()
            .map(|route| route.file.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
