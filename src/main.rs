mod ast;
mod cli;
mod fix;
mod format;
mod git;
mod lsp;
mod parse;
mod schema;
mod validate;

use std::path::PathBuf;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    match cli.command {
        cli::Command::Fmt => {
            let opts = format::FormatOptions { check: false };
            match format::format_dir(&root, opts) {
                Ok(result) => {
                    if result.files_changed > 0 {
                        eprintln!(
                            "formatted {} of {} file(s)",
                            result.files_changed, result.files_checked
                        );
                    }
                    for err in &result.errors {
                        eprintln!("{}:", err.path.display());
                        for d in &err.diagnostics {
                            eprintln!("  error: {}", d.message());
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

        cli::Command::Check => match format::check_dir(&root) {
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
        },

        cli::Command::Init => {
            let schema_dir = root.join(".typedown");
            if schema_dir.exists() {
                eprintln!(".typedown/ already exists");
                std::process::exit(1);
            }
            if let Err(e) = std::fs::create_dir(&schema_dir) {
                eprintln!("error creating .typedown/: {e}");
                std::process::exit(1);
            }
            eprintln!("created .typedown/");
            eprintln!("add YAML files to define document types, e.g. .typedown/note.yaml");
        }

        cli::Command::Lsp => {
            if let Err(e) = lsp::run() {
                eprintln!("LSP error: {e}");
                std::process::exit(1);
            }
        }
    }
}
