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
    Fmt {
        /// Files to process (defaults to entire project)
        paths: Vec<std::path::PathBuf>,
    },
    /// Validate without writing (for CI)
    Check {
        /// Files to check (defaults to entire project)
        paths: Vec<std::path::PathBuf>,
    },
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
    /// List built-in schema presets or print a specific one
    Preset {
        /// Preset name to display (omit to list all)
        name: Option<String>,
    },
    /// Filter documents by type, filename, date, text, links, or frontmatter
    Query {
        /// Files or directories to scan (defaults to current directory)
        paths: Vec<std::path::PathBuf>,
        /// Match docs whose resolved type equals this name
        #[arg(long = "type", value_name = "NAME")]
        type_name: Option<String>,
        /// Match basename against this glob pattern (e.g. `????-??-??T??-??.md`)
        #[arg(long, value_name = "PAT")]
        filename_glob: Option<String>,
        /// Keep only the newest N matches (by filename date, descending)
        #[arg(long, value_name = "N")]
        last: Option<usize>,
        /// Only entries from the last N days (filename date, else frontmatter `date:`)
        #[arg(long, value_name = "N")]
        days: Option<i64>,
        /// Case-insensitive substring match over rendered body text
        #[arg(long, value_name = "PAT")]
        grep: Option<String>,
        /// Keep docs that contain a link whose URL contains this substring
        #[arg(long, value_name = "SUBSTR")]
        has_link: Option<String>,
        /// Frontmatter equality filter, `key=value` (repeatable)
        #[arg(long = "property", value_name = "K=V")]
        property: Vec<String>,
        /// Emit JSONL (same shape as `td json`) instead of default markdown
        #[arg(long)]
        json: bool,
        /// Print the match count only
        #[arg(long)]
        count: bool,
        /// Print matching paths only (one per line)
        #[arg(long = "paths-only")]
        paths_only: bool,
    },
}
