//! `td query` -- filter and list structured documents.
//!
//! Complements `td json` (full structured output) and `td check` (validation)
//! with a focused filter surface for "show me N docs matching X":
//!
//! - `--type NAME`       keep docs whose resolved type matches
//! - `--filename-glob P` filter by basename glob (e.g. `????-??-??T??-??.md`)
//! - `--last N`          keep the newest N matches (by filename date, desc)
//! - `--days N`          only entries from the last N days
//! - `--grep PAT`        case-insensitive substring over rendered body
//! - `--has-link SUBSTR` match against link URLs
//! - `--property k=v`    frontmatter equality (repeatable)
//! - `--json`            emit JSONL (same shape as `td json`)
//! - `--count`           print match count only
//! - `--path-only`       print matching paths only
//!
//! Fast path: candidates are sorted newest-first by filename-embedded date
//! before any file is read, so `--last 1` on a 1000-entry journal still only
//! reads one file.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{
    ast::{inlines_to_string, Block, Document, Inline, ListItem},
    format::{
        find_schema_for, is_markdown, load_all_schemas, resolve_type, walk, FormatResult,
        ResolvedType,
    },
    json::document_to_json,
    parse::{parse, serialize_blocks},
    schema::{PathMatcher, Schema, TypeDef},
};
use anyhow::{bail, Result};
use chrono::{Local, NaiveDate};
use globset::{Glob, GlobMatcher};

// ── Public API ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct QueryOptions {
    pub type_name: Option<String>,
    pub filename_glob: Option<String>,
    pub last: Option<usize>,
    pub days: Option<i64>,
    pub grep: Option<String>,
    pub has_link: Option<String>,
    pub properties: Vec<(String, String)>,
    pub json: bool,
    pub count: bool,
    pub path_only: bool,
}

/// Parse `k=v` strings collected from `--property` flags.
pub fn parse_property_flags(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| anyhow::anyhow!("--property expects 'key=value', got '{s}'"))
        })
        .collect()
}

