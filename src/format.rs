//! Orchestration: walk directories, detect types, validate, fix, write.
//!
//! All filesystem I/O lives here. `validate.rs` and `fix.rs` are pure.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use rayon::prelude::*;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use tracing::debug;

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

/// Where the preset overlay that merges under every project schema comes from.
///
/// Production always uses [`PresetSource::Xdg`]; tests name their overlay
/// directory outright.  The distinction exists because `XDG_CONFIG_HOME` is
/// process-global: a test that set it while its neighbours ran would change
/// which presets *their* `check_dir` loaded, and a test whose temp dir was
/// deleted while a neighbour was mid-load would hand that neighbour a
/// "preset error: ... No such file or directory" it never asked for.
#[derive(Debug)]
pub(crate) enum PresetSource {
    /// `$XDG_CONFIG_HOME/typedown/presets/`, if it exists.
    Xdg,
    /// An explicit overlay directory, or none at all (built-ins only).
    /// Only the test suite names one; production always reads XDG.
    #[cfg_attr(not(test), allow(dead_code))]
    Overlay(Option<PathBuf>),
}

impl PresetSource {
    /// The overlay directory this source resolves to, if any.
    ///
    /// Under `cfg(test)` the XDG source resolves to nothing: whether the
    /// developer running the suite happens to have `~/.config/typedown/presets/`
    /// is not something a test result may depend on.
    fn dir(&self) -> Option<PathBuf> {
        match self {
            PresetSource::Xdg if cfg!(test) => None,
            PresetSource::Xdg => schema::presets_dir(),
            PresetSource::Overlay(dir) => dir.clone(),
        }
    }

    /// Built-in presets with this source's overlay merged on top.
    fn load(&self) -> (Option<Schema>, Option<anyhow::Error>) {
        schema::load_presets_from(self.dir())
    }
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
/// - Walks the directory tree (respecting `.gitignore` and `.ignore`; always skips `.git/`).
/// - For each `.md` file, finds the nearest `.typedown/` schema dir (walking up to `root`).
/// - Reads the `type` field from frontmatter to pick the schema type.
/// - If no `type:` is present, tries path-pattern matching from `paths:` in schemas.
/// - Validates, applies fixes (unless `check` mode), and writes if changed.
pub fn format_dir(
    root: &Path,
    explicit_paths: &[PathBuf],
    opts: FormatOptions,
) -> Result<FormatResult> {
    format_dir_with(root, explicit_paths, opts, &PresetSource::Xdg)
}

/// [`format_dir`] against an explicit preset overlay.
pub(crate) fn format_dir_with(
    root: &Path,
    explicit_paths: &[PathBuf],
    opts: FormatOptions,
    presets: &PresetSource,
) -> Result<FormatResult> {
    let mut result = FormatResult::default();

    // Pre-load schemas: schema_dir → Schema, and build path matchers.
    let (schemas, matchers) = load_all_schemas_with(root, presets, &mut result);
    debug!(schema_dirs = schemas.len(), "loaded schemas");

    // Pre-load linked-doc info for bidirectional link validation.
    // Key: absolute path → LinkedDocInfo.  Also returns a doc cache so
    // format_file can skip re-reading/re-parsing files it already has.
    let (mut linked_docs, doc_cache) = preload_linked_docs(root, &schemas, &matchers);
    debug!(linked_docs = linked_docs.len(), "preloaded linked docs");

    // Pre-load git-tracked paths for cross-project link validation.
    let git_tree = crate::git::read_head_tree(root);

    // Discover and pre-load cross-project link targets for typed link validation.
    // Uses the git repo root as the ceiling for external schema discovery.
    let external_types = if let Some(repo_root) = crate::git::git_repo_root(root) {
        let ext_targets = collect_external_targets(&linked_docs, root);
        if ext_targets.is_empty() {
            HashMap::new()
        } else {
            let (preset_schema, _) = presets.load();
            let (ext_linked, ext_types) =
                preload_external_link_targets(&ext_targets, &repo_root, preset_schema.as_ref());
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

    // Schema files themselves get one fix: drop the `# yaml-language-server:`
    // modeline. It pointed at typedown's `src/schema.json` by relative path,
    // which only resolves when the typedown repo happens to sit beside the
    // project. `td schema` prints that schema from the binary instead.
    if !opts.check {
        let (seen, stripped) = strip_schema_modelines(schemas.keys().map(PathBuf::as_path));
        result.files_checked += seen;
        result.files_changed += stripped;
    }

    // Process files in parallel.  Each format_file call is independent: it
    // reads (from cache), applies fixes, and writes its own file.
    type FormatOutcome = Result<(Option<bool>, Vec<Diagnostic>, PathBuf), (PathBuf, anyhow::Error)>;
    let outcomes: Vec<FormatOutcome> = paths
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
    check_dir_with(root, explicit_paths, &PresetSource::Xdg)
}

/// [`check_dir`] against an explicit preset overlay.
pub(crate) fn check_dir_with(
    root: &Path,
    explicit_paths: &[PathBuf],
    presets: &PresetSource,
) -> Result<Vec<FileError>> {
    let mut result = FormatResult::default();
    let (schemas, matchers) = load_all_schemas_with(root, presets, &mut result);
    debug!(schema_dirs = schemas.len(), "loaded schemas");
    let (mut linked_docs, doc_cache) = preload_linked_docs(root, &schemas, &matchers);
    debug!(linked_docs = linked_docs.len(), "preloaded linked docs");
    let git_tree = crate::git::read_head_tree(root);

    let external_types = if let Some(repo_root) = crate::git::git_repo_root(root) {
        let ext_targets = collect_external_targets(&linked_docs, root);
        if ext_targets.is_empty() {
            HashMap::new()
        } else {
            let (preset_schema, _) = presets.load();
            let (ext_linked, ext_types) =
                preload_external_link_targets(&ext_targets, &repo_root, preset_schema.as_ref());
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
    git_tree: Option<&crate::git::GitTree>,
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
                schema_root: schema_dir.parent(),
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
        ResolvedType::Unknown => {
            // Virtual root schema (from built-in/XDG presets, no local .typedown/)
            // only validates files that match a path pattern. Unmatched files are
            // silently skipped — the project didn't opt into full schema coverage.
            if !schema_dir.exists() {
                debug!(file = %rel.display(), "no path match under virtual schema, skipping");
                return Ok((None, vec![]));
            }
            validate_unknown_type(&doc, schema)
        }
    };

    // Scan raw source for malformed links (spaces in URLs that the parser
    // silently drops as non-links).
    diagnostics.extend(detect_malformed_links(&content));

    // Path-matched files with no frontmatter: suppress MissingFrontmatter if
    // the schema has no required fields/frontmatter entries.
    if matches!(&resolved, ResolvedType::PathMatched(..)) && doc.frontmatter.is_none() {
        suppress_missing_frontmatter(&mut diagnostics, &resolved);
    }

    let frontmatter_error = report_frontmatter_error(&mut diagnostics, &content, &doc);

    if !diagnostics.is_empty() {
        debug!(file = %rel.display(), count = diagnostics.len(), "diagnostics found");
    }

    // Partition into fixable and unfixable diagnostics.
    // Apply fixes for fixable ones; return unfixable ones to the caller.
    let (fixable, unfixable): (Vec<_>, Vec<_>) = diagnostics
        .into_iter()
        .partition(crate::fix::Fix::is_fixable);

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
            order.extend(td.frontmatter_field_order());
            order
        })
    });

