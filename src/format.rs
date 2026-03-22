//! Orchestration: walk directories, detect types, validate, fix, write.
//!
//! All filesystem I/O lives here. `validate.rs` and `fix.rs` are pure.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use rayon::prelude::*;

use anyhow::{Context, Result};
use tracing::debug;
use walkdir::WalkDir;

use crate::{
    ast::Document,
    fix::apply_fixes,
    parse::{get_frontmatter_error, parse, serialize, serialize_with_field_order},
    schema::{self, PathMatcher, Schema, TypeDef, SCHEMA_DIR},
    validate::{
        detect_malformed_links, resolve_link_path, validate, validate_unknown_type, Diagnostic,
        LinkedDocInfo, ValidateCtx,
    },
};

// ── Public types ──────────────────────────────────────────────────────────────

/// Options controlling formatting behaviour.
#[derive(Debug, Default, Clone, Copy)]
pub struct FormatOptions {
    /// Check mode: report what would change but don't write anything.
    pub check: bool,
}

/// Summary returned after formatting a directory tree.
#[derive(Debug, Default)]
pub struct FormatResult {
    pub files_checked: usize,
    pub files_changed: usize,
    pub errors: Vec<FileError>,
}

/// Diagnostics for a single file.
#[derive(Debug)]
pub struct FileError {
    pub path: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// Format (or check) all markdown files under `root`.
///
/// - Walks the directory tree (skipping `node_modules` and `.git`).
/// - For each `.md` file, finds the nearest `.typedown/` schema dir (walking up to `root`).
/// - Reads the `type` field from frontmatter to pick the schema type.
/// - If no `type:` is present, tries path-pattern matching from `paths:` in schemas.
/// - Validates, applies fixes (unless `check` mode), and writes if changed.
pub fn format_dir(root: &Path, explicit_paths: &[PathBuf], opts: FormatOptions) -> Result<FormatResult> {
    let mut result = FormatResult::default();

    // Pre-load schemas: schema_dir → Schema, and build path matchers.
    let (schemas, matchers) = load_all_schemas(root, &mut result);
    debug!(schema_dirs = schemas.len(), "loaded schemas");

    // Pre-load linked-doc info for bidirectional link validation.
    // Key: absolute path → LinkedDocInfo.  Also returns a doc cache so
    // format_file can skip re-reading/re-parsing files it already has.
    let (mut linked_docs, doc_cache) = preload_linked_docs(root, &schemas, &matchers);
    debug!(linked_docs = linked_docs.len(), "preloaded linked docs");

    // Pre-load git-tracked paths for cross-project link validation.
    let git_tree = crate::git::list_git_paths(root);

    // Discover and pre-load cross-project link targets for typed link validation.
    // Uses the git repo root as the ceiling for external schema discovery.
    let external_types = if let Some(repo_root) = crate::git::git_repo_root(root) {
        let ext_targets = collect_external_targets(&linked_docs, root);
        if ext_targets.is_empty() {
            HashMap::new()
        } else {
            let presets = schema::load_presets();
            let (ext_linked, ext_types) =
                preload_external_link_targets(&ext_targets, &repo_root, presets.as_ref());
            debug!(
                external_docs = ext_linked.len(),
                "preloaded external link targets"
            );
            linked_docs.extend(ext_linked);
            ext_types
        }
    } else {
        HashMap::new()
    };

    // Collect markdown paths: use explicit paths if given, otherwise walk the tree.
    let paths = resolve_file_args(root, explicit_paths);

    result.files_checked = paths.len();

    // Process files in parallel.  Each format_file call is independent: it
    // reads (from cache), applies fixes, and writes its own file.
    let outcomes: Vec<Result<(Option<bool>, Vec<Diagnostic>, PathBuf), (PathBuf, anyhow::Error)>> =
        paths
            .par_iter()
            .map(|path| {
                format_file(
                    path,
                    root,
                    &schemas,
                    &matchers,
                    &linked_docs,
                    &doc_cache,
                    git_tree.as_ref(),
                    &external_types,
                    opts,
                )
                .map(|(changed, diags)| (changed, diags, path.clone()))
                .map_err(|e| (path.clone(), e))
            })
            .collect();

    for outcome in outcomes {
        match outcome {
            Ok((Some(true), unfixable, path)) => {
                result.files_changed += 1;
                if !unfixable.is_empty() {
                    result.errors.push(FileError {
                        path,
                        diagnostics: unfixable,
                    });
                }
            }
            Ok((_, unfixable, path)) => {
                if !unfixable.is_empty() {
                    result.errors.push(FileError {
                        path,
                        diagnostics: unfixable,
                    });
                }
            }
            Err((path, e)) => result.errors.push(FileError {
                path,
                diagnostics: vec![Diagnostic::UnknownType {
                    line: 0,
                    message: format!("error processing file: {e}"),
                }],
            }),
        }
    }

    Ok(result)
}

/// Check all markdown files under `root` without writing anything.
///
/// Returns diagnostics grouped by file.
pub fn check_dir(root: &Path, explicit_paths: &[PathBuf]) -> Result<Vec<FileError>> {
    let mut result = FormatResult::default();
    let (schemas, matchers) = load_all_schemas(root, &mut result);
    debug!(schema_dirs = schemas.len(), "loaded schemas");
    let (mut linked_docs, doc_cache) = preload_linked_docs(root, &schemas, &matchers);
    debug!(linked_docs = linked_docs.len(), "preloaded linked docs");
    let git_tree = crate::git::list_git_paths(root);

    let external_types = if let Some(repo_root) = crate::git::git_repo_root(root) {
        let ext_targets = collect_external_targets(&linked_docs, root);
        if ext_targets.is_empty() {
            HashMap::new()
        } else {
            let presets = schema::load_presets();
            let (ext_linked, ext_types) =
                preload_external_link_targets(&ext_targets, &repo_root, presets.as_ref());
            debug!(
                external_docs = ext_linked.len(),
                "preloaded external link targets"
            );
            linked_docs.extend(ext_linked);
            ext_types
        }
    } else {
        HashMap::new()
    };

    let mut file_errors: Vec<FileError> = result.errors; // schema load errors

    // Collect markdown paths: use explicit paths if given, otherwise walk the tree.
    let paths = resolve_file_args(root, explicit_paths);

    // Check files in parallel; each is independent (read-only validation).
    let parallel_errors: Vec<FileError> = paths
        .par_iter()
        .filter_map(|path| {
            let diagnostics = check_file(
                path,
                root,
                &schemas,
                &matchers,
                &linked_docs,
                &doc_cache,
                git_tree.as_ref(),
                &external_types,
            );
            if diagnostics.is_empty() {
                None
            } else {
                Some(FileError {
                    path: path.clone(),
                    diagnostics,
                })
            }
        })
        .collect();

    file_errors.extend(parallel_errors);
    Ok(file_errors)
}

// ── Per-file helpers ──────────────────────────────────────────────────────────

/// Format a single file. Returns `Ok(Some(changed))` if the file is covered by
/// a schema, `Ok(None)` if it has no applicable schema.
#[allow(clippy::too_many_arguments)]
fn format_file(
    path: &Path,
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
    linked_docs: &HashMap<PathBuf, LinkedDocInfo>,
    doc_cache: &HashMap<PathBuf, (String, Document)>,
    git_tree: Option<&std::collections::HashSet<PathBuf>>,
    external_types: &HashMap<String, TypeDef>,
    opts: FormatOptions,
) -> Result<(Option<bool>, Vec<Diagnostic>)> {
    // Use the preloaded content+doc if available; otherwise read from disk.
    // For format mode we always need an owned (mutable) Document for fix
    // application, so we clone the cached doc rather than re-parsing from disk.
    let (content, mut doc) = if let Some((cached_content, cached_doc)) = doc_cache.get(path) {
        (cached_content.clone(), cached_doc.clone())
    } else {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let doc = parse(&content);
        (content, doc)
    };

    let file_size = content.len();
    let rel = path.strip_prefix(root).unwrap_or(path);
    let Some((schema, schema_dir)) = find_schema_for(path, root, schemas) else {
        debug!(file = %rel.display(), "no schema covers file, skipping");
        return Ok((None, vec![]));
    };

    // Resolve type: explicit frontmatter `type:` takes priority, then path patterns.
    let resolved = resolve_type(path, &doc, schema, &schema_dir, matchers);
    match &resolved {
        ResolvedType::Explicit(name, _) => {
            debug!(file = %rel.display(), r#type = name, "explicit type from frontmatter");
        }
        ResolvedType::PathMatched(name, _) => {
            debug!(file = %rel.display(), r#type = name, "type matched by path pattern");
        }
        ResolvedType::OptedOut => {
            debug!(file = %rel.display(), "type: none, opted out");
        }
        ResolvedType::Conflict(types) => {
            debug!(file = %rel.display(), ?types, "conflicting path matches");
        }
        ResolvedType::Unknown => {
            debug!(file = %rel.display(), "no type resolved");
        }
    }

    let mut diagnostics = match &resolved {
        ResolvedType::Explicit(name, type_def) | ResolvedType::PathMatched(name, type_def) => {
            let ctx = ValidateCtx {
                source_path: path,
                source_type: name,
                schema,
                linked_docs,
                git_tree,
                external_types,
            };
            let mut diags = validate(&doc, type_def, &ctx, Some(file_size));
            // Path-matched files don't need `type:` in frontmatter -- suppress
            // MissingRequiredField for "type" when the match came from paths.
            if matches!(&resolved, ResolvedType::PathMatched(..)) {
                suppress_type_field_requirement(&mut diags);
            }
            diags
        }
        ResolvedType::OptedOut => return Ok((Some(false), vec![])),
        ResolvedType::Conflict(types) => {
            vec![Diagnostic::UnknownType {
                line: 0,
                message: format!(
                    "file matches path patterns from multiple types: {}",
                    types.join(", ")
                ),
            }]
        }
        ResolvedType::Unknown => validate_unknown_type(&doc, schema),
    };

    // Scan raw source for malformed links (spaces in URLs that the parser
    // silently drops as non-links).
    diagnostics.extend(detect_malformed_links(&content));

    // Enrich MissingFrontmatter with parse error reason (for display)
    if doc.frontmatter.is_none() {
        if let Some(parse_err) = get_frontmatter_error(&content) {
            for d in &mut diagnostics {
                if matches!(d, Diagnostic::MissingFrontmatter) {
                    *d = Diagnostic::UnknownType {
                        line: 1,
                        message: format!("frontmatter parse error: {parse_err}"),
                    };
                }
            }
        }
    }

    // Path-matched files with no frontmatter: suppress MissingFrontmatter if
    // the schema has no required fields/frontmatter entries.
    if matches!(&resolved, ResolvedType::PathMatched(..)) && doc.frontmatter.is_none() {
        suppress_missing_frontmatter(&mut diagnostics, &resolved);
    }

    if !diagnostics.is_empty() {
        debug!(file = %rel.display(), count = diagnostics.len(), "diagnostics found");
    }

    // Partition into fixable and unfixable diagnostics.
    // Apply fixes for fixable ones; return unfixable ones to the caller.
    let (fixable, unfixable): (Vec<_>, Vec<_>) = diagnostics
        .into_iter()
        .partition(|d| crate::fix::Fix::is_fixable(d));

    if !unfixable.is_empty() {
        debug!(file = %rel.display(), count = unfixable.len(), "unfixable diagnostics");
    }

    apply_fixes(&mut doc, &fixable);
    crate::parse::normalize_blank_lines(&mut doc.blocks);
    // Strip trailing blank lines so serialized output round-trips cleanly.
    while doc.blocks.last() == Some(&crate::ast::Block::BlankLine) {
        doc.blocks.pop();
    }

    // Build field order for serialization
    let type_name = match &resolved {
        ResolvedType::Explicit(name, _) | ResolvedType::PathMatched(name, _) => Some(name.clone()),
        _ => None,
    };
    let field_order: Option<Vec<String>> = type_name.as_deref().and_then(|name| {
        schema.get_type(name).map(|td| {
            let mut order = vec!["type".to_string()];
            order.extend(td.fields.keys().cloned());
            order
        })
    });

    let formatted = match &field_order {
        Some(order) => {
            let refs: Vec<&str> = order.iter().map(|s| s.as_str()).collect();
            serialize_with_field_order(&doc, &refs)
        }
        None => serialize(&doc),
    };

    if formatted == content {
        debug!(file = %rel.display(), "unchanged");
        return Ok((Some(false), unfixable));
    }

    if !opts.check {
        debug!(file = %rel.display(), "formatted (written)");
        std::fs::write(path, &formatted).with_context(|| format!("writing {}", path.display()))?;
    } else {
        debug!(file = %rel.display(), "would change (check mode)");
    }

    Ok((Some(true), unfixable))
}

/// Check a single file and return any diagnostics (no writes).
#[allow(clippy::too_many_arguments)]
fn check_file(
    path: &Path,
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
    linked_docs: &HashMap<PathBuf, LinkedDocInfo>,
    doc_cache: &HashMap<PathBuf, (String, Document)>,
    git_tree: Option<&std::collections::HashSet<PathBuf>>,
    external_types: &HashMap<String, TypeDef>,
) -> Vec<Diagnostic> {
    // Fast path: reuse the preloaded (content, doc) — no disk I/O or re-parse.
    if let Some((content, doc)) = doc_cache.get(path) {
        return check_document(
            path,
            content,
            doc,
            root,
            schemas,
            matchers,
            linked_docs,
            git_tree,
            external_types,
        );
    }
    // Slow path: file wasn't preloaded (shouldn't happen for files under a schema).
    let Ok(content) = std::fs::read_to_string(path) else {
        return vec![Diagnostic::UnknownType {
            line: 0,
            message: "could not read file".to_string(),
        }];
    };
    let doc = parse(&content);
    check_document(
        path,
        &content,
        &doc,
        root,
        schemas,
        matchers,
        linked_docs,
        git_tree,
        external_types,
    )
}

/// Inner check logic operating on already-loaded content and document.
#[allow(clippy::too_many_arguments)]
fn check_document(
    path: &Path,
    content: &str,
    doc: &Document,
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
    linked_docs: &HashMap<PathBuf, LinkedDocInfo>,
    git_tree: Option<&std::collections::HashSet<PathBuf>>,
    external_types: &HashMap<String, TypeDef>,
) -> Vec<Diagnostic> {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let file_size = content.len();
    let Some((schema, schema_dir)) = find_schema_for(path, root, schemas) else {
        debug!(file = %rel.display(), "no schema covers file, skipping");
        return vec![];
    };

    let resolved = resolve_type(path, doc, schema, &schema_dir, matchers);
    match &resolved {
        ResolvedType::Explicit(name, _) => {
            debug!(file = %rel.display(), r#type = name, "explicit type from frontmatter");
        }
        ResolvedType::PathMatched(name, _) => {
            debug!(file = %rel.display(), r#type = name, "type matched by path pattern");
        }
        ResolvedType::OptedOut => {
            debug!(file = %rel.display(), "type: none, opted out");
        }
        ResolvedType::Conflict(types) => {
            debug!(file = %rel.display(), ?types, "conflicting path matches");
        }
        ResolvedType::Unknown => {
            debug!(file = %rel.display(), "no type resolved");
        }
    }

    let mut diagnostics = match &resolved {
        ResolvedType::Explicit(name, type_def) | ResolvedType::PathMatched(name, type_def) => {
            let ctx = ValidateCtx {
                source_path: path,
                source_type: name,
                schema,
                linked_docs,
                git_tree,
                external_types,
            };
            let mut diags = validate(doc, type_def, &ctx, Some(file_size));
            if matches!(&resolved, ResolvedType::PathMatched(..)) {
                suppress_type_field_requirement(&mut diags);
            }
            diags
        }
        ResolvedType::OptedOut => return vec![],
        ResolvedType::Conflict(types) => {
            vec![Diagnostic::UnknownType {
                line: 0,
                message: format!(
                    "file matches path patterns from multiple types: {}",
                    types.join(", ")
                ),
            }]
        }
        ResolvedType::Unknown => validate_unknown_type(doc, schema),
    };

    // Suppress MissingFrontmatter for path-matched files with no required fields
    if matches!(&resolved, ResolvedType::PathMatched(..)) && doc.frontmatter.is_none() {
        suppress_missing_frontmatter(&mut diagnostics, &resolved);
    }

    diagnostics.extend(detect_malformed_links(content));
    diagnostics
}

// ── Schema loading ────────────────────────────────────────────────────────────

/// Walk `root` and load every `.typedown/` directory found.
///
/// Merges XDG presets (`~/.config/typedown/presets/`) into each discovered
/// schema as a base layer — project-local types win entirely when names collide.
///
/// Schema load errors are pushed into `result.errors`.
/// Returns both the loaded schemas and compiled path matchers.
pub(crate) fn load_all_schemas(
    root: &Path,
    result: &mut FormatResult,
) -> (HashMap<PathBuf, Schema>, HashMap<PathBuf, PathMatcher>) {
    let mut schemas: HashMap<PathBuf, Schema> = HashMap::new();
    let mut matchers: HashMap<PathBuf, PathMatcher> = HashMap::new();
    let presets = schema::load_presets();
    if let Some(ref p) = presets {
        debug!(types = p.types.len(), "loaded presets");
    }

    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e.file_name().to_str().unwrap_or("")));

    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(SCHEMA_DIR) && path.is_dir() {
            if schemas.contains_key(path) {
                continue;
            }
            match Schema::load(path) {
                Ok(mut schema) => {
                    let local_types: Vec<&str> = schema.types.keys().map(|s| s.as_str()).collect();
                    debug!(
                        dir = %path.display(),
                        ?local_types,
                        "found schema directory"
                    );
                    // Merge presets: fill in types not defined locally.
                    if let Some(ref presets) = presets {
                        for (name, type_def) in &presets.types {
                            if !schema.types.contains_key(name) {
                                debug!(name, "merged preset type");
                                schema.types.insert(name.clone(), type_def.clone());
                            }
                        }
                    }

                    match schema.build_path_matcher() {
                        Ok(matcher) => {
                            matchers.insert(path.to_path_buf(), matcher);
                        }
                        Err(e) => {
                            result.errors.push(FileError {
                                path: path.to_path_buf(),
                                diagnostics: vec![Diagnostic::UnknownType {
                                    line: 0,
                                    message: format!("path pattern error: {e}"),
                                }],
                            });
                        }
                    }
                    schemas.insert(path.to_path_buf(), schema);
                }
                Err(e) => {
                    result.errors.push(FileError {
                        path: path.to_path_buf(),
                        diagnostics: vec![Diagnostic::UnknownType {
                            line: 0,
                            message: format!("schema error: {e}"),
                        }],
                    });
                }
            }
        }
    }

    // If no .typedown/ dirs found but presets exist, create a virtual
    // root-level schema so preset path patterns can anchor against root.
    if schemas.is_empty() {
        if let Some(presets) = presets {
            debug!("no .typedown/ dir found; activating presets at root");
            let virtual_dir = root.join(SCHEMA_DIR);
            match presets.build_path_matcher() {
                Ok(matcher) => {
                    matchers.insert(virtual_dir.clone(), matcher);
                }
                Err(e) => {
                    result.errors.push(FileError {
                        path: virtual_dir.clone(),
                        diagnostics: vec![Diagnostic::UnknownType {
                            line: 0,
                            message: format!("path pattern error: {e}"),
                        }],
                    });
                }
            }
            schemas.insert(virtual_dir, presets);
        }
    }

    (schemas, matchers)
}

/// Find the nearest schema covering `path` (search up to `root`).
pub(crate) fn find_schema_for<'a>(
    path: &Path,
    root: &Path,
    schemas: &'a HashMap<PathBuf, Schema>,
) -> Option<(&'a Schema, PathBuf)> {
    let mut dir = path.parent()?;
    loop {
        let candidate = dir.join(SCHEMA_DIR);
        if let Some(schema) = schemas.get(&candidate) {
            return Some((schema, candidate));
        }
        if dir == root {
            break;
        }
        dir = dir.parent()?;
    }
    None
}

// ── Linked-doc pre-loading ────────────────────────────────────────────────────

/// Read every `.md` file under directories covered by a schema and extract its
/// doc type + per-section link URLs. This is passed to validate() so
/// bidirectional link checks need no I/O during validation.
///
/// Also returns a doc cache: `abs_path → (raw_content, parsed Document)`.
/// The main file loop uses this cache to avoid re-reading and re-parsing every
/// file, cutting I/O and parse work roughly in half.
fn preload_linked_docs(
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
) -> (
    HashMap<PathBuf, LinkedDocInfo>,
    HashMap<PathBuf, (String, Document)>,
) {
    let mut linked: HashMap<PathBuf, LinkedDocInfo> = HashMap::new();
    let mut doc_cache: HashMap<PathBuf, (String, Document)> = HashMap::new();

    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e.file_name().to_str().unwrap_or("")));

    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_markdown(path) {
            continue;
        }

        // Only pre-load files that are under a known schema
        let abs = path.to_path_buf();
        if find_schema_for(&abs, root, schemas).is_none() {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };

        let doc = parse(&content);

        // Resolve doc type: explicit frontmatter first, then path patterns
        let doc_type = match doc.frontmatter.as_ref().and_then(|fm| fm.doc_type.clone()) {
            Some(t) => Some(t),
            None => {
                // Try path-pattern matching
                if let Some((schema, schema_dir)) = find_schema_for(&abs, root, schemas) {
                    match resolve_type(&abs, &doc, schema, &schema_dir, matchers) {
                        ResolvedType::PathMatched(name, _) => Some(name),
                        _ => None,
                    }
                } else {
                    None
                }
            }
        };

        linked.insert(
            abs.clone(),
            LinkedDocInfo {
                path: path.to_path_buf(),
                doc_type,
                section_links: extract_doc_section_links(&doc),
            },
        );

        // Cache the parsed content+doc so the main loop can skip re-read/re-parse.
        doc_cache.insert(abs, (content, doc));
    }

    (linked, doc_cache)
}