/// Run a query against the project tree.
///
/// - `root`: project root (where schemas anchor).
/// - `cwd`: used as the default target when `paths` is empty.
/// - `paths`: optional list of files/directories to scan.
pub fn query_output(root: &Path, cwd: &Path, paths: &[PathBuf], opts: QueryOptions) -> Result<()> {
    // Load schemas & path matchers (same machinery as `td json`).
    let mut dummy = FormatResult::default();
    let (schemas, matchers) = load_all_schemas(root, &mut dummy);
    for err in &dummy.errors {
        eprintln!(
            "warning: {}: {}",
            err.path.display(),
            err.diagnostics[0].message()
        );
    }

    // Compile --filename-glob once.
    let filename_matcher: Option<GlobMatcher> = match &opts.filename_glob {
        Some(pat) => {
            let glob = Glob::new(pat)
                .map_err(|e| anyhow::anyhow!("invalid --filename-glob '{pat}': {e}"))?;
            Some(glob.compile_matcher())
        }
        None => None,
    };

    // Verify --type is known (error early with a helpful list).
    if let Some(ref tname) = opts.type_name {
        let mut known = std::collections::BTreeSet::new();
        for schema in schemas.values() {
            for name in schema.types.keys() {
                known.insert(name.clone());
            }
        }
        if !known.contains(tname) {
            let list: Vec<String> = known.into_iter().collect();
            bail!("unknown --type '{tname}'; known types: {}", list.join(", "));
        }
    }

    // Compute target roots (default to cwd).
    let targets: Vec<PathBuf> = if paths.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
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
    };

    // Discover candidates (no file reads yet).
    let mut candidates: Vec<Candidate> = Vec::new();
    for target in &targets {
        collect_candidates(target, root, &schemas, &matchers, &mut candidates);
    }

    // Cheap pre-filters using only path / filename metadata.
    let day_cutoff = opts
        .days
        .map(|n| Local::now().date_naive() - chrono::Duration::days(n));

    candidates.retain(|c| {
        // --filename-glob against basename
        if let Some(m) = &filename_matcher {
            if !m.is_match(&c.basename) {
                return false;
            }
        }
        // --type: if the type has path patterns AND this file has a path_match,
        // we can reject mismatches without reading. Files with no path_match are
        // kept for a content-phase frontmatter check.
        if let Some(wanted) = &opts.type_name {
            if let Some(matched) = &c.path_match_type {
                if matched != wanted {
                    return false;
                }
            }
        }
        // --days by filename-embedded date (fast). Files without a filename
        // date are kept for the content phase (frontmatter date check).
        if let (Some(cutoff), Some(d)) = (day_cutoff, c.file_date) {
            if d < cutoff {
                return false;
            }
        }
        true
    });

    // Sort newest-first: filename date desc, then basename desc.
    candidates.sort_by(|a, b| match (b.file_date, a.file_date) {
        (Some(bd), Some(ad)) => bd.cmp(&ad).then_with(|| b.basename.cmp(&a.basename)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.basename.cmp(&a.basename),
    });

    // Stream through candidates, reading files lazily, stopping at --last N.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut emitted = 0usize;
    let mut first = true;
    let limit = opts.last.unwrap_or(usize::MAX);

    for cand in &candidates {
        if emitted >= limit {
            break;
        }

        // Fast-path path-only + count-only when no content filters and type
        // filter is satisfied by path-match alone (or no type filter).
        let needs_content = opts.grep.is_some()
            || opts.has_link.is_some()
            || !opts.properties.is_empty()
            || day_cutoff
                .map(|_| cand.file_date.is_none())
                .unwrap_or(false)
            || opts
                .type_name
                .as_ref()
                .map(|_| cand.path_match_type.is_none())
                .unwrap_or(false);

        let mut content_cache: Option<(String, Document, Option<String>)> = None;
        if needs_content {
            let Ok(content) = std::fs::read_to_string(&cand.abs_path) else {
                continue;
            };
            let doc = parse(&content);
            let resolved =
                find_schema_for(&cand.abs_path, root, &schemas).map(|(schema, schema_dir)| {
                    resolve_type(&cand.abs_path, &doc, schema, &schema_dir, &matchers)
                });
            let resolved_type_name: Option<String> = resolved.as_ref().and_then(|r| match r {
                ResolvedType::Explicit(n, _) | ResolvedType::PathMatched(n, _) => Some(n.clone()),
                _ => None,
            });

            if !apply_content_filters(&opts, &content, &doc, resolved_type_name.as_deref(), cand) {
                continue;
            }
            content_cache = Some((content, doc, resolved_type_name));
        }

        emitted += 1;
        if opts.count {
            continue;
        }

        if opts.path_only {
            writeln!(out, "{}", cand.rel_path.display())?;
            continue;
        }

        // Ensure we have content for output (json / default body).
        let (content, doc, resolved_type_name) = match content_cache {
            Some(triple) => triple,
            None => {
                let content = std::fs::read_to_string(&cand.abs_path)?;
                let doc = parse(&content);
                // Re-resolve type for output.
                let resolved =
                    find_schema_for(&cand.abs_path, root, &schemas).map(|(schema, schema_dir)| {
                        resolve_type(&cand.abs_path, &doc, schema, &schema_dir, &matchers)
                    });
                let resolved_type_name = resolved.as_ref().and_then(|r| match r {
                    ResolvedType::Explicit(n, _) | ResolvedType::PathMatched(n, _) => {
                        Some(n.clone())
                    }
                    _ => None,
                });
                (content, doc, resolved_type_name)
            }
        };

        if opts.json {
            let type_def = resolved_type_name
                .as_deref()
                .and_then(|n| find_type_def(&cand.abs_path, n, root, &schemas));
            let jdoc = document_to_json(
                &doc,
                &cand.abs_path,
                resolved_type_name.as_deref(),
                type_def,
                root,
            );
            writeln!(out, "{}", serde_json::to_string(&jdoc)?)?;
        } else {
            if !first {
                writeln!(out)?;
            }
            first = false;
            writeln!(out, "## {}", cand.rel_path.display())?;
            writeln!(out)?;
            // Serialize the body (blocks only, no frontmatter) for a
            // grep-friendly rendering.
            let body = serialize_blocks(&doc.blocks);
            let trimmed = body.trim_matches('\n');
            if !trimmed.is_empty() {
                writeln!(out, "{trimmed}")?;
            }
            let _ = content; // retained for future raw-output mode
        }
    }

    if opts.count {
        writeln!(out, "{emitted}")?;
    }

    Ok(())
}

