use std::env;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use prompt_context_router::{
    ConfigError, HookOutput, load_config, process_codex_hook, process_hook, router_warning,
    validate_route_files,
};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(name = "prompt-context-router", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Hook {
        #[arg(long)]
        config: PathBuf,
    },
    CodexHook {
        #[arg(long)]
        project_root: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
    Check {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Debug, Error)]
enum MainError {
    #[error("could not determine project root: {0}")]
    ProjectRoot(#[source] io::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check { config } => match check(&config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Command::Hook { config } => {
            run_hook(&config);
            ExitCode::SUCCESS
        }
        Command::CodexHook {
            project_root,
            config,
        } => {
            run_codex_hook(&config, &project_root);
            ExitCode::SUCCESS
        }
    }
}

fn run_codex_hook(config: &Path, project_root: &Path) {
    let mut input = String::new();
    let result = match io::stdin().read_to_string(&mut input) {
        Ok(_) => process_codex_hook(config, project_root, &codex_cache_root(), &input),
        Err(error) => {
            let output = HookOutput::warning(router_warning(format_args!(
                "could not read hook input: {error}"
            )));
            let _ = write_codex_output(&output);
            return;
        }
    };

    let output_written = result.output().is_none_or(write_codex_output);
    if output_written {
        result.commit();
    }
}

fn write_codex_output(output: &HookOutput) -> bool {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut stdout, output) {
        eprintln!("prompt context router could not write hook output: {error}");
        return false;
    }
    if let Err(error) = stdout.flush() {
        eprintln!("prompt context router could not flush hook output: {error}");
        return false;
    }
    true
}

fn codex_cache_root() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("prompt-context-router")
}

fn check(config: &Path) -> Result<(), MainError> {
    let root = project_root().map_err(MainError::ProjectRoot)?;
    let config = load_config(config)?;
    validate_route_files(&config, &root)?;
    Ok(())
}

fn run_hook(config: &Path) {
    let mut input = String::new();
    let output = match io::stdin().read_to_string(&mut input) {
        Ok(_) => match project_root() {
            Ok(root) => process_hook(config, &root, &input),
            Err(error) => Some(HookOutput::warning(router_warning(format_args!(
                "could not determine project root: {error}"
            )))),
        },
        Err(error) => Some(HookOutput::warning(router_warning(format_args!(
            "could not read hook input: {error}"
        )))),
    };

    if let Some(output) = output {
        let stdout = io::stdout();
        if let Err(error) = serde_json::to_writer(stdout.lock(), &output) {
            eprintln!("prompt context router could not write hook output: {error}");
        }
    }
}

fn project_root() -> io::Result<PathBuf> {
    env::var_os("CLAUDE_PROJECT_DIR")
        .filter(|path| !path.is_empty())
        .map_or_else(env::current_dir, |path| Ok(PathBuf::from(path)))
}
