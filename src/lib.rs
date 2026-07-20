//! Routes Claude Code prompt keywords to project context files.

mod claude;
mod codex;
mod config;
mod routing;

#[cfg(test)]
pub(crate) mod test_support;

pub use claude::process_hook;
pub use codex::{CodexHookResult, process_codex_hook};
pub use config::{Config, ConfigError, load_config, validate_route_files};
pub use routing::{HookOutput, router_warning};
