//! LSP server for typedown markdown diagnostics.
//!
//! Runs the same validation as `td check` on file open/change and publishes
//! errors as LSP diagnostics. Designed for use with opencode, which sends
//! `didOpen` and `didChange` (not `didSave`) with content read from disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Notification as LspNotification};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, InitializeParams, Position, PublishDiagnosticsParams, Range,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

use crate::format;

/// Start the LSP server on stdio.
pub fn run() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    };

    let caps_value =
        serde_json::to_value(capabilities).context("failed to serialize server capabilities")?;
    let init_value = connection
        .initialize(caps_value)
        .map_err(|e| anyhow::anyhow!("LSP initialize failed: {e}"))?;
    let _init_params: InitializeParams = serde_json::from_value(init_value)?;

    main_loop(&connection)?;
    io_threads
        .join()
        .map_err(|e| anyhow::anyhow!("IO thread error: {e}"))?;

    Ok(())
}

/// Process messages until shutdown.
fn main_loop(connection: &Connection) -> Result<()> {
    // Track files with published diagnostics so we can clear stale ones.
    let mut published_files: HashSet<PathBuf> = HashSet::new();

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection
                    .handle_shutdown(&req)
                    .map_err(|e| anyhow::anyhow!("shutdown error: {e}"))?
                {
                    return Ok(());
                }
            }
            Message::Notification(not) => {
                handle_notification(connection, &not, &mut published_files);
            }
            Message::Response(_) => {}
        }
    }

    Ok(())
}

/// Dispatch incoming notifications.
fn handle_notification(
    connection: &Connection,
    not: &LspNotification,
    published_files: &mut HashSet<PathBuf>,
) {
    match not.method.as_str() {
        "textDocument/didOpen" | "textDocument/didChange" => {
            if let Some(path) = extract_file_path(not) {
                if path.extension().is_some_and(|e| e == "md") {
                    if let Err(e) = validate_and_publish(connection, &path, published_files) {
                        eprintln!("validation failed for {}: {e:#}", path.display());
                    }
                }
            }
        }
        _ => {}
    }
}

/// Run project validation and publish diagnostics for all files.
fn validate_and_publish(
    connection: &Connection,
    file_path: &Path,
    published_files: &mut HashSet<PathBuf>,
) -> Result<()> {
    let root = format::find_project_root(file_path).context("could not determine project root")?;

    let file_errors = format::check_dir(&root, &[])?;

    // Publish diagnostics for files with errors.
    let mut new_published: HashSet<PathBuf> = HashSet::new();

    for file_error in &file_errors {
        let diagnostics: Vec<Diagnostic> = file_error
            .diagnostics
            .iter()
            .map(|d| {
                // typedown lines are 1-based (None = document-level); LSP is 0-based.
                let line = d.line().map(|l| l.saturating_sub(1)).unwrap_or(0) as u32;
                Diagnostic {
                    range: Range {
                        start: Position { line, character: 0 },
                        end: Position { line, character: 0 },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("typedown".to_string()),
                    message: d.message(),
                    ..Default::default()
                }
            })
            .collect();

        publish_diagnostics(connection, &file_error.path, diagnostics)?;
        new_published.insert(file_error.path.clone());
    }

    // Clear diagnostics for files that previously had errors but are now clean.
    for old_path in published_files.difference(&new_published) {
        publish_diagnostics(connection, old_path, vec![])?;
    }

    *published_files = new_published;
    Ok(())
}

/// Pull the file URI out of a didOpen or didChange notification.
fn extract_file_path(not: &LspNotification) -> Option<PathBuf> {
    let uri_str = not
        .params
        .as_object()?
        .get("textDocument")?
        .as_object()?
        .get("uri")?
        .as_str()?;
    uri_to_path(uri_str)
}

/// Convert a `file://` URI to a filesystem path.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let parsed: Uri = uri.parse().ok()?;
    let scheme = parsed.scheme()?.as_str();
    if scheme != "file" {
        return None;
    }
    // Percent-decode the path component.
    let path_str = parsed.path().as_str();
    let decoded = percent_decode(path_str);
    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded))
}

/// Convert a filesystem path to a `file://` URI.
fn path_to_uri(path: &Path) -> Result<Uri> {
    let abs = if path.is_absolute() {
        path.to_string_lossy().to_string()
    } else {
        return Err(anyhow::anyhow!("path must be absolute: {}", path.display()));
    };
    format!("file://{abs}")
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid URI for {}: {e}", path.display()))
}

/// Decode percent-encoded bytes in a URI path.
fn percent_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                result.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(result).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Send a publishDiagnostics notification.
fn publish_diagnostics(
    connection: &Connection,
    path: &Path,
    diagnostics: Vec<Diagnostic>,
) -> Result<()> {
    let params = PublishDiagnosticsParams {
        uri: path_to_uri(path)?,
        diagnostics,
        version: None,
    };
    connection
        .sender
        .send(Message::Notification(LspNotification::new(
            "textDocument/publishDiagnostics".to_string(),
            params,
        )))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_to_path_unix() {
        let path = uri_to_path("file:///Users/test/doc.md");
        assert_eq!(path, Some(PathBuf::from("/Users/test/doc.md")));
    }

    #[test]
    fn test_uri_to_path_percent_encoded() {
        let path = uri_to_path("file:///Users/test/my%20doc.md");
        assert_eq!(path, Some(PathBuf::from("/Users/test/my doc.md")));
    }

    #[test]
    fn test_uri_to_path_invalid() {
        assert_eq!(uri_to_path("not-a-uri"), None);
        assert_eq!(uri_to_path("https://example.com"), None);
    }

    #[test]
    fn test_path_to_uri_roundtrip() {
        let original = PathBuf::from("/Users/test/doc.md");
        let uri = path_to_uri(&original).unwrap();
        let recovered = uri_to_path(uri.as_str()).unwrap();
        assert_eq!(original, recovered);
    }
}
