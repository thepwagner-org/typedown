mod ast;
mod cli;
mod fix;
mod format;
mod git;
mod json;
mod lsp;
mod parse;
mod schema;
mod validate;

use std::path::{Path, PathBuf};

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Resolve CLI path arguments to absolute paths relative to cwd.
fn resolve_paths(cwd: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                cwd.join(p)
            }
        })
        .collect()
}

fn main() {
    let cli = cli::Cli::parse();

    let filter = if cli.debug {
        EnvFilter::new("td=debug")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = format::find_project_root(&cwd).unwrap_or_else(|| cwd.clone());

    match cli.command {
        cli::Command::Fmt { paths } => {
            let opts = format::FormatOptions { check: false };
            match format::format_dir(&root, &resolve_paths(&cwd, &paths), opts) {
                Ok(result) => {
                    if result.files_changed > 0 {
                        eprintln!(
                            "formatted {} of {} file(s)",
                            result.files_changed, result.files_checked
                        );
                    }
                    for err in &result.errors {
                        for d in &err.diagnostics {
                            let line = d.line().map(|l| format!(":{l}")).unwrap_or_default();
                            eprintln!("{}{}:  {}", err.path.display(), line, d.message());
                        }
                    }
                    if !result.errors.is_empty() {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }

        cli::Command::Check { paths } => {
            match format::check_dir(&root, &resolve_paths(&cwd, &paths)) {
                Ok(errors) => {
                    for file_err in &errors {
                        for d in &file_err.diagnostics {
                            let line = d.line().map(|l| format!(":{l}")).unwrap_or_default();
                            eprintln!("{}{}:  {}", file_err.path.display(), line, d.message());
                        }
                    }
                    if !errors.is_empty() {
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }

        cli::Command::Lsp => {
            if let Err(e) = lsp::run() {
                eprintln!("LSP error: {e}");
                std::process::exit(1);
            }
        }

        cli::Command::Json {
            paths,
            pretty,
            depth,
        } => {
            if let Err(e) = json::json_output(&root, &cwd, &paths, pretty, depth) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}