// ── Cross-project schema discovery and external target preloading ─────────────

/// Collect all relative link URLs from H2 sections in a document, keyed by
/// section title.  Used by both local and external preloading.
fn extract_doc_section_links(doc: &Document) -> HashMap<String, Vec<String>> {
    let mut section_links: HashMap<String, Vec<String>> = HashMap::new();
    let h2s: Vec<(usize, String)> = doc
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b {
            crate::ast::Block::Heading {
                level: 2, content, ..
            } => Some((i, crate::ast::inlines_to_string(content))),
            _ => None,
        })
        .collect();

    for (hi, (start, title)) in h2s.iter().enumerate() {
        let end = h2s.get(hi + 1).map(|(i, _)| *i).unwrap_or(doc.blocks.len());
        let section_blocks = &doc.blocks[start + 1..end];
        let links = extract_all_links(section_blocks);
        if !links.is_empty() {
            section_links.insert(title.clone(), links);
        }
    }

    section_links
}

/// Walk up from `path` looking for a `.typedown/` directory, stopping at
/// `ceiling` (typically the git repo root).  Returns the first found.
///
/// Unlike `find_schema_for`, this checks the filesystem directly (no
/// pre-loaded map) and is used for cross-project schema discovery.
fn find_external_schema_dir(path: &Path, ceiling: &Path) -> Option<PathBuf> {
    let mut dir = path.parent()?;
    loop {
        let candidate = dir.join(SCHEMA_DIR);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if dir == ceiling {
            break;
        }
        dir = dir.parent()?;
    }
    None
}