// ── Candidate discovery ───────────────────────────────────────────────────────

#[derive(Debug)]
struct Candidate {
    abs_path: PathBuf,
    rel_path: PathBuf,
    basename: String,
    file_date: Option<NaiveDate>,
    /// Type resolved purely from path matching (cheap, no content read).
    path_match_type: Option<String>,
}

fn collect_candidates(
    target: &Path,
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
    out: &mut Vec<Candidate>,
) {
    if target.is_file() {
        if is_markdown(target) {
            if let Some(c) = make_candidate(target, root, schemas, matchers) {
                out.push(c);
            }
        }
        return;
    }
    for entry in walk(target).filter_map(|e| e.ok()) {
        let path = entry.path();
        if is_markdown(path) {
            if let Some(c) = make_candidate(path, root, schemas, matchers) {
                out.push(c);
            }
        }
    }
}

fn make_candidate(
    path: &Path,
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
) -> Option<Candidate> {
    let abs = path.to_path_buf();
    let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    let basename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let file_date = parse_filename_date(&basename);
    let path_match_type = resolve_path_match_type(&abs, schemas, matchers);
    Some(Candidate {
        abs_path: abs,
        rel_path: rel,
        basename,
        file_date,
        path_match_type,
    })
}

/// Extract `YYYY-MM-DD` from a filename (first occurrence).
fn parse_filename_date(name: &str) -> Option<NaiveDate> {
    let bytes = name.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    for start in 0..=bytes.len().saturating_sub(10) {
        let window = &bytes[start..start + 10];
        if window[4] == b'-'
            && window[7] == b'-'
            && window[0..4].iter().all(|b| b.is_ascii_digit())
            && window[5..7].iter().all(|b| b.is_ascii_digit())
            && window[8..10].iter().all(|b| b.is_ascii_digit())
        {
            let y: i32 = std::str::from_utf8(&window[0..4]).ok()?.parse().ok()?;
            let m: u32 = std::str::from_utf8(&window[5..7]).ok()?.parse().ok()?;
            let d: u32 = std::str::from_utf8(&window[8..10]).ok()?.parse().ok()?;
            return NaiveDate::from_ymd_opt(y, m, d);
        }
    }
    None
}

/// Look up the file's path-matched type via its nearest schema, without
/// reading the file. Returns `None` if no unique match.
fn resolve_path_match_type(
    path: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
) -> Option<String> {
    let mut dir = path.parent()?;
    loop {
        let candidate_dir = dir.join(crate::schema::SCHEMA_DIR);
        if schemas.contains_key(&candidate_dir) {
            if let Some(matcher) = matchers.get(&candidate_dir) {
                let schema_root = candidate_dir.parent().unwrap_or_else(|| Path::new(""));
                if let Ok(rel) = path.strip_prefix(schema_root) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    let matched = matcher.match_path(&rel_str);
                    if matched.len() == 1 {
                        return Some(matched[0].to_string());
                    }
                }
            }
            return None;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return None,
        }
    }
}

fn find_type_def<'a>(
    path: &Path,
    type_name: &str,
    root: &Path,
    schemas: &'a HashMap<PathBuf, Schema>,
) -> Option<&'a TypeDef> {
    let (schema, _) = find_schema_for(path, root, schemas)?;
    schema.get_type(type_name)
}

// ── Content filters ───────────────────────────────────────────────────────────

