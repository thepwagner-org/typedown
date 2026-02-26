//! CLI definitions for the `td` binary.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "td", about = "Schema-driven markdown toolkit")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Validate, fix, and format markdown files
    Fmt,
    /// Validate without writing (for CI)
    Check,
    /// Scaffold a .typedown/ schema directory
    Init,
    /// Start Language Server Protocol server (diagnostics for markdown files)
    Lsp,
}