/// Collect the set of unique external (cross-project) link targets that appear
/// in any section of any preloaded document.
///
/// "External" means the resolved absolute path is outside `root`.  Only files
/// that actually exist on disk are included; broken cross-project links are
/// already caught by the `git_tree` check in `validate_all_links`.
fn collect_external_targets(
    linked_docs: &HashMap<PathBuf, LinkedDocInfo>,
    root: &Path,
) -> HashSet<PathBuf> {
    let mut targets = HashSet::new();
    for (source_path, info) in linked_docs {
        for urls in info.section_links.values() {
            for url in urls {
                let Some(target) = resolve_link_path(url, source_path) else {
                    continue;
                };
                if target.starts_with(root) {
                    continue; // internal — already in linked_docs
                }
                if target.exists() {
                    targets.insert(target);
                }
            }
        }
    }
    targets
}

/// Pre-load type and section-link information for cross-project link targets.
///
/// For each unique external target file:
/// 1. Walk up from the target to find its `.typedown/` directory (stopping at
///    `ceiling`, the git repo root).
/// 2. Load that schema (cached by schema dir — typically just one load per
///    sibling project).
/// 3. Read + parse the target file and resolve its doc type via frontmatter or
///    path-pattern matching against the external schema.
/// 4. Extract H2 section links for bidirectional validation.
///
/// File reads in step 3 are parallelised with rayon.
///
/// Returns:
/// - A map of `abs_path → LinkedDocInfo` to merge into the main `linked_docs`.
/// - A flat map of `type_name → TypeDef` from all discovered external schemas,
///   used to resolve target type definitions during bidi validation.
fn preload_external_link_targets(
    targets: &HashSet<PathBuf>,
    ceiling: &Path,
    presets: Option<&Schema>,
) -> (HashMap<PathBuf, LinkedDocInfo>, HashMap<String, TypeDef>) {
    if targets.is_empty() {
        return (HashMap::new(), HashMap::new());
    }

    // Phase 1 (sequential): discover schema dirs and load schemas.
    // Typically hits just one or two sibling projects, so this is cheap.
    let mut schema_cache: HashMap<PathBuf, (Schema, Option<PathMatcher>)> = HashMap::new();
    let mut target_schema_dir: HashMap<PathBuf, PathBuf> = HashMap::new();

    for target in targets {
        let Some(schema_dir) = find_external_schema_dir(target, ceiling) else {
            continue;
        };
        target_schema_dir.insert(target.clone(), schema_dir.clone());

        if let std::collections::hash_map::Entry::Vacant(e) = schema_cache.entry(schema_dir) {
            let schema_dir_ref = e.key();
            match Schema::load(schema_dir_ref) {
                Ok(mut schema) => {
                    // Merge presets: fill in types not defined locally.
                    if let Some(presets) = presets {
                        for (name, type_def) in &presets.types {
                            if !schema.types.contains_key(name) {
                                debug!(name, "merged preset type into external schema");
                                schema.types.insert(name.clone(), type_def.clone());
                            }
                        }
                    }
                    let matcher = schema.build_path_matcher().ok();
                    debug!(
                        dir = %schema_dir_ref.display(),
                        types = schema.types.len(),
                        "loaded external schema"
                    );
                    e.insert((schema, matcher));
                }
                Err(err) => {
                    debug!(dir = %schema_dir_ref.display(), err = %err, "failed to load external schema");
                }
            }
        }
    }

    // Phase 2: build flat type map from all external schemas.
    let mut external_types: HashMap<String, TypeDef> = HashMap::new();
    for (schema, _) in schema_cache.values() {
        for (name, type_def) in &schema.types {
            external_types
                .entry(name.clone())
                .or_insert_with(|| type_def.clone());
        }
    }

    // Phase 3 (parallel): read + parse each target file, resolve type,
    // extract section links.  schema_cache is read-only here.
    let work: Vec<(PathBuf, Option<PathBuf>)> = targets
        .iter()
        .map(|t| (t.clone(), target_schema_dir.get(t).cloned()))
        .collect();

    let entries: Vec<(PathBuf, LinkedDocInfo)> = work
        .par_iter()
        .filter_map(|(target, schema_dir_opt)| {
            let content = std::fs::read_to_string(target).ok()?;
            let doc = parse(&content);

            // Resolve type: frontmatter first, then path-pattern fallback.
            let doc_type =
                if let Some(t) = doc.frontmatter.as_ref().and_then(|fm| fm.doc_type.clone()) {
                    // "type: none" opts out of validation — treat as untyped.
                    if t == "none" {
                        None
                    } else {
                        Some(t)
                    }
                } else if let Some(schema_dir) = schema_dir_opt {
                    schema_cache
                        .get(schema_dir)
                        .and_then(|(_, matcher_opt)| matcher_opt.as_ref())
                        .and_then(|matcher| {
                            let schema_root = schema_dir.parent().unwrap_or_else(|| Path::new(""));
                            target.strip_prefix(schema_root).ok().and_then(|rel| {
                                let rel_str = rel.to_string_lossy().replace('\\', "/");
                                let matched = matcher.match_path(&rel_str);
                                if matched.len() == 1 {
                                    Some(matched[0].to_string())
                                } else {
                                    None
                                }
                            })
                        })
                } else {
                    None
                };

            Some((
                target.clone(),
                LinkedDocInfo {
                    path: target.clone(),
                    doc_type,
                    section_links: extract_doc_section_links(&doc),
                },
            ))
        })
        .collect();

    let linked_map = entries.into_iter().collect();
    (linked_map, external_types)
}

