//! CLI definitions for the `td` binary.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "td", about = "Schema-driven markdown toolkit")]
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
}
