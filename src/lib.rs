//! Routes Claude Code prompt keywords to project context files.

mod config;
mod hook;
mod transcript;

#[cfg(test)]
pub(crate) mod test_support;

pub use config::{Config, ConfigError, load_config, validate_route_files};
pub use hook::{HookOutput, process_hook, router_warning};