// ── Link extraction ───────────────────────────────────────────────────────────

fn extract_all_links(blocks: &[crate::ast::Block]) -> Vec<String> {
    let mut links = Vec::new();
    for block in blocks {
        collect_links(block, &mut links);
    }
    links
}

fn collect_links(block: &crate::ast::Block, links: &mut Vec<String>) {
    use crate::ast::Block;
    match block {
        Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
            collect_inline_links(content, links)
        }
        Block::List { items, .. } => {
            for item in items {
                collect_inline_links(&item.content, links);
                for child in &item.children {
                    collect_links(child, links);
                }
            }
        }
        Block::BlockQuote { blocks, .. } => {
            for inner in blocks {
                collect_links(inner, links);
            }
        }
        Block::Table { header, rows, .. } => {
            for cell in header {
                collect_inline_links(cell, links);
            }
            for row in rows {
                for cell in row {
                    collect_inline_links(cell, links);
                }
            }
        }
        _ => {}
    }
}

fn collect_inline_links(inlines: &[crate::ast::Inline], links: &mut Vec<String>) {
    use crate::ast::Inline;
    for inline in inlines {
        match inline {
            Inline::Link { url, .. } | Inline::Image { url, .. } => links.push(url.clone()),
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strikethrough(inner) => {
                collect_inline_links(inner, links)
            }
            _ => {}
        }
    }
}

// ── Type resolution ───────────────────────────────────────────────────────────