fn apply_content_filters(
    opts: &QueryOptions,
    content: &str,
    doc: &Document,
    resolved_type: Option<&str>,
    cand: &Candidate,
) -> bool {
    // --type by frontmatter (only reached when path_match didn't decide)
    if let Some(wanted) = &opts.type_name {
        if resolved_type != Some(wanted.as_str()) {
            return false;
        }
    }

    // --days via frontmatter `date:` field (only if filename had no date)
    if let Some(n) = opts.days {
        if cand.file_date.is_none() {
            let cutoff = Local::now().date_naive() - chrono::Duration::days(n);
            match frontmatter_date(doc) {
                Some(d) if d >= cutoff => {}
                _ => return false,
            }
        }
    }

    // --property k=v (frontmatter equality, stringified)
    for (k, v) in &opts.properties {
        let fm_val = doc
            .frontmatter
            .as_ref()
            .and_then(|fm| fm.fields.get(k.as_str()));
        let matches = match fm_val {
            Some(yv) => yaml_value_eq(yv, v),
            None => false,
        };
        if !matches {
            return false;
        }
    }

    // --grep over inline text (headings, paragraphs, list items, tables).
    if let Some(pat) = &opts.grep {
        let needle = pat.to_lowercase();
        let haystack = collect_all_text(&doc.blocks).to_lowercase();
        if !haystack.contains(&needle) {
            return false;
        }
    }

    // --has-link against link URLs.
    if let Some(pat) = &opts.has_link {
        let mut found = false;
        let mut urls = Vec::new();
        for block in &doc.blocks {
            collect_link_urls(block, &mut urls);
        }
        for url in urls {
            if url.contains(pat.as_str()) {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }

    // `content` is currently unused but kept in the signature so future
    // filters (e.g. raw-source regex) can opt into it without a refactor.
    let _ = content;
    true
}

fn yaml_value_eq(yv: &serde_yaml::Value, raw: &str) -> bool {
    match yv {
        serde_yaml::Value::String(s) => s == raw,
        serde_yaml::Value::Bool(b) => match raw {
            "true" | "yes" => *b,
            "false" | "no" => !*b,
            _ => false,
        },
        serde_yaml::Value::Number(n) => n.to_string() == raw,
        serde_yaml::Value::Sequence(seq) => seq.iter().any(|v| yaml_value_eq(v, raw)),
        serde_yaml::Value::Null => raw.is_empty() || raw == "null" || raw == "~",
        _ => false,
    }
}

fn frontmatter_date(doc: &Document) -> Option<NaiveDate> {
    let fm = doc.frontmatter.as_ref()?;
    for key in ["date", "created", "published", "updated"] {
        if let Some(serde_yaml::Value::String(s)) = fm.fields.get(key) {
            let head = &s[..s.len().min(10)];
            if let Ok(d) = NaiveDate::parse_from_str(head, "%Y-%m-%d") {
                return Some(d);
            }
        }
    }
    None
}

// ── Inline text + link extraction ─────────────────────────────────────────────

fn collect_all_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        collect_block_text(block, &mut out);
    }
    out
}

