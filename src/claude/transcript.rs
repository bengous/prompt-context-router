//! Claude Code transcript JSONL handling.
//!
//! This is the crate's only dependency on Claude Code's undocumented
//! transcript schema. The attachment and compact-boundary forms handled here
//! were verified against Claude Code v2.1.215.
//!
//! Scanning is fail-open: missing or unreadable transcripts and malformed or
//! unexpected entries become warnings or silent no-ops through the caller;
//! they never block prompt submission.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::config::Config;

const CONTEXT_ATTACHMENT_TYPE: &str = "hook_additional_context";
const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TranscriptRouteState {
    /// Routes whose marker appears after the last compact boundary: still in
    /// the live context window, so a matching prompt stays silent.
    pub(crate) live_route_ids: HashSet<String>,
    /// Routes whose marker appears anywhere in the transcript history; the
    /// compact hook restores these into the fresh window.
    pub(crate) known_route_ids: HashSet<String>,
    /// Transcript missing or unreadable: nothing could be recovered.
    pub(crate) unavailable: bool,
    /// Some transcript lines were unparseable: recovery may be incomplete.
    pub(crate) partially_malformed: bool,
}

/// Prompt scans skip the per-line malformed bookkeeping that only the compact
/// warning consumes; the transcript can be megabytes on the blocking path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Prompt,
    Compact,
}

pub fn load_transcript_state(
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

#[cfg(test)]
mod tests {
    use std::error::Error;

    use serde_json::json;

    use super::*;
    use crate::test_support::{
        CONFIG, attachment, compact_boundary, load_test_config, test_project,
    };

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
}