/// Result of resolving a file's document type.
pub(crate) enum ResolvedType<'a> {
    /// Frontmatter `type:` matched a schema type (highest priority).
    Explicit(String, &'a crate::schema::TypeDef),
    /// No `type:` in frontmatter, but file path matched a schema's `paths:` patterns.
    PathMatched(String, &'a crate::schema::TypeDef),
    /// Frontmatter has `type: none` -- explicitly opted out of validation.
    OptedOut,
    /// Path patterns from multiple types matched (conflict).
    Conflict(Vec<String>),
    /// No type could be determined.
    Unknown,
}

/// Resolve a file's type: frontmatter `type:` takes priority, then path patterns.
pub(crate) fn resolve_type<'a>(
    path: &Path,
    doc: &crate::ast::Document,
    schema: &'a Schema,
    schema_dir: &Path,
    matchers: &'a HashMap<PathBuf, PathMatcher>,
) -> ResolvedType<'a> {
    let fm_type = doc.frontmatter.as_ref().and_then(|fm| fm.doc_type.clone());

    // 1. `type: none` opts out entirely
    if fm_type.as_deref() == Some("none") {
        return ResolvedType::OptedOut;
    }

    // 2. Explicit `type:` in frontmatter
    if let Some(ref name) = fm_type {
        if let Some(type_def) = schema.get_type(name) {
            return ResolvedType::Explicit(name.clone(), type_def);
        }
        // type field present but unknown -- fall through to Unknown
        return ResolvedType::Unknown;
    }

    // 3. Path-pattern matching (only when no `type:` field)
    if let Some(matcher) = matchers.get(schema_dir) {
        let schema_root = schema_dir.parent().unwrap_or_else(|| Path::new(""));
        if let Ok(rel) = path.strip_prefix(schema_root) {
            // Normalize to forward slashes for cross-platform glob matching
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let matched_types = matcher.match_path(&rel_str);

            match matched_types.len() {
                0 => {} // no match, fall through
                1 => {
                    let type_name = matched_types[0];
                    if let Some(type_def) = schema.get_type(type_name) {
                        return ResolvedType::PathMatched(type_name.to_string(), type_def);
                    }
                }
                _ => {
                    return ResolvedType::Conflict(
                        matched_types.iter().map(|s| s.to_string()).collect(),
                    );
                }
            }
        }
    }

    ResolvedType::Unknown
}

/// Remove `MissingRequiredField` diagnostics for the `type` field.
///
/// Path-matched files don't need `type:` in frontmatter since their type is
/// determined by their location.
fn suppress_type_field_requirement(diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.retain(|d| {
        !matches!(d, Diagnostic::MissingRequiredField { field, .. }
            if field.starts_with("type"))
    });
}

/// Remove `MissingFrontmatter` if a path-matched schema has no required fields.
fn suppress_missing_frontmatter(diagnostics: &mut Vec<Diagnostic>, resolved: &ResolvedType<'_>) {
    if let ResolvedType::PathMatched(name, _) = resolved {
        let ResolvedType::PathMatched(_, type_def) = resolved else {
            return;
        };
        let has_required_fields = type_def.fields.values().any(|f| f.required);
        if !has_required_fields {
            diagnostics.retain(|d| !matches!(d, Diagnostic::MissingFrontmatter));
        } else {
            // Replace MissingFrontmatter with a more specific message
            for d in diagnostics.iter_mut() {
                if matches!(d, Diagnostic::MissingFrontmatter) {
                    *d = Diagnostic::UnknownType {
                        line: 1,
                        message: format!(
                            "file matched type '{name}' by path but is missing required frontmatter fields"
                        ),
                    };
                }
            }
        }
    }
}

// ── Project root detection ────────────────────────────────────────────────────