    // A frontmatter block that didn't parse isn't in the AST, so rewriting the
    // file would drop it on the floor. Report the error and leave the file be.
    if frontmatter_error {
        debug!(file = %rel.display(), "frontmatter did not parse, not rewriting");
        return Ok((None, unfixable));
    }

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
    git_tree: Option<&crate::git::GitTree>,
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
    git_tree: Option<&crate::git::GitTree>,
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
                schema_root: schema_dir.parent(),
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
        ResolvedType::Unknown => {
            if !schema_dir.exists() {
                debug!(file = %rel.display(), "no path match under virtual schema, skipping");
                return vec![];
            }
            validate_unknown_type(doc, schema)
        }
    };

    // Suppress MissingFrontmatter for path-matched files with no required fields
    if matches!(&resolved, ResolvedType::PathMatched(..)) && doc.frontmatter.is_none() {
        suppress_missing_frontmatter(&mut diagnostics, &resolved);
    }

    report_frontmatter_error(&mut diagnostics, content, doc);

    diagnostics.extend(detect_malformed_links(content));
    diagnostics
}

/// Report a frontmatter block that exists but doesn't parse. Returns whether
/// there was one.
///
/// A block that fails to deserialize isn't in the AST, so every other check has
/// just run against a document that looks like it has no frontmatter at all —
/// and the suppression rules above, which exist for files that genuinely have
/// none, will happily drop the one diagnostic that hinted at it. So this is
/// reported unconditionally rather than by upgrading a `MissingFrontmatter` that
/// may never have been raised or may already have been suppressed.
///
/// Being silent here is worse than an ordinary missed diagnostic: `format_file`
/// refuses to rewrite a file it can't parse the frontmatter of, so `td fmt`
/// leaves it alone forever. Without a diagnostic, nothing tells anyone why.
fn report_frontmatter_error(
    diagnostics: &mut Vec<Diagnostic>,
    content: &str,
    doc: &Document,
) -> bool {
    // Short-circuit only: `get_frontmatter_error` would return `None` here
    // anyway, but it costs a second YAML parse to say so.
    if doc.frontmatter.is_some() {
        return false;
    }
    let Some(parse_err) = get_frontmatter_error(content) else {
        return false;
    };
    // `MissingFrontmatter` describes the same block, less usefully.
    diagnostics.retain(|d| !matches!(d, Diagnostic::MissingFrontmatter));
    diagnostics.insert(
        0,
        Diagnostic::UnknownType {
            line: 1,
            message: format!("frontmatter parse error: {parse_err}"),
        },
    );
    true
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
    load_all_schemas_with(root, &PresetSource::Xdg, result)
}

/// [`load_all_schemas`] against an explicit preset overlay.
pub(crate) fn load_all_schemas_with(
    root: &Path,
    source: &PresetSource,
    result: &mut FormatResult,
) -> (HashMap<PathBuf, Schema>, HashMap<PathBuf, PathMatcher>) {
    let mut schemas: HashMap<PathBuf, Schema> = HashMap::new();
    let mut matchers: HashMap<PathBuf, PathMatcher> = HashMap::new();
    let (presets, preset_error) = source.load();
    if let Some(ref p) = presets {
        debug!(types = p.types.len(), "loaded presets");
    }
    if let Some(e) = preset_error {
        // Reported against the preset directory itself: no project file is at
        // fault, and the one that would be blamed instead is whichever document
        // the broken preset stopped typing.
        result.errors.push(FileError {
            path: source.dir().unwrap_or_default(),
            diagnostics: vec![Diagnostic::UnknownType {
                line: 0,
                message: format!("preset error: {e:#}"),
            }],
        });
    }

    for entry in walk(root).filter_map(|e| e.ok()) {
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
                    // Lint before merging presets, so a preset's problem is only
                    // ever reported against the project that actually defines it.
                    report_vacuous_templates(&schema, path, result);
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
                            // `{e:#}` so the cause chain (which file, which rule)
                            // survives — the outer context alone is just a path.
                            message: format!("schema error: {e:#}"),
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

/// Report every section `template:` in `schema` that accepts any list item.
///
/// Reported against the `.typedown/` directory rather than a document: the
/// problem is in the schema, and no document can be at fault for it.
fn report_vacuous_templates(schema: &Schema, schema_dir: &Path, result: &mut FormatResult) {
    let diagnostics: Vec<Diagnostic> = schema
        .vacuous_templates()
        .into_iter()
        .map(|v| Diagnostic::VacuousTemplate {
            type_name: v.type_name,
            section: v.section,
            template: v.template,
        })
        .collect();
    if !diagnostics.is_empty() {
        result.errors.push(FileError {
            path: schema_dir.to_path_buf(),
            diagnostics,
        });
    }
}

/// Remove `# yaml-language-server:` modelines from the YAML files in each of
/// `schema_dirs`. Returns `(files seen, files rewritten)`.
///
/// Skips directories that don't exist on disk — `load_all_schemas` can hand
/// back a virtual `.typedown/` when only presets are active.
fn strip_schema_modelines<'a>(schema_dirs: impl Iterator<Item = &'a Path>) -> (usize, usize) {
    let (mut seen, mut stripped) = (0, 0);
    for dir in schema_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("yaml") | Some("yml")) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            seen += 1;
            let Some(cleaned) = strip_modeline(&content) else {
                continue;
            };
            match std::fs::write(&path, cleaned) {
                Ok(()) => {
                    debug!(file = %path.display(), "dropped yaml-language-server modeline");
                    stripped += 1;
                }
                Err(e) => debug!(file = %path.display(), error = %e, "modeline rewrite failed"),
            }
        }
    }
    (seen, stripped)
}