fn collect_block_text(block: &Block, out: &mut String) {
    match block {
        Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
            out.push_str(&inlines_to_string(content));
            out.push('\n');
        }
        Block::List { items, .. } => {
            for item in items {
                collect_list_item_text(item, out);
            }
        }
        Block::CodeBlock { content, .. } => {
            out.push_str(content);
            out.push('\n');
        }
        Block::BlockQuote { blocks, .. } => {
            for b in blocks {
                collect_block_text(b, out);
            }
        }
        Block::Table { header, rows, .. } => {
            for cell in header {
                out.push_str(&inlines_to_string(cell));
                out.push('\n');
            }
            for row in rows {
                for cell in row {
                    out.push_str(&inlines_to_string(cell));
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
}

fn collect_list_item_text(item: &ListItem, out: &mut String) {
    out.push_str(&inlines_to_string(&item.content));
    out.push('\n');
    for child in &item.children {
        collect_block_text(child, out);
    }
}

fn collect_link_urls(block: &Block, out: &mut Vec<String>) {
    match block {
        Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
            collect_inline_urls(content, out);
        }
        Block::List { items, .. } => {
            for item in items {
                collect_inline_urls(&item.content, out);
                for child in &item.children {
                    collect_link_urls(child, out);
                }
            }
        }
        Block::BlockQuote { blocks, .. } => {
            for b in blocks {
                collect_link_urls(b, out);
            }
        }
        Block::Table { header, rows, .. } => {
            for cell in header {
                collect_inline_urls(cell, out);
            }
            for row in rows {
                for cell in row {
                    collect_inline_urls(cell, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_inline_urls(inlines: &[Inline], out: &mut Vec<String>) {
    for inline in inlines {
        match inline {
            Inline::Link { url, .. } | Inline::Image { url, .. } => out.push(url.clone()),
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strikethrough(inner) => {
                collect_inline_urls(inner, out)
            }
            _ => {}
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tree(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (rel_path, content) in files {
            let full = dir.path().join(rel_path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        dir
    }

    /// Helper: collect candidates from a tree, applying the cheap pre-filter
    /// path the real query uses. Returns rel paths in discovery order.
    fn discover(dir: &Path) -> Vec<Candidate> {
        let mut dummy = FormatResult::default();
        let (schemas, matchers) = load_all_schemas(dir, &mut dummy);
        let mut out = Vec::new();
        collect_candidates(dir, dir, &schemas, &matchers, &mut out);
        out
    }

    #[test]
    fn discover_picks_up_journal_files() {
        let dir = make_tree(&[
            ("journal/2026-01-01T10-00.md", "# A\n"),
            ("journal/2026-02-15T12-00.md", "# B\n"),
            ("README.md", "# Root\n"),
        ]);
        let cands = discover(dir.path());
        assert_eq!(cands.len(), 3);
        let by_name: HashMap<String, Option<NaiveDate>> = cands
            .iter()
            .map(|c| (c.basename.clone(), c.file_date))
            .collect();
        assert_eq!(
            by_name.get("2026-01-01T10-00.md").copied().flatten(),
            NaiveDate::from_ymd_opt(2026, 1, 1),
            "journal file date extracted from basename"
        );
        assert_eq!(
            by_name.get("README.md").copied().flatten(),
            None,
            "README has no date"
        );
    }

    #[test]
    fn discover_tags_path_match_type() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "paths:\n  - notes/*.md\nfields: {}\n",
            ),
            ("notes/first.md", "# First\n"),
            ("other/skip.md", "# Skip\n"),
        ]);
        let cands = discover(dir.path());
        let notes: Vec<_> = cands
            .iter()
            .filter(|c| c.path_match_type.as_deref() == Some("note"))
            .collect();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].rel_path.ends_with("first.md"));
    }

    #[test]
    fn parse_filename_date_basic() {
        assert_eq!(
            parse_filename_date("2026-04-05T09-53.md"),
            NaiveDate::from_ymd_opt(2026, 4, 5)
        );
        assert_eq!(
            parse_filename_date("2026-03.md"),
            None,
            "month-only filename has no full date"
        );
        assert_eq!(parse_filename_date("README.md"), None);
        assert_eq!(
            parse_filename_date("notes-2025-12-31-final.md"),
            NaiveDate::from_ymd_opt(2025, 12, 31)
        );
        assert_eq!(parse_filename_date("9999-99-99.md"), None);
    }

    #[test]
    fn parse_property_flags_ok() {
        let got = parse_property_flags(&["status=draft".into(), "count=5".into()]).unwrap();
        assert_eq!(got[0], ("status".into(), "draft".into()));
        assert_eq!(got[1], ("count".into(), "5".into()));
    }

    #[test]
    fn parse_property_flags_missing_equals() {
        assert!(parse_property_flags(&["broken".into()]).is_err());
    }

    #[test]
    fn yaml_value_eq_scalars() {
        assert!(yaml_value_eq(
            &serde_yaml::Value::String("draft".into()),
            "draft"
        ));
        assert!(yaml_value_eq(&serde_yaml::Value::Bool(true), "true"));
        assert!(yaml_value_eq(&serde_yaml::Value::Bool(true), "yes"));
        assert!(!yaml_value_eq(&serde_yaml::Value::Bool(true), "false"));
        let n = serde_yaml::Value::Number(serde_yaml::Number::from(5));
        assert!(yaml_value_eq(&n, "5"));
    }

    #[test]
    fn yaml_value_eq_sequence_contains() {
        let seq = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("rust".into()),
            serde_yaml::Value::String("markdown".into()),
        ]);
        assert!(yaml_value_eq(&seq, "rust"));
        assert!(!yaml_value_eq(&seq, "python"));
    }
}