/// Walk up from `start` to find the nearest directory containing `.typedown/`.
///
/// Returns the parent of `.typedown/`, not the `.typedown/` dir itself.
/// Used by the LSP and CLI to determine the project root from a file path.
///
/// Stops walking at the nearest `.git` directory to avoid escaping the current
/// repository. Falls back to `start`'s directory when no `.typedown/` is found
/// within the repo boundary, so preset-only projects still get a usable root.
pub(crate) fn find_project_root(start: &Path) -> Option<PathBuf> {
    let start_dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    let mut dir = start_dir;
    let mut git_boundary: Option<&Path> = None;
    loop {
        if dir.join(SCHEMA_DIR).is_dir() {
            return Some(dir.to_path_buf());
        }
        // Record the git root but keep walking -- .typedown/ may be above .git/
        // in a monorepo layout. We only use it as the fallback boundary.
        if git_boundary.is_none() && dir.join(".git").exists() {
            git_boundary = Some(dir);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    // Fallback: stop at git boundary if found, otherwise use start directory.
    Some(git_boundary.unwrap_or(start_dir).to_path_buf())
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Resolve explicit file arguments into a list of markdown paths, or fall back
/// to walking the entire project tree when no arguments are given.
///
/// Directories in `explicit_paths` are walked recursively.  Non-existent paths
/// are silently skipped (the caller already has the full project tree for
/// link-validation purposes).  When explicit paths are provided, a warning is
/// emitted so that callers know to re-run without arguments for full coverage.
fn resolve_file_args(root: &Path, explicit_paths: &[PathBuf]) -> Vec<PathBuf> {
    if explicit_paths.is_empty() {
        return WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_ignored(e.file_name().to_str().unwrap_or("")))
            .filter_map(|e| e.ok())
            .filter(|e| is_markdown(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect();
    }

    let mut paths = Vec::new();
    for p in explicit_paths {
        if p.is_dir() {
            let walked: Vec<PathBuf> = WalkDir::new(p)
                .into_iter()
                .filter_entry(|e| !is_ignored(e.file_name().to_str().unwrap_or("")))
                .filter_map(|e| e.ok())
                .filter(|e| is_markdown(e.path()))
                .map(|e| e.path().to_path_buf())
                .collect();
            paths.extend(walked);
        } else if p.is_file() && is_markdown(p) {
            paths.push(p.clone());
        } else if !p.exists() {
            debug!("skipping non-existent path: {}", p.display());
        }
    }

    eprintln!(
        "warning: only checked {} file(s); re-run without file arguments to check everything",
        paths.len(),
    );

    paths
}

pub(crate) fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

pub(crate) fn is_ignored(name: &str) -> bool {
    matches!(name, "node_modules" | ".git" | "target")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialize tests that mutate `XDG_CONFIG_HOME`.  Rust's test harness runs
    /// tests in parallel; `set_var`/`remove_var` are not thread-safe, so
    /// concurrent mutations cause spurious failures in sandboxed environments
    /// (e.g. Nix builds).
    static XDG_LOCK: Mutex<()> = Mutex::new(());

    fn make_tree(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (rel_path, content) in files {
            let full = dir.path().join(rel_path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        dir
    }

    // ── format_dir ────────────────────────────────────────────────────────────

    #[test]
    fn test_format_dir_no_schema_no_changes() {
        // Isolate from real XDG presets on the developer's machine.
        let xdg_dir = TempDir::new().unwrap();
        let dir = make_tree(&[("notes/hello.md", "# Hello\n\nJust a note.\n")]);
        let _guard = XDG_LOCK.lock().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        drop(_guard);
        assert_eq!(result.files_changed, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_format_dir_valid_doc_no_changes() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "fields:\n  title:\n    type: string\n    required: true\n",
            ),
            ("note.md", "---\ntype: note\ntitle: Hello\n---\n# Hello\n"),
        ]);
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        assert_eq!(result.files_changed, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_format_dir_unfixable_errors_not_written() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "fields:\n  title:\n    type: string\n    required: true\n",
            ),
            // Missing required field 'title' — unfixable
            ("note.md", "---\ntype: note\n---\n# Hello\n"),
        ]);

        let original = fs::read_to_string(dir.path().join("note.md")).unwrap();
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();

        // File should be unchanged (no fixable issues)
        let after = fs::read_to_string(dir.path().join("note.md")).unwrap();
        assert_eq!(original, after);
        // Result records 0 changes (file not written)
        assert_eq!(result.files_changed, 0);
        // Unfixable errors are now reported
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].path.ends_with("note.md"));
    }

    // ── check_dir ─────────────────────────────────────────────────────────────

    #[test]
    fn test_check_dir_reports_errors() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "fields:\n  title:\n    type: string\n    required: true\n",
            ),
            ("note.md", "---\ntype: note\n---\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(!errors.is_empty(), "expected validation errors, got none");
        assert!(errors[0]
            .diagnostics
            .iter()
            .any(|d| d.message().contains("missing required field 'title'")));
    }

    #[test]
    fn test_check_dir_clean_doc_no_errors() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "fields:\n  title:\n    type: string\n    required: true\n",
            ),
            ("note.md", "---\ntype: note\ntitle: Hello\n---\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }

    #[test]
    fn test_check_dir_no_schema_no_errors() {
        // Isolate from real XDG presets on the developer's machine.
        let xdg_dir = TempDir::new().unwrap();
        let dir = make_tree(&[("notes/hello.md", "# Hello\n")]);
        let _guard = XDG_LOCK.lock().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        let errors = check_dir(dir.path(), &[]).unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        drop(_guard);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_check_mode_does_not_write() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "structure:\n  title: from_filename\n",
            ),
            // Title doesn't match filename — fixable, but check mode shouldn't write
            ("my-note.md", "---\ntype: note\n---\n# Wrong Title\n"),
        ]);

        let original = fs::read_to_string(dir.path().join("my-note.md")).unwrap();
        let _result = format_dir(dir.path(), &[], FormatOptions { check: true }).unwrap();
        let after = fs::read_to_string(dir.path().join("my-note.md")).unwrap();
        assert_eq!(original, after, "check mode should not modify files");
    }

    // ── Schema in subdirectory ─────────────────────────────────────────────────

    #[test]
    fn test_schema_in_subdir_covers_files_below_it() {
        let dir = make_tree(&[
            (
                "projects/.typedown/task.yaml",
                "fields:\n  owner:\n    type: string\n    required: true\n",
            ),
            // Invalid: missing required 'owner'
            ("projects/build.md", "---\ntype: task\n---\n"),
            // Not covered by the schema (above it)
            ("README.md", "# Root\n"),
        ]);

        let errors = check_dir(dir.path(), &[]).unwrap();
        // Only projects/build.md should have errors
        assert_eq!(errors.len(), 1);
        assert!(errors[0].path.ends_with("build.md"));
    }

    // ── Fix idempotency ───────────────────────────────────────────────────────
    //
    // Running `td fmt` twice on the same directory must produce identical output
    // on the second pass (no changes, no errors).  These tests verify that
    // each fixable diagnostic class doesn't re-trigger itself after being fixed.

    fn assert_idempotent(files: &[(&str, &str)]) {
        let dir = make_tree(files);
        let root = dir.path();

        // First pass — apply fixes
        let _result1 = format_dir(root, &[], FormatOptions::default()).unwrap();

        // Second pass — nothing should change
        let result2 = format_dir(root, &[], FormatOptions::default()).unwrap();
        assert_eq!(
            result2.files_changed, 0,
            "second pass changed files — fix is not idempotent"
        );
    }

    #[test]
    fn test_fix_h1_mismatch_idempotent() {
        assert_idempotent(&[
            (".typedown/doc.yaml", "structure:\n  title: from_filename\n"),
            ("my-doc.md", "---\ntype: doc\n---\n# Wrong Title\n"),
        ]);
    }

    #[test]
    fn test_fix_missing_h1_idempotent() {
        assert_idempotent(&[
            (".typedown/doc.yaml", "structure:\n  title: from_filename\n"),
            ("my-doc.md", "---\ntype: doc\n---\nNo heading here.\n"),
        ]);
    }

    #[test]
    fn test_fix_section_intro_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "structure:\n  sections:\n    - title: Goals\n      intro_text: 'Explicitly:'\n",
            ),
            (
                "doc.md",
                "---\ntype: doc\n---\n# Doc\n\n## Goals\n\n- item\n",
            ),
        ]);
    }

    #[test]
    fn test_fix_managed_section_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "structure:\n  sections:\n    - title: Related\n      managed_content:\n        template: |\n          ## Related\n\n          - [README](README.md)\n",
            ),
            ("doc.md", "---\ntype: doc\n---\n# Doc\n\nSome intro.\n"),
            ("README.md", "# README\n"),
        ]);
    }

    #[test]
    fn test_fix_empty_optional_section_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "structure:\n  sections:\n    - title: Notes\n      required: false\n",
            ),
            ("doc.md", "---\ntype: doc\n---\n# Doc\n\n## Notes\n"),
        ]);
    }

    #[test]
    fn test_fix_paragraph_to_bullet_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "structure:\n  sections:\n    - title: Goals\n      bullets: any\n",
            ),
            (
                "doc.md",
                "---\ntype: doc\n---\n# Doc\n\n## Goals\n\nThis is a paragraph.\n",
            ),
        ]);
    }

    #[test]
    fn test_fix_list_type_conversion_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "structure:\n  sections:\n    - title: Steps\n      bullets: ordered\n",
            ),
            (
                "doc.md",
                "---\ntype: doc\n---\n# Doc\n\n## Steps\n\n- First\n- Second\n",
            ),
        ]);
    }

    #[test]
    fn test_fix_section_reorder_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "structure:\n  sections:\n    - title: Alpha\n    - title: Beta\n    - title: Gamma\n",
            ),
            (
                "doc.md",
                "---\ntype: doc\n---\n# Doc\n\n## Gamma\n\nG content.\n\n## Alpha\n\nA content.\n\n## Beta\n\nB content.\n",
            ),
        ]);
    }

    #[test]
    fn test_fix_section_reorder_preserves_content() {
        let dir = make_tree(&[
            (
                ".typedown/doc.yaml",
                "structure:\n  sections:\n    - title: Alpha\n    - title: Beta\n",
            ),
            (
                "doc.md",
                "---\ntype: doc\n---\n# Doc\n\n## Beta\n\nBeta content.\n\n## Alpha\n\nAlpha content.\n",
            ),
        ]);

        format_dir(dir.path(), &[], FormatOptions::default()).unwrap();

        let result = fs::read_to_string(dir.path().join("doc.md")).unwrap();
        let alpha_pos = result.find("## Alpha").expect("Alpha present");
        let beta_pos = result.find("## Beta").expect("Beta present");
        assert!(alpha_pos < beta_pos, "Alpha should come before Beta: {result}");
        assert!(result.contains("Alpha content."), "Alpha content preserved: {result}");
        assert!(result.contains("Beta content."), "Beta content preserved: {result}");

        // Second pass should be clean
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(errors.is_empty(), "should be clean after fmt: {errors:?}");
    }

    // ── Path-based type matching ──────────────────────────────────────────────

    #[test]
    fn test_path_match_no_type_frontmatter() {
        // File matched by path pattern, no `type:` in frontmatter → validates cleanly
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "paths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
            ),
            ("README.md", "---\ncreated: 2026-01-01\n---\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        // Should not complain about missing `type:` field
        let type_errors: Vec<_> = errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .filter(|d| d.message().contains("type"))
            .collect();
        assert!(
            type_errors.is_empty(),
            "path-matched file should not require type: field, got: {type_errors:?}"
        );
    }

    #[test]
    fn test_path_match_no_frontmatter_at_all() {
        // File matched by path, no frontmatter, schema has no required fields → clean
        let dir = make_tree(&[
            (
                ".typedown/roadmap.yaml",
                "paths:\n  - \"**/ROADMAP.md\"\nstructure:\n  title: Roadmap\n  strict_sections: false\n",
            ),
            ("ROADMAP.md", "# Roadmap\n\nSome content.\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors.is_empty(),
            "path-matched file with no required fields should pass without frontmatter, got: {errors:?}"
        );
    }

    #[test]
    fn test_path_match_with_required_fields_reports_error() {
        // Path-matched, no frontmatter, but schema requires fields → error
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "paths:\n  - \"**/README.md\"\nfields:\n  description:\n    type: string\n    required: true\n",
            ),
            ("README.md", "# My Project\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            !errors.is_empty(),
            "path-matched file with required fields and no frontmatter should have errors"
        );
    }

    #[test]
    fn test_explicit_type_overrides_path_match() {
        // File has `type: other` in frontmatter even though path matches "readme"
        // → explicit type wins, gets unknown type error
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "paths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
            ),
            ("README.md", "---\ntype: other\n---\n# Title\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors
                .iter()
                .flat_map(|e| &e.diagnostics)
                .any(|d| d.message().contains("unknown type")),
            "explicit type: should override path match, got: {errors:?}"
        );
    }

    #[test]
    fn test_type_none_opts_out_despite_path_match() {
        // File has `type: none` even though path matches a schema → no validation
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "paths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
            ),
            ("README.md", "---\ntype: none\n---\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors.is_empty(),
            "type: none should opt out despite path match, got: {errors:?}"
        );
    }

    #[test]
    fn test_path_match_conflict_detected() {
        // Two schemas claim the same path → conflict diagnostic
        let dir = make_tree(&[
            (
                ".typedown/a.yaml",
                "paths:\n  - \"**/*.md\"\nstructure:\n  title: none\n",
            ),
            (
                ".typedown/b.yaml",
                "paths:\n  - \"docs/*.md\"\nstructure:\n  title: none\n",
            ),
            ("docs/hello.md", "# Hello\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        let conflict_errors: Vec<_> = errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .filter(|d| d.message().contains("multiple types"))
            .collect();
        assert!(
            !conflict_errors.is_empty(),
            "overlapping path patterns should produce conflict diagnostic, got: {errors:?}"
        );
    }

    #[test]
    fn test_path_match_recursive_glob() {
        // ** matches files at any depth
        let dir = make_tree(&[
            (
                ".typedown/agents.yaml",
                "paths:\n  - \"**/AGENTS.md\"\nstructure:\n  title: none\n  strict_sections: false\n",
            ),
            ("AGENTS.md", "---\ntype: agents\n---\n# Root Agents\n"),
            ("sub/AGENTS.md", "# Sub Agents\n"),
            ("deep/nested/AGENTS.md", "# Deep Agents\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors.is_empty(),
            "** glob should match at all depths, got: {errors:?}"
        );
    }

    #[test]
    fn test_path_match_fmt_idempotent() {
        // Path-matched file: td fmt should be idempotent
        assert_idempotent(&[
            (
                ".typedown/roadmap.yaml",
                "paths:\n  - \"**/ROADMAP.md\"\nstructure:\n  title: Roadmap\n  strict_sections: false\n",
            ),
            ("ROADMAP.md", "# Roadmap\n\nSome content.\n"),
        ]);
    }

    #[test]
    fn test_path_match_with_frontmatter_fields_validated() {
        // Path-matched file that does have frontmatter → fields still validated
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "paths:\n  - \"**/README.md\"\nfields:\n  created:\n    type: date\n    required: true\n",
            ),
            ("README.md", "---\ncreated: not-a-date\n---\n# Title\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors
                .iter()
                .flat_map(|e| &e.diagnostics)
                .any(|d| d.message().contains("valid date")),
            "path-matched files should still validate field types, got: {errors:?}"
        );
    }

    // ── Frontmatter serialization regression ────────────────────────────────

    #[test]
    fn test_no_duplicate_frontmatter_keys() {
        // Regression: fields defined in the schema should not produce duplicate
        // keys in the serialized frontmatter output.
        let dir = make_tree(&[
            (
                ".typedown/skill.yaml",
                "fields:\n  name:\n    type: string\n    required: true\n  description:\n    type: string\n    required: true\n",
            ),
            (
                "skill.md",
                "---\ntype: skill\nname: my-skill\ndescription: A cool skill\n---\n# my-skill\n",
            ),
        ]);

        format_dir(dir.path(), &[], FormatOptions::default()).unwrap();

        let content = fs::read_to_string(dir.path().join("skill.md")).unwrap();
        let name_count = content.matches("name:").count();
        let desc_count = content.matches("description:").count();
        assert_eq!(
            name_count, 1,
            "name should appear exactly once, got {name_count}. Content:\n{content}"
        );
        assert_eq!(
            desc_count, 1,
            "description should appear exactly once, got {desc_count}. Content:\n{content}"
        );
    }

    // ── Preset loading ────────────────────────────────────────────────────────

    #[test]
    fn test_presets_merged_into_project_schema() {
        // Set up XDG presets with a "readme" type
        let xdg_dir = TempDir::new().unwrap();
        let presets = xdg_dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("readme.yaml"),
            "paths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n  strict_sections: false\n",
        )
        .unwrap();

        // Project has a .typedown/ with only a "note" type — no readme
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "fields:\n  title:\n    type: string\n",
            ),
            ("README.md", "# test-presets-merged-into-project-schema\n"),
        ]);

        let _guard = XDG_LOCK.lock().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        let errors = check_dir(dir.path(), &[]).unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        drop(_guard);

        // README.md should be validated by the preset readme type (no errors)
        assert!(
            errors.is_empty(),
            "preset readme type should validate README.md, got: {errors:?}"
        );
    }

    #[test]
    fn test_project_local_overrides_preset() {
        // XDG preset: readme requires `description` field
        let xdg_dir = TempDir::new().unwrap();
        let presets = xdg_dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("readme.yaml"),
            "paths:\n  - \"**/README.md\"\nfields:\n  description:\n    type: string\n    required: true\n",
        )
        .unwrap();

        // Project overrides readme locally with NO required fields
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "paths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n  strict_sections: false\n",
            ),
            ("README.md", "# test-project-local-overrides-preset\n"),
        ]);

        let _guard = XDG_LOCK.lock().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        let errors = check_dir(dir.path(), &[]).unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        drop(_guard);

        // Should pass: project-local readme wins, no `description` required
        assert!(
            errors.is_empty(),
            "project-local schema should override preset, got: {errors:?}"
        );
    }

    #[test]
    fn test_presets_apply_without_local_schema_dir() {
        // XDG presets exist but project has no .typedown/ dir — presets
        // should still activate via a virtual root schema.
        let xdg_dir = TempDir::new().unwrap();
        let presets = xdg_dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("readme.yaml"),
            "paths:\n  - \"**/README.md\"\nfields:\n  created:\n    type: date\n    required: true\n",
        )
        .unwrap();

        // No .typedown/ dir in the project
        let dir = make_tree(&[("README.md", "# Hello\n")]);

        let _guard = XDG_LOCK.lock().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        let errors = check_dir(dir.path(), &[]).unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        drop(_guard);

        // Presets should validate: README.md is missing required `created` field
        assert!(
            !errors.is_empty(),
            "presets should apply without a .typedown/ dir"
        );
    }

    // ── Cross-project typed link validation ───────────────────────────────────

    /// Create two sibling project directories inside a temp parent, initialise
    /// a git repo at the parent (so `git_repo_root` has a ceiling to return),
    /// and return `(parent_tempdir, proj_a_path, proj_b_path)`.
    fn make_cross_project_tree(
        files_a: &[(&str, &str)],
        files_b: &[(&str, &str)],
    ) -> (TempDir, PathBuf, PathBuf) {
        let parent = TempDir::new().unwrap();
        let proj_a = parent.path().join("proj-a");
        let proj_b = parent.path().join("proj-b");

        for (rel, content) in files_a {
            let full = proj_a.join(rel);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
        }
        for (rel, content) in files_b {
            let full = proj_b.join(rel);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
        }

        // Git repo at parent — gives check_dir a real ceiling for schema walks.
        git2::Repository::init(parent.path()).unwrap();

        (parent, proj_a, proj_b)
    }

    /// Schema YAML: interest type with a Movies section requiring `target_type: movie`.
    const INTEREST_YAML: &str =
        "structure:\n  sections:\n    - title: Movies\n      links:\n        target_type: movie\n";

    /// Schema YAML: minimal movie type (no required fields).
    const MOVIE_YAML: &str = "description: A movie\n";

    #[test]
    fn test_cross_project_link_correct_type_no_error() {
        // interest.md in proj-a links to a movie in proj-b;
        // proj-b has .typedown/movie.yaml — td should discover it and not fire.
        let (_parent, proj_a, _proj_b) = make_cross_project_tree(
            &[
                (".typedown/interest.yaml", INTEREST_YAML),
                (
                    "interest.md",
                    "---\ntype: interest\n---\n# Test Interest\n\n## Movies\n\n- [Some Movie](../proj-b/movies/some-movie.md)\n",
                ),
            ],
            &[
                (".typedown/movie.yaml", MOVIE_YAML),
                ("movies/some-movie.md", "---\ntype: movie\n---\n# Some Movie\n"),
            ],
        );

        let errors = check_dir(&proj_a, &[]).unwrap();
        let type_mismatches: Vec<_> = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter())
            .filter(|d| matches!(d, Diagnostic::LinkTargetTypeMismatch { .. }))
            .collect();
        assert!(
            type_mismatches.is_empty(),
            "expected no LinkTargetTypeMismatch for correctly-typed cross-project link, got: {type_mismatches:?}"
        );
    }

    #[test]
    fn test_cross_project_link_wrong_type_reports_error() {
        // interest.md expects target_type: movie but links to a tvshow.
        let (_parent, proj_a, _proj_b) = make_cross_project_tree(
            &[
                (".typedown/interest.yaml", INTEREST_YAML),
                (
                    "interest.md",
                    "---\ntype: interest\n---\n# Test Interest\n\n## Movies\n\n- [Some Show](../proj-b/tvshows/some-show.md)\n",
                ),
            ],
            &[
                (".typedown/tvshow.yaml", "description: A TV show\n"),
                (
                    "tvshows/some-show.md",
                    "---\ntype: tvshow\n---\n# Some Show\n",
                ),
            ],
        );

        let errors = check_dir(&proj_a, &[]).unwrap();
        let has_mismatch = errors.iter().flat_map(|e| e.diagnostics.iter()).any(|d| {
            matches!(d, Diagnostic::LinkTargetTypeMismatch { expected, actual: Some(actual), .. }
                    if expected == "movie" && actual == "tvshow")
        });
        assert!(
            has_mismatch,
            "expected LinkTargetTypeMismatch(expected=movie, actual=tvshow), got: {errors:?}"
        );
    }

    #[test]
    fn test_cross_project_link_untyped_target_reports_error() {
        // Target file has no `type:` frontmatter and proj-b has no .typedown/;
        // td cannot determine the type → LinkTargetTypeMismatch with actual: None.
        let (_parent, proj_a, _proj_b) = make_cross_project_tree(
            &[
                (".typedown/interest.yaml", INTEREST_YAML),
                (
                    "interest.md",
                    "---\ntype: interest\n---\n# Test Interest\n\n## Movies\n\n- [Some Movie](../proj-b/movies/some-movie.md)\n",
                ),
            ],
            &[
                // No .typedown/ in proj-b, no type: in target frontmatter
                ("movies/some-movie.md", "# Some Movie\n"),
            ],
        );

        let errors = check_dir(&proj_a, &[]).unwrap();
        let has_mismatch = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter())
            .any(|d| matches!(d, Diagnostic::LinkTargetTypeMismatch { actual: None, .. }));
        assert!(
            has_mismatch,
            "expected LinkTargetTypeMismatch(actual:None) for untyped cross-project target, got: {errors:?}"
        );
    }

    #[test]
    fn test_cross_project_bidi_backlink_present_no_error() {
        // interest.md (proj-a) links to personality (proj-b) with bidirectional: true.
        // personality.md has a backlink in "Related Interests" section.
        let interest_yaml = "structure:\n  sections:\n    - title: Personalities\n      links:\n        target_type: personality\n        bidirectional: true\n";
        let personality_yaml = "structure:\n  sections:\n    - title: Related Interests\n      links:\n        target_type: interest\n";

        let (_parent, proj_a, _proj_b) = make_cross_project_tree(
            &[
                (".typedown/interest.yaml", interest_yaml),
                (
                    "interest.md",
                    "---\ntype: interest\n---\n# Test Interest\n\n## Personalities\n\n- [Alice](../proj-b/personalities/alice.md)\n",
                ),
            ],
            &[
                (".typedown/personality.yaml", personality_yaml),
                (
                    "personalities/alice.md",
                    // two levels up: personalities/ → proj-b/ → parent/ → proj-a/
                    "---\ntype: personality\n---\n# Alice\n\n## Related Interests\n\n- [Test Interest](../../proj-a/interest.md)\n",
                ),
            ],
        );

        let errors = check_dir(&proj_a, &[]).unwrap();
        let backlink_errors: Vec<_> = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter())
            .filter(|d| matches!(d, Diagnostic::MissingBacklink { .. }))
            .collect();
        assert!(
            backlink_errors.is_empty(),
            "expected no MissingBacklink when cross-project backlink is present, got: {backlink_errors:?}"
        );
    }

    #[test]
    fn test_cross_project_bidi_backlink_missing_reports_error() {
        // Same setup but personality.md has no backlink → MissingBacklink.
        let interest_yaml = "structure:\n  sections:\n    - title: Personalities\n      links:\n        target_type: personality\n        bidirectional: true\n";
        let personality_yaml = "structure:\n  sections:\n    - title: Related Interests\n      links:\n        target_type: interest\n";

        let (_parent, proj_a, _proj_b) = make_cross_project_tree(
            &[
                (".typedown/interest.yaml", interest_yaml),
                (
                    "interest.md",
                    "---\ntype: interest\n---\n# Test Interest\n\n## Personalities\n\n- [Alice](../proj-b/personalities/alice.md)\n",
                ),
            ],
            &[
                (".typedown/personality.yaml", personality_yaml),
                (
                    "personalities/alice.md",
                    // Related Interests section exists but has no link back
                    "---\ntype: personality\n---\n# Alice\n\n## Related Interests\n\n- placeholder\n",
                ),
            ],
        );

        let errors = check_dir(&proj_a, &[]).unwrap();
        let has_missing_backlink = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter())
            .any(|d| matches!(d, Diagnostic::MissingBacklink { .. }));
        assert!(
            has_missing_backlink,
            "expected MissingBacklink when cross-project backlink is absent, got: {errors:?}"
        );
    }

    #[test]
    fn test_cross_project_preset_type_merged_into_external_schema() {
        // proj-a has interest.yaml requiring target_type: movie in Movies section.
        // proj-b has .typedown/ with a different type — the "movie" type comes
        // from an XDG preset that gets merged into proj-b's schema.
        let xdg_dir = TempDir::new().unwrap();
        let presets = xdg_dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("movie.yaml"),
            "description: A movie\npaths:\n  - \"movies/*.md\"\n",
        )
        .unwrap();

        let (_parent, proj_a, _proj_b) = make_cross_project_tree(
            &[
                (".typedown/interest.yaml", INTEREST_YAML),
                (
                    "interest.md",
                    "---\ntype: interest\n---\n# Test Interest\n\n## Movies\n\n- [Some Movie](../proj-b/movies/some-movie.md)\n",
                ),
            ],
            &[
                // proj-b has .typedown/ but no movie type — it comes from preset
                (".typedown/other.yaml", "description: Other type\n"),
                ("movies/some-movie.md", "# Some Movie\n"),
            ],
        );

        let _guard = XDG_LOCK.lock().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg_dir.path());
        let errors = check_dir(&proj_a, &[]).unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        drop(_guard);

        let type_mismatches: Vec<_> = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter())
            .filter(|d| matches!(d, Diagnostic::LinkTargetTypeMismatch { .. }))
            .collect();
        assert!(
            type_mismatches.is_empty(),
            "preset movie type should resolve for cross-project target, got: {type_mismatches:?}"
        );
    }

}
