//! CLI definitions for the `td` binary.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "td", about = "Schema-driven markdown toolkit", version)]
pub struct Cli {
    /// Enable debug logging (schema loading, type resolution, per-file decisions)
    #[arg(long)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Validate, fix, and format markdown files
    Fmt,
    /// Validate without writing (for CI)
    Check,
    /// Start Language Server Protocol server (diagnostics for markdown files)
    Lsp,
    /// Output documents as structured JSON (JSONL for multiple files)
    Json {
        /// Files or directories to process (defaults to current directory)
        paths: Vec<std::path::PathBuf>,
        /// Pretty-print JSON output
        #[arg(long)]
        pretty: bool,
        /// Follow local markdown links N hops deep (0 = seed files only)
        #[arg(long, default_value_t = 0)]
        depth: u32,
    },
}