/// Drop every `# yaml-language-server:` line from `content`, plus any blank
/// lines the removal leaves at the top. Returns `None` when there is no
/// modeline, so untouched files are never rewritten.
fn strip_modeline(content: &str) -> Option<String> {
    let is_modeline = |l: &str| l.trim_start().starts_with("# yaml-language-server:");
    if !content.lines().any(is_modeline) {
        return None;
    }
    let mut out = String::with_capacity(content.len());
    for line in content.lines().filter(|l| !is_modeline(l)) {
        out.push_str(line);
        out.push('\n');
    }
    Some(out.trim_start_matches('\n').to_string())
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

    for entry in walk(root).filter_map(|e| e.ok()) {
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
    crate::ast::links(blocks)
        .into_iter()
        .map(|link| link.url.to_string())
        .collect()
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
        if !type_def.has_required_frontmatter() {
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
        return walk(root)
            .filter_map(|e| e.ok())
            .filter(|e| is_markdown(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect();
    }

    let mut paths = Vec::new();
    for p in explicit_paths {
        if p.is_dir() {
            let walked: Vec<PathBuf> = walk(p)
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

/// Build a recursive directory walker that respects `.gitignore`, `.ignore`,
/// `.git/info/exclude`, and the user's global gitignore. Always skips `.git/`
/// directories. Hidden files/dirs (dotfiles) are NOT skipped — projects
/// commonly keep markdown in `.github/`, `.claude/`, etc.
///
/// Works outside git repos: `.gitignore` / `.ignore` files are still read and
/// applied relative to the directory they live in.
pub(crate) fn walk(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
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

    /// Write a frontmatter-less `README.md` whose H1 is the temp dir's name,
    /// satisfying `title: from_directory`.
    fn write_readme_titled_for_dir(dir: &TempDir) {
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        fs::write(dir.path().join("README.md"), format!("# {name}\n")).unwrap();
    }

    // ── modeline stripping ────────────────────────────────────────────────────

    #[test]
    fn test_strip_modeline_removes_line_and_leading_blank() {
        let src = "# yaml-language-server: $schema=../../typedown/src/schema.json\n\ndescription: An album\n";
        assert_eq!(
            strip_modeline(src).unwrap(),
            "description: An album\n",
            "modeline and the blank line it left behind should both go"
        );
    }

    #[test]
    fn test_strip_modeline_keeps_other_comments() {
        let src = "# yaml-language-server: $schema=../schema.json\n# A real comment\ndescription: A person\n";
        assert_eq!(
            strip_modeline(src).unwrap(),
            "# A real comment\ndescription: A person\n"
        );
    }

    #[test]
    fn test_strip_modeline_returns_none_without_one() {
        assert!(strip_modeline("description: A person\n").is_none());
    }

    #[test]
    fn test_format_dir_strips_schema_modelines() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\n# yaml-language-server: $schema=../../typedown/src/schema.json\ndescription: A note\n",
            ),
            ("notes/hello.md", "# Hello\n\nJust a note.\n"),
        ]);
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join(".typedown/note.yaml")).unwrap(),
            "version: 2\ndescription: A note\n"
        );
        assert!(result.files_changed >= 1);
    }

    #[test]
    fn test_check_dir_leaves_schema_modelines_alone() {
        let original =
            "# yaml-language-server: $schema=../../typedown/src/schema.json\ndescription: A note\n";
        let dir = make_tree(&[
            (".typedown/note.yaml", original),
            ("notes/hello.md", "# Hello\n\nJust a note.\n"),
        ]);
        let _ = check_dir(dir.path(), &[]).unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join(".typedown/note.yaml")).unwrap(),
            original,
            "check must not write"
        );
    }

    // ── format_dir ────────────────────────────────────────────────────────────

    #[test]
    fn test_format_dir_no_schema_no_changes() {
        let dir = make_tree(&[("notes/hello.md", "# Hello\n\nJust a note.\n")]);
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        assert_eq!(result.files_changed, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_format_dir_valid_doc_no_changes() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    title:\n      type: string\n  required: [title]\n",
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
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    title:\n      type: string\n  required: [title]\n",
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

    #[test]
    fn test_format_dir_from_date_day_precision_leaves_h1_alone() {
        let dir = make_tree(&[
            (
                ".typedown/day.yaml",
                "version: 2\nstructure:\n  title: from_date\n",
            ),
            (
                "days/2026-04-14.md",
                "---\ntype: day\n---\n# April 14, 2026\n",
            ),
        ]);
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("days/2026-04-14.md")).unwrap(),
            "---\ntype: day\n---\n# April 14, 2026\n"
        );
        assert_eq!(result.files_changed, 0);
        assert!(result.errors.is_empty(), "got: {:?}", result.errors);
    }

    #[test]
    fn test_format_dir_from_date_undated_filename_keeps_h1() {
        let dir = make_tree(&[
            (
                ".typedown/day.yaml",
                "version: 2\nstructure:\n  title: from_date\n",
            ),
            ("days/notes.md", "---\ntype: day\n---\n# Scratch Notes\n"),
        ]);
        let original = fs::read_to_string(dir.path().join("days/notes.md")).unwrap();
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("days/notes.md")).unwrap(),
            original,
            "fmt must never delete an H1 it can't derive a replacement for"
        );
        assert_eq!(result.files_changed, 0);
        assert_eq!(result.errors.len(), 1, "got: {:?}", result.errors);
    }

    // ── check_dir ─────────────────────────────────────────────────────────────

    #[test]
    fn test_check_dir_reports_errors() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    title:\n      type: string\n  required: [title]\n",
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

    /// A container document plus season docs beside it, one of which the
    /// container's `## Seasons` section may or may not link.
    fn seasons_tree(seasons_section: &str) -> TempDir {
        make_tree(&[
            (
                ".typedown/tvshow.yaml",
                "version: 2\nstructure:\n  strict_sections: false\n  sections:\n    - title: Seasons\n\
                 correspondence:\n  - each: docs-of-type tvseason in-directory \".\"\n    \
                 requires:\n      link-in: \"## Seasons\"\n",
            ),
            (
                ".typedown/tvseason.yaml",
                "version: 2\nstructure:\n  strict_sections: false\n",
            ),
            (
                "3rd Rock/Show.md",
                &format!("---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n{seasons_section}"),
            ),
            (
                "3rd Rock/Season 1.md",
                "---\ntype: tvseason\n---\n\n# Season 1\n",
            ),
            (
                "3rd Rock/Season 2.md",
                "---\ntype: tvseason\n---\n\n# Season 2\n",
            ),
        ])
    }

    #[test]
    fn test_check_dir_reports_a_doc_the_section_never_links() {
        let dir = seasons_tree("\n- [Season 1](Season%201.md)\n");
        let errors = check_dir(dir.path(), &[]).unwrap();
        let messages: Vec<String> = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter().map(|d| d.message()))
            .collect();
        assert_eq!(
            messages,
            ["'Season 2.md' is a tvseason document that '## Seasons' doesn't link"],
            "got: {errors:?}"
        );
    }

    #[test]
    fn test_check_dir_is_clean_when_every_sibling_doc_is_linked() {
        let dir = seasons_tree("\n- [Season 1](Season%201.md)\n- [Season 2](Season%202.md)\n");
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(errors.is_empty(), "got: {errors:?}");
    }

    // ── unparseable frontmatter ───────────────────────────────────────────────
    //
    // A frontmatter block that doesn't deserialize is absent from the AST, so
    // the document looks frontmatter-less to every other check. `td fmt` won't
    // rewrite such a file — it would drop the block — so if nothing reports the
    // parse error, the file is stuck with nothing to say why.

    /// A schema matching every `.md` by path, requiring nothing. Reproduces the
    /// suppression path: `MissingFrontmatter` is dropped for path-matched files
    /// with no required fields, which used to take the parse error with it.
    const PERMISSIVE_SCHEMA: &str =
        "version: 2\npaths:\n  - \"**/*.md\"\nstructure:\n  strict_sections: false\n";

    /// `item: - item` — a block sequence entry where a value belongs.
    const BROKEN_FRONTMATTER: &str = "---\nitem: - item\n---\n# Doc\n";

    fn frontmatter_error_reported(errors: &[FileError]) -> bool {
        errors.iter().any(|e| {
            e.diagnostics
                .iter()
                .any(|d| d.message().contains("frontmatter parse error"))
        })
    }

    #[test]
    fn test_check_dir_reports_unparseable_frontmatter() {
        let dir = make_tree(&[
            (".typedown/note.yaml", PERMISSIVE_SCHEMA),
            ("note.md", BROKEN_FRONTMATTER),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            frontmatter_error_reported(&errors),
            "check must report a frontmatter block that doesn't parse, got: {errors:?}"
        );
    }

    #[test]
    fn test_check_dir_reports_unparseable_frontmatter_with_required_fields() {
        // The other resolution path: an explicit type can't be read either, so
        // the file falls through to unknown-type validation.
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    title:\n      type: string\n  required: [title]\n",
            ),
            ("note.md", BROKEN_FRONTMATTER),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            frontmatter_error_reported(&errors),
            "check must report the parse error, got: {errors:?}"
        );
    }

    #[test]
    fn test_fmt_reports_unparseable_frontmatter_and_leaves_file_alone() {
        let dir = make_tree(&[
            (".typedown/note.yaml", PERMISSIVE_SCHEMA),
            ("note.md", BROKEN_FRONTMATTER),
        ]);
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        assert!(
            frontmatter_error_reported(&result.errors),
            "fmt must report the parse error, got: {:?}",
            result.errors
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("note.md")).unwrap(),
            BROKEN_FRONTMATTER,
            "fmt must not rewrite a file whose frontmatter it couldn't parse"
        );
    }

    #[test]
    fn test_genuinely_absent_frontmatter_is_not_a_parse_error() {
        // No block at all is a different thing, and under this schema it's fine.
        let dir = make_tree(&[
            (".typedown/note.yaml", PERMISSIVE_SCHEMA),
            ("note.md", "# Doc\n\nNo frontmatter here.\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            !frontmatter_error_reported(&errors),
            "a file with no frontmatter must not be reported as a parse error: {errors:?}"
        );
    }

    #[test]
    fn test_check_dir_clean_doc_no_errors() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    title:\n      type: string\n  required: [title]\n",
            ),
            ("note.md", "---\ntype: note\ntitle: Hello\n---\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }

    #[test]
    fn test_check_dir_no_schema_no_errors() {
        let dir = make_tree(&[("notes/hello.md", "# Hello\n")]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn test_check_mode_does_not_write() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\nstructure:\n  title: from_filename\n",
            ),
            // Title doesn't match filename — fixable, but check mode shouldn't write
            ("my-note.md", "---\ntype: note\n---\n# Wrong Title\n"),
        ]);

        let original = fs::read_to_string(dir.path().join("my-note.md")).unwrap();
        let _result = format_dir(dir.path(), &[], FormatOptions { check: true }).unwrap();
        let after = fs::read_to_string(dir.path().join("my-note.md")).unwrap();
        assert_eq!(original, after, "check mode should not modify files");
    }

    #[test]
    fn test_unparseable_frontmatter_is_not_rewritten_away() {
        // The block isn't in the AST, so serializing would silently delete it.
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\nstructure:\n  title: from_filename\n",
            ),
            (
                "my-note.md",
                "---\ntype: note\ntitle: [unclosed\n---\n# Doc\n",
            ),
        ]);

        let original = fs::read_to_string(dir.path().join("my-note.md")).unwrap();
        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        let after = fs::read_to_string(dir.path().join("my-note.md")).unwrap();
        assert_eq!(original, after, "broken frontmatter must survive td fmt");
        assert!(
            result.errors.iter().any(|e| e
                .diagnostics
                .iter()
                .any(|d| d.message().contains("frontmatter parse error"))),
            "the parse error should be reported: {:?}",
            result.errors
        );
    }

    // ── Schema in subdirectory ─────────────────────────────────────────────────

    #[test]
    fn test_schema_in_subdir_covers_files_below_it() {
        let dir = make_tree(&[
            (
                "projects/.typedown/task.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    owner:\n      type: string\n  required: [owner]\n",
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

    // ── Vacuous template lint ─────────────────────────────────────────────────

    #[test]
    fn test_vacuous_template_reported_against_the_schema_dir() {
        let dir = make_tree(&[
            (
                ".typedown/movie.yaml",
                "version: 2\nstructure:\n  sections:\n    - title: Notes\n      template: '- Fact or impression about the movie'\n",
            ),
            ("dune.md", "---\ntype: movie\n---\n## Notes\n\n- anything\n"),
        ]);

        let errors = check_dir(dir.path(), &[]).unwrap();
        let lint = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter().map(move |d| (&e.path, d)))
            .find(|(_, d)| matches!(d, Diagnostic::VacuousTemplate { .. }))
            .expect("expected a VacuousTemplate diagnostic");

        assert!(lint.0.ends_with(SCHEMA_DIR), "got: {}", lint.0.display());
        let message = lint.1.message();
        assert!(message.contains("movie"), "{message}");
        assert!(message.contains("Notes"), "{message}");
    }

    #[test]
    fn test_constraining_template_is_not_linted() {
        let dir = make_tree(&[
            (
                ".typedown/movie.yaml",
                "version: 2\nstructure:\n  sections:\n    - title: Notes\n      template: '- **Text**: Text'\n",
            ),
            (
                "dune.md",
                "---\ntype: movie\n---\n## Notes\n\n- **Score**: 9\n",
            ),
        ]);

        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            !errors.iter().any(|e| e
                .diagnostics
                .iter()
                .any(|d| matches!(d, Diagnostic::VacuousTemplate { .. }))),
            "got: {errors:?}"
        );
    }

    // ── Fix idempotency ───────────────────────────────────────────────────────
    //
    // Running `td fmt` twice on the same directory must produce identical output
    // on the second pass (no changes, no errors).  These tests verify that
    // each fixable diagnostic class doesn't re-trigger itself after being fixed.

    /// Every markdown file under `root`, as (relative path, content).
    fn read_markdown(root: &Path) -> BTreeMap<String, String> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
            .map(|e| {
                let rel = e.path().strip_prefix(root).unwrap().display().to_string();
                (rel, fs::read_to_string(e.path()).unwrap())
            })
            .collect()
    }

    /// Format `files` twice and assert the second pass is a no-op.
    ///
    /// `files_changed == 0` alone is not that assertion. It counts *writes*,
    /// and a fix that keeps rewriting a file to the same bytes reports zero;
    /// worse, it says nothing about what the first pass produced, so a pass
    /// that mangled the document and then sat still read as clean. Both passes
    /// are compared by content, and what the first one did to each file is
    /// returned for the caller to check.
    fn assert_idempotent(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        let dir = make_tree(files);
        let root = dir.path();

        // First pass — apply fixes
        let _result1 = format_dir(root, &[], FormatOptions::default()).unwrap();
        let after1 = read_markdown(root);

        // Second pass — nothing should change
        let result2 = format_dir(root, &[], FormatOptions::default()).unwrap();
        let after2 = read_markdown(root);
        assert_eq!(
            result2.files_changed, 0,
            "second pass changed files — fix is not idempotent"
        );
        for (path, before) in &after1 {
            assert_eq!(
                Some(before),
                after2.get(path),
                "{path} differs after a second pass — fix is not idempotent"
            );
        }
        assert_eq!(
            after1.keys().collect::<Vec<_>>(),
            after2.keys().collect::<Vec<_>>(),
            "the second pass added or removed files"
        );
        after1
    }

    /// Format `files` twice and assert nothing moved at all — not on the second
    /// pass, and not on the first.
    ///
    /// For input that is already correct, which is where a serializer bug hides:
    /// there is no fix to justify a rewrite, so every byte must come back.
    fn assert_unchanged(files: &[(&str, &str)]) {
        let after = assert_idempotent(files);
        for (path, content) in files {
            if !path.ends_with(".md") {
                continue;
            }
            assert_eq!(
                after.get(*path).map(String::as_str),
                Some(*content),
                "{path} was rewritten, but there was nothing to fix"
            );
        }
    }

    /// `td fmt` leaves a correct document exactly as it found it.
    ///
    /// The block constructs here are the ones a fix pass has no business
    /// touching. They are covered against `parse`/`serialize` directly in
    /// `parse.rs`; this is the same claim made about the real binary's write
    /// path, where a validate-and-fix round also gets a say.
    #[test]
    fn test_fmt_leaves_a_clean_document_alone() {
        assert_unchanged(&[
            (
                ".typedown/doc.yaml",
                "version: 2\nstructure:\n  title: from_filename\n",
            ),
            (
                "my-doc.md",
                concat!(
                    "---\n",
                    "type: doc\n",
                    "---\n",
                    "# my-doc\n",
                    "\n",
                    "Prose with `code`, **bold**, a \\*literal star\\*, and & an ampersand.\n",
                    "\n",
                    "A hard break\\\n",
                    "lands here.\n",
                    "\n",
                    "- bullet\n",
                    "  - nested\n",
                    "- | inline pipe |\n",
                    "\n",
                    "| col | wide |\n",
                    "| :--- | ---: |\n",
                    "| a | b |\n",
                    "\n",
                    "> quoted\\\n",
                    "> lines\n",
                    "\n",
                    "---\n",
                    "\n",
                    "![img](pic.png \"caption\")\n",
                    "\n",
                    "```rust\n",
                    "let x = 1;\n",
                    "```\n",
                ),
            ),
        ]);
    }

    /// Non-canonical spellings get rewritten once and then hold still.
    ///
    /// The first pass is allowed to move these — that is what `td fmt` is for.
    /// What it may not do is keep moving them. (That the rewrite still *says*
    /// the same thing is asserted against `parse`/`serialize` in `parse.rs`,
    /// where the AST is in reach.)
    #[test]
    fn test_fmt_rewrites_then_holds_still() {
        for body in [
            "Setext\n======\n\nprose\n",
            "* star bullets\n* here\n",
            "1) paren numbers\n2) here\n",
            "&#42;entity star&#42;\n",
            "trailing spaces  \nbreak\n",
            "    indented code\n",
            "> quoted\n| a | b |\n| --- | --- |\n",
            "text\n\n\n\n\nwith big gaps\n",
        ] {
            let src = format!("---\ntype: doc\n---\n# my-doc\n\n{body}");
            let dir = make_tree(&[
                (
                    ".typedown/doc.yaml",
                    "version: 2\nstructure:\n  title: from_filename\n",
                ),
                ("my-doc.md", &src),
            ]);
            let root = dir.path();
            format_dir(root, &[], FormatOptions::default()).unwrap();
            let after1 = fs::read_to_string(root.join("my-doc.md")).unwrap();
            format_dir(root, &[], FormatOptions::default()).unwrap();
            let after2 = fs::read_to_string(root.join("my-doc.md")).unwrap();
            assert_eq!(after1, after2, "second pass moved {body:?}");
        }
    }

    #[test]
    fn test_fix_h1_mismatch_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "version: 2\nstructure:\n  title: from_filename\n",
            ),
            ("my-doc.md", "---\ntype: doc\n---\n# Wrong Title\n"),
        ]);
    }

    #[test]
    fn test_fix_missing_h1_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "version: 2\nstructure:\n  title: from_filename\n",
            ),
            ("my-doc.md", "---\ntype: doc\n---\nNo heading here.\n"),
        ]);
    }

    #[test]
    fn test_fix_managed_section_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "version: 2\nstructure:\n  sections:\n    - title: Related\n      managed_content:\n        template: |\n          ## Related\n\n          - [README](README.md)\n",
            ),
            ("doc.md", "---\ntype: doc\n---\n# Doc\n\nSome intro.\n"),
            ("README.md", "# README\n"),
        ]);
    }

    #[test]
    fn test_managed_section_scope_root_skips_nested_files() {
        // `agents.yaml` keeps `**/CLAUDE.md` — per-directory instruction files
        // are a real convention — but its Related Documents template names
        // project-root paths. `scope: root` confines the section to the root
        // copy instead of inventing one beside every nested file.
        let dir = make_tree(&[
            (
                ".typedown/agents.yaml",
                "version: 2\npaths:\n  - \"**/CLAUDE.md\"\nstructure:\n  title: none\n  strict_sections: false\n  sections:\n    - title: Related Documents\n      managed_content:\n        scope: root\n        template: |\n          ## Related Documents\n\n          - **README.md** — what this project is and why.\n",
            ),
            ("CLAUDE.md", "# Project\n\nRoot instructions.\n"),
            ("src/CLAUDE.md", "# Src\n\nNested instructions.\n"),
        ]);

        format_dir(dir.path(), &[], FormatOptions::default()).unwrap();

        let root = fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert!(
            root.contains("## Related Documents") && root.contains("**README.md**"),
            "root CLAUDE.md should gain the managed section: {root:?}"
        );

        let nested = fs::read_to_string(dir.path().join("src/CLAUDE.md")).unwrap();
        assert!(
            !nested.contains("Related Documents"),
            "nested CLAUDE.md should not have a root-shaped section invented: {nested:?}"
        );
        assert_eq!(
            nested, "# Src\n\nNested instructions.\n",
            "nested CLAUDE.md should be left as authored"
        );

        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(errors.is_empty(), "should be clean after fmt: {errors:?}");
    }

    #[test]
    fn test_managed_section_default_scope_reaches_nested_files() {
        // The narrowing is opt-in: without `scope: root` a recursive glob still
        // manages every match, which is what a genuinely per-directory section
        // wants.
        let dir = make_tree(&[
            (
                ".typedown/doc.yaml",
                "version: 2\npaths:\n  - \"**/NOTES.md\"\nstructure:\n  title: none\n  strict_sections: false\n  sections:\n    - title: Related\n      managed_content:\n        template: |\n          ## Related\n\n          - **sibling.md** — a neighbour.\n",
            ),
            ("src/NOTES.md", "# Notes\n\nNested.\n"),
        ]);

        format_dir(dir.path(), &[], FormatOptions::default()).unwrap();

        let nested = fs::read_to_string(dir.path().join("src/NOTES.md")).unwrap();
        assert!(
            nested.contains("## Related"),
            "default scope should still manage nested matches: {nested:?}"
        );
    }

    #[test]
    fn test_from_directory_title_keeps_authored_capitalisation() {
        // `bridge/README.md` has `# Bridge`; the directory name is a filesystem
        // slug, so it says which title the H1 states, not how it is capitalised.
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n  strict_sections: false\n",
            ),
            ("bridge/README.md", "# Bridge\n\nThe bridge.\n"),
        ]);

        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors.is_empty(),
            "an H1 matching the directory up to case should be clean: {errors:?}"
        );

        let result = format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        assert_eq!(result.files_changed, 0, "fmt should not rewrite the H1");
        assert_eq!(
            fs::read_to_string(dir.path().join("bridge/README.md")).unwrap(),
            "# Bridge\n\nThe bridge.\n"
        );
    }

    #[test]
    fn test_from_directory_title_still_rewrites_a_different_title() {
        // Case-insensitivity is not permissiveness: an H1 naming something else
        // is still the mismatch the mode exists to catch.
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n  strict_sections: false\n",
            ),
            ("bridge/README.md", "# Something Else\n"),
        ]);

        format_dir(dir.path(), &[], FormatOptions::default()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("bridge/README.md")).unwrap(),
            "# bridge\n"
        );
    }

    #[test]
    fn test_fmt_keeps_a_leading_thematic_break() {
        // A `---` on line 1 is a frontmatter opener to the next parse, so
        // serializing one there deletes the break on the following run.
        let files: &[(&str, &str)] = &[
            (".typedown/doc.yaml", "version: 2\ndescription: A doc\n"),
            ("plain.md", "***\n\n# Plain\n"),
            ("dashes.md", "---\n\n# Dashes\n"),
            ("typed.md", "---\ntype: doc\n---\n\n---\n\n# Typed\n"),
        ];
        let dir = make_tree(files);
        let root = dir.path();

        format_dir(root, &[], FormatOptions::default()).unwrap();
        let after_first: Vec<String> = files[1..]
            .iter()
            .map(|(name, _)| fs::read_to_string(root.join(name)).unwrap())
            .collect();
        for (content, (name, _)) in after_first.iter().zip(&files[1..]) {
            assert!(
                content.contains("***"),
                "{name}: leading thematic break lost: {content:?}"
            );
        }
        assert!(
            after_first[2].starts_with("---\ntype: doc\n---\n"),
            "typed.md: frontmatter lost: {:?}",
            after_first[2]
        );

        let result2 = format_dir(root, &[], FormatOptions::default()).unwrap();
        assert_eq!(
            result2.files_changed, 0,
            "second pass changed files: {after_first:?}"
        );
    }

    #[test]
    fn test_fix_empty_optional_section_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "version: 2\nstructure:\n  sections:\n    - title: Notes\n      required: false\n",
            ),
            ("doc.md", "---\ntype: doc\n---\n# Doc\n\n## Notes\n"),
        ]);
    }

    #[test]
    fn test_fix_paragraph_to_bullet_idempotent() {
        assert_idempotent(&[
            (
                ".typedown/doc.yaml",
                "version: 2\nstructure:\n  sections:\n    - title: Goals\n      bullets: unordered\n",
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
                "version: 2\nstructure:\n  sections:\n    - title: Steps\n      bullets: ordered\n",
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
                "version: 2\nstructure:\n  sections:\n    - title: Alpha\n    - title: Beta\n    - title: Gamma\n",
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
                "version: 2\nstructure:\n  sections:\n    - title: Alpha\n    - title: Beta\n",
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
        assert!(
            alpha_pos < beta_pos,
            "Alpha should come before Beta: {result}"
        );
        assert!(
            result.contains("Alpha content."),
            "Alpha content preserved: {result}"
        );
        assert!(
            result.contains("Beta content."),
            "Beta content preserved: {result}"
        );

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
                "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
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
                "version: 2\npaths:\n  - \"**/ROADMAP.md\"\nstructure:\n  title: Roadmap\n  strict_sections: false\n",
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
                "version: 2\npaths:\n  - \"**/README.md\"\nfrontmatter:\n  type: object\n  properties:\n    description:\n      type: string\n  required: [description]\n",
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
                "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
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
                "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
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
                "version: 2\npaths:\n  - \"**/*.md\"\nstructure:\n  title: none\n",
            ),
            (
                ".typedown/b.yaml",
                "version: 2\npaths:\n  - \"docs/*.md\"\nstructure:\n  title: none\n",
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
                "version: 2\npaths:\n  - \"**/AGENTS.md\"\nstructure:\n  title: none\n  strict_sections: false\n",
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
                "version: 2\npaths:\n  - \"**/ROADMAP.md\"\nstructure:\n  title: Roadmap\n  strict_sections: false\n",
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
                "version: 2\npaths:\n  - \"**/README.md\"\nfrontmatter:\n  type: object\n  properties:\n    created:\n      type: string\n      format: date\n  required: [created]\n",
            ),
            ("README.md", "---\ncreated: not-a-date\n---\n# Title\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors
                .iter()
                .flat_map(|e| &e.diagnostics)
                .any(|d| d.message().contains("is not a \"date\"")),
            "path-matched files should still validate field types, got: {errors:?}"
        );
    }

    // ── v1 / v2 schema coexistence ──────────────────────────────────────────

    #[test]
    fn test_v1_and_v2_types_validate_side_by_side() {
        let dir = make_tree(&[
            (
                ".typedown/note.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    priority:\n      type: integer\n  required: [priority]\n",
            ),
            (
                ".typedown/security.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    ticker:\n      type: string\n      pattern: \"^[A-Z]+$\"\n  required: [ticker]\n",
            ),
            ("bad-note.md", "---\ntype: note\npriority: high\n---\n# N\n"),
            ("bad-sec.md", "---\ntype: security\nticker: aapl\n---\n# S\n"),
            ("good-note.md", "---\ntype: note\npriority: 1\n---\n# N\n"),
            ("good-sec.md", "---\ntype: security\nticker: AAPL\n---\n# S\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();

        let flagged: Vec<String> = errors
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(flagged.len(), 2, "got: {errors:?}");
        assert!(flagged.contains(&"bad-note.md".to_string()), "{flagged:?}");
        assert!(flagged.contains(&"bad-sec.md".to_string()), "{flagged:?}");
    }

    #[test]
    fn test_v2_path_matched_file_still_needs_required_frontmatter() {
        let dir = make_tree(&[
            (
                ".typedown/security.yaml",
                "version: 2\npaths:\n  - \"securities/*.md\"\nfrontmatter:\n  type: object\n  properties:\n    ticker:\n      type: string\n  required: [ticker]\n",
            ),
            ("securities/S.md", "# S\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(
            errors
                .iter()
                .flat_map(|e| &e.diagnostics)
                .any(|d| d.message().contains("missing required frontmatter fields")),
            "got: {errors:?}"
        );
    }

    #[test]
    fn test_v2_path_matched_file_without_requirements_needs_no_frontmatter() {
        let dir = make_tree(&[
            (
                ".typedown/security.yaml",
                "version: 2\npaths:\n  - \"securities/*.md\"\nfrontmatter:\n  type: object\n  properties:\n    ticker:\n      type: string\n",
            ),
            ("securities/S.md", "# S\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        assert!(errors.is_empty(), "got: {errors:?}");
    }

    /// Omitting frontmatter is a licence to skip *frontmatter* checks only.
    /// Regression: validation used to bail on the missing block, so a
    /// path-matched file with no frontmatter was never structure-checked --
    /// `td check` passed it having validated nothing at all.
    #[test]
    fn test_path_matched_file_without_frontmatter_is_still_structure_checked() {
        let dir = make_tree(&[
            (
                ".typedown/health-day.yaml",
                "version: 2\npaths:\n  - \"health/*.md\"\nstructure:\n  title: from_date\n  sections:\n    - title: Sleep\n    - title: Workouts\n",
            ),
            (
                "health/2026-04-14.md",
                "# Totally Wrong Title\n\n## Not A Real Section\n",
            ),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        let messages: Vec<String> = errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .map(|d| d.message())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("Totally Wrong Title")),
            "H1 mismatch should be reported, got: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("Not A Real Section")),
            "unexpected section should be reported, got: {messages:?}"
        );
        assert!(
            !messages.iter().any(|m| m.contains("frontmatter")),
            "no-frontmatter is legal for this type, got: {messages:?}"
        );
    }

    /// Structure diagnostics join the missing-frontmatter error rather than
    /// replacing it -- a type that requires fields still says so.
    #[test]
    fn test_required_frontmatter_error_survives_alongside_structure_errors() {
        let dir = make_tree(&[
            (
                ".typedown/security.yaml",
                "version: 2\npaths:\n  - \"securities/*.md\"\nstructure:\n  title: from_filename\nfrontmatter:\n  type: object\n  properties:\n    ticker:\n      type: string\n  required: [ticker]\n",
            ),
            ("securities/S.md", "# Wrong\n"),
        ]);
        let errors = check_dir(dir.path(), &[]).unwrap();
        let messages: Vec<String> = errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .map(|d| d.message())
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("missing required frontmatter fields")),
            "got: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("Wrong")),
            "H1 mismatch should also be reported, got: {messages:?}"
        );
    }

    #[test]
    fn test_fmt_preserves_raw_html_and_autolinks() {
        // Regression: `td fmt` used to delete inline HTML outright — `<Cat>`
        // vanished, and the missing tag re-balanced the surrounding backticks
        // into a mangled paragraph.
        let doc = concat!(
            "# Notes\n",
            "\n",
            "the `Mats` <Cat> parent\n",
            "\n",
            "a <br/> break and a <span>span</span>\n",
            "\n",
            "<div align=\"center\">\n",
            "  <img src=\"logo.png\">\n",
            "</div>\n",
            "\n",
            "<!-- a comment block -->\n",
            "\n",
            "See <https://example.com> or mail <foo@example.com>.\n",
        );
        let dir = make_tree(&[
            (".typedown/note.yaml", "version: 2\npaths:\n  - \"*.md\"\n"),
            ("doc.md", doc),
        ]);
        format_dir(dir.path(), &[], FormatOptions { check: false }).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("doc.md")).unwrap(),
            doc,
            "td fmt must not rewrite raw HTML or autolinks"
        );
    }

    #[test]
    fn test_v2_fmt_orders_frontmatter_by_schema_properties() {
        let dir = make_tree(&[
            (
                ".typedown/security.yaml",
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    ticker:\n      type: string\n    asset_class:\n      type: string\n    weight:\n      type: integer\n",
            ),
            (
                "S.md",
                "---\nweight: 5\ntype: security\nasset_class: cash\nticker: S\n---\n\n# S\n",
            ),
        ]);
        format_dir(dir.path(), &[], FormatOptions { check: false }).unwrap();
        let out = std::fs::read_to_string(dir.path().join("S.md")).unwrap();
        assert!(
            out.starts_with("---\ntype: security\nticker: S\nasset_class: cash\nweight: 5\n---\n"),
            "{out}"
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
                "version: 2\nfrontmatter:\n  type: object\n  properties:\n    name:\n      type: string\n    description:\n      type: string\n  required: [name, description]\n",
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
        let presets = TempDir::new().unwrap();
        fs::write(
            presets.path().join("readme.yaml"),
            "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n  strict_sections: false\n",
        )
        .unwrap();

        // Project has a .typedown/ with only a "note" type — no readme
        let dir = make_tree(&[(
            ".typedown/note.yaml",
            "version: 2\nfrontmatter:\n  type: object\n  properties:\n    title:\n      type: string\n",
        )]);
        // `title: from_directory` expects the H1 to be the temp dir's name,
        // which is only known after the tree exists.
        write_readme_titled_for_dir(&dir);

        let errors = check_dir_with(
            dir.path(),
            &[],
            &PresetSource::Overlay(Some(presets.path().to_path_buf())),
        )
        .unwrap();

        // README.md should be validated by the preset readme type (no errors)
        assert!(
            errors.is_empty(),
            "preset readme type should validate README.md, got: {errors:?}"
        );
    }

    #[test]
    fn test_project_local_overrides_preset() {
        // XDG preset: readme requires `description` field
        let presets = TempDir::new().unwrap();
        fs::write(
            presets.path().join("readme.yaml"),
            "version: 2\npaths:\n  - \"**/README.md\"\nfrontmatter:\n  type: object\n  properties:\n    description:\n      type: string\n  required: [description]\n",
        )
        .unwrap();

        // Project overrides readme locally with NO required fields
        let dir = make_tree(&[
            (
                ".typedown/readme.yaml",
                "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n  strict_sections: false\n",
            ),
        ]);
        write_readme_titled_for_dir(&dir);

        let errors = check_dir_with(
            dir.path(),
            &[],
            &PresetSource::Overlay(Some(presets.path().to_path_buf())),
        )
        .unwrap();

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
        let presets = TempDir::new().unwrap();
        fs::write(
            presets.path().join("readme.yaml"),
            "version: 2\npaths:\n  - \"**/README.md\"\nfrontmatter:\n  type: object\n  properties:\n    created:\n      type: string\n      format: date\n  required: [created]\n",
        )
        .unwrap();

        // No .typedown/ dir in the project
        let dir = make_tree(&[("README.md", "# Hello\n")]);

        let errors = check_dir_with(
            dir.path(),
            &[],
            &PresetSource::Overlay(Some(presets.path().to_path_buf())),
        )
        .unwrap();

        // Presets should validate: README.md is missing required `created` field
        assert!(
            !errors.is_empty(),
            "presets should apply without a .typedown/ dir"
        );
    }

    /// The built-in presets are `version: 2`, but a project that still writes
    /// v1 must keep loading: the two versions coexist in one merged schema, and
    /// the local definition wins whatever version either side is on.
    #[test]
    fn test_v1_project_schema_overrides_a_v2_builtin_preset() {
        // Built-in `task` requires `description:`; this v1 override requires
        // `owner:` instead, so the document below only passes if v1 wins.
        let dir = make_tree(&[
            (
                ".typedown/task.yaml",
                "version: 2\npaths:\n  - \"**/tasks/*.md\"\nfrontmatter:\n  type: object\n  properties:\n    owner:\n      type: string\n  required: [owner]\nstructure:\n  title: none\n  strict_sections: false\n",
            ),
            ("tasks/thing.md", "---\nowner: pat\n---\n\nDo the thing.\n"),
        ]);

        let errors = check_dir(dir.path(), &[]).unwrap();

        assert!(
            errors.is_empty(),
            "v1 project schema should override the v2 preset, got: {errors:?}"
        );
    }

    /// Same mix from the other direction: a v1 XDG preset shadowing a v2
    /// built-in of the same name. Its `fields:` are the ones that apply.
    #[test]
    fn test_v1_xdg_preset_overrides_a_v2_builtin_preset() {
        let presets = TempDir::new().unwrap();
        fs::write(
            presets.path().join("task.yaml"),
            "version: 2\npaths:\n  - \"**/tasks/*.md\"\nfrontmatter:\n  type: object\n  properties:\n    owner:\n      type: string\n  required: [owner]\nstructure:\n  title: none\n  strict_sections: false\n",
        )
        .unwrap();

        // No `owner:` — the v1 XDG preset must be what reports it, and the v2
        // built-in's `description:` requirement must be gone.
        let dir = make_tree(&[("tasks/thing.md", "---\nparent: other\n---\n\nDo it.\n")]);

        let errors = check_dir_with(
            dir.path(),
            &[],
            &PresetSource::Overlay(Some(presets.path().to_path_buf())),
        )
        .unwrap();

        let joined: String = errors
            .iter()
            .flat_map(|e| e.diagnostics.iter().map(Diagnostic::message))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("owner"),
            "expected the v1 rule, got: {joined}"
        );
        assert!(
            !joined.contains("description"),
            "the v2 built-in should be shadowed, got: {joined}"
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
        "version: 2\nstructure:\n  sections:\n    - title: Movies\n      links:\n        target_type: movie\n";

    /// Schema YAML: minimal movie type (no required fields).
    const MOVIE_YAML: &str = "version: 2\ndescription: A movie\n";

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
                (".typedown/tvshow.yaml", "version: 2\ndescription: A TV show\n"),
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
        let interest_yaml = "version: 2\nstructure:\n  sections:\n    - title: Personalities\n      links:\n        target_type: personality\n        bidirectional: true\n";
        let personality_yaml = "version: 2\nstructure:\n  sections:\n    - title: Related Interests\n      links:\n        target_type: interest\n";

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
        let interest_yaml = "version: 2\nstructure:\n  sections:\n    - title: Personalities\n      links:\n        target_type: personality\n        bidirectional: true\n";
        let personality_yaml = "version: 2\nstructure:\n  sections:\n    - title: Related Interests\n      links:\n        target_type: interest\n";

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
        let presets = TempDir::new().unwrap();
        fs::write(
            presets.path().join("movie.yaml"),
            "version: 2\ndescription: A movie\npaths:\n  - \"movies/*.md\"\n",
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
                (".typedown/other.yaml", "version: 2\ndescription: Other type\n"),
                ("movies/some-movie.md", "# Some Movie\n"),
            ],
        );

        let errors = check_dir_with(
            &proj_a,
            &[],
            &PresetSource::Overlay(Some(presets.path().to_path_buf())),
        )
        .unwrap();

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
