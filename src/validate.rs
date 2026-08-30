//! Pure document validation.
//!
//! Takes `&Document` + `&TypeDef` (and optionally pre-loaded link data), returns
//! `Vec<Diagnostic>`. No filesystem access, no I/O, no thread-local caches.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    ast::{
        self, inlines_to_markdown, inlines_to_string, Block, Document, Frontmatter, Inline,
        ListItem,
    },
    parse::parse,
    schema::{
        matches_template, parse_template, BulletMode, DateHeadingsDef, FieldDef, FieldType,
        HeadingSort, LinksDef, ManagedContent, ManagedScope, MergeMode, Schema, SectionDef,
        StructureDef, TitleMode, TypeDef,
    },
};

// ── Types ─────────────────────────────────────────────────────────────────────

/// A date-heading entry ready for sorting or fixing: `(date, time, suffix, blocks)`.
pub type SortedEntry = (String, Option<String>, Option<String>, Vec<Block>);

// ── Diagnostic ───────────────────────────────────────────────────────────────

/// A validation problem found in a document.
#[derive(Debug, Clone, PartialEq)]
pub enum Diagnostic {
    /// Frontmatter is missing entirely.
    MissingFrontmatter,
    /// A required frontmatter field is absent.
    MissingRequiredField { line: usize, field: String },
    /// A frontmatter field has the wrong type.
    InvalidFieldType {
        line: usize,
        field: String,
        message: String,
    },
    /// The document's `type` field doesn't match the expected type.
    TypeMismatch {
        line: usize,
        expected: String,
        actual: String,
    },
    /// The document is missing its H1 heading.
    MissingH1 { expected: String },
    /// The document's H1 doesn't match what the schema requires.
    H1Mismatch {
        line: usize,
        expected: String,
        actual: String,
    },
    /// A required section is missing.
    MissingSection { section: String },
    /// A section appears that is not in the schema allowlist.
    UnexpectedSection {
        line: usize,
        section: String,
        allowed: Vec<String>,
    },
    /// A section appears out of order (individual — for display only).
    SectionOutOfOrder { line: usize, section: String },
    /// Sections are out of order and can be reordered (fixable).
    SectionsOutOfOrder {
        /// Blocks before the first H2 (frontmatter, H1, intro).
        preamble: Vec<Block>,
        /// Sections reordered into schema-defined order, each as its blocks.
        sorted_sections: Vec<Vec<Block>>,
    },
    /// A section contains non-bullet content where only bullets are expected.
    SectionNotBullets { line: usize, context: String },
    /// A list in a section has the wrong type (ordered vs unordered).
    WrongListType {
        line: usize,
        context: String,
        expected: String,
    },
    /// A list item doesn't match the section's template.
    TemplateMismatch {
        line: usize,
        section: String,
        template: String,
    },
    /// A managed section needs to be updated (created or template mismatch).
    ManagedSectionNeedsUpdate {
        /// Block index of section start (None if section is absent).
        section_start: Option<usize>,
        /// Block index of section end (exclusive).
        section_end: usize,
        /// The blocks the section should hold: the rendered template, with its
        /// lists upserted against the entries the document already had (see
        /// [`MergeMode::Upsert`]).
        managed_blocks: Vec<Block>,
        /// Blocks the template didn't account for, appended after it on fix.
        custom_content: Vec<Block>,
    },
    /// A section link points to a file of the wrong type.
    LinkTargetTypeMismatch {
        line: usize,
        url: String,
        expected: String,
        actual: Option<String>,
    },
    /// A bidirectional link is missing a backlink.
    MissingBacklink {
        line: usize,
        url: String,
        inverse_section: String,
    },
    /// A frontmatter item has no prose subsection describing it
    /// (`correspondence`, data → prose).
    MissingSubsection {
        line: usize,
        /// The heading the item requires, marker included: `### 4k`.
        heading: String,
        /// The section it belongs under, marker included: `## Files`.
        container: String,
    },
    /// A prose subsection has no frontmatter item behind it
    /// (`correspondence`, prose → data).
    OrphanSubsection {
        line: usize,
        /// The orphaned heading, marker included: `### 4k`.
        heading: String,
        /// The frontmatter path that should have named it: `files[].quality`.
        expected: String,
    },
    /// A frontmatter value has no link answering it in the section that should
    /// point at it (`correspondence`, data → prose).
    MissingItemLink {
        /// The frontmatter key the value came from.
        line: usize,
        /// The value that needs a link: `Blue Jay`.
        item: String,
        /// The section that should link it, marker included: `## Species`.
        section: String,
        /// The type the link's target must resolve to, when the rule named one.
        target_type: Option<String>,
    },
    /// A link has no frontmatter value behind it (`correspondence`, prose → data).
    OrphanLink {
        line: usize,
        /// The orphaned link's URL, as written.
        url: String,
        /// The frontmatter path that should have named it: `species[]`.
        expected: String,
    },
    /// A document that exists on disk is linked from nowhere in the section
    /// that should list it (`correspondence`, disk → prose).
    UnlinkedDocument {
        /// The section's heading line, when the document has that section at all.
        line: Option<usize>,
        /// The unlinked file, relative to the document that should link it.
        file: String,
        /// The schema type that put it in scope.
        doc_type: String,
        /// The section that should have linked it, marker included: `## Seasons`.
        section: String,
    },
    /// The file exceeds the schema's size warning threshold.
    FileTooLarge {
        line: usize,
        size: usize,
        threshold: usize,
    },
    /// An unknown schema type was referenced.
    UnknownType { line: usize, message: String },
    /// A schema section's `template:` accepts every list item, so it enforces
    /// nothing. Raised against the schema at load time, not against a document.
    VacuousTemplate {
        type_name: String,
        section: String,
        template: String,
    },
    /// An H2 heading in a `date_headings` document isn't a valid date.
    InvalidDateHeading { line: usize, text: String },
    /// An H2 date entry's YYYY-MM prefix doesn't match the file's period.
    DateHeadingFileMismatch {
        line: usize,
        heading: String,
        expected_period: String,
    },
    /// Date entries are not in the expected sort order (fixable).
    EntriesOutOfOrder {
        /// Preamble blocks before the first date heading.
        preamble: Vec<Block>,
        /// Entries sorted into correct order: `(date_str, time_str_opt, suffix_opt, blocks)`.
        sorted_entries: Vec<SortedEntry>,
    },
    /// A link-like pattern has an unencoded space in the URL, making it unparseable.
    ///
    /// CommonMark parsers silently drop `[text](url with space)` — scanning the
    /// raw source catches these before the AST is built.
    MalformedLink { line: usize, url: String },
    /// `title: from_date` applies to a file whose name isn't `YYYY-MM` or
    /// `YYYY-MM-DD`, so no title can be derived. Not fixable: there is nothing
    /// to write, and an empty title would delete the document's H1.
    UndatedFilename { filename: String },
    /// One or more optional sections are empty and will be removed (fixable).
    EmptyOptionalSection {
        /// `(start_block_idx, end_block_idx_exclusive)` for each empty section,
        /// in document order. The fix removes them in reverse to preserve indices.
        section_ranges: Vec<(usize, usize)>,
    },
}

impl Diagnostic {
    /// Returns a human-readable description of this diagnostic.
    pub fn message(&self) -> String {
        match self {
            Self::MissingFrontmatter => "document is missing frontmatter".to_string(),
            Self::MissingRequiredField { field, .. } => {
                format!("missing required field '{field}'")
            }
            Self::InvalidFieldType { field, message, .. } => {
                format!("field '{field}': {message}")
            }
            Self::TypeMismatch {
                expected, actual, ..
            } => {
                format!("type '{actual}' does not match schema type '{expected}'")
            }
            Self::MissingH1 { expected } => {
                format!("document is missing H1 title '{expected}'")
            }
            Self::H1Mismatch {
                expected, actual, ..
            } => {
                format!("H1 '{actual}' does not match expected '{expected}'")
            }
            Self::MissingSection { section } => {
                format!("required section '{section}' is missing")
            }
            Self::UnexpectedSection {
                section, allowed, ..
            } => {
                format!(
                    "unexpected section '{section}' (allowed: {})",
                    allowed.join(", ")
                )
            }
            Self::SectionOutOfOrder { section, .. } => {
                format!("section '{section}' appears out of order")
            }
            Self::SectionsOutOfOrder { .. } => "sections are out of order".to_string(),
            Self::SectionNotBullets { context, .. } => {
                format!("{context}: only bullet lists are allowed here")
            }
            Self::WrongListType {
                context, expected, ..
            } => {
                format!("{context}: expected {expected} list")
            }
            Self::TemplateMismatch {
                section, template, ..
            } => {
                format!("section '{section}': item doesn't match template '{template}'")
            }
            Self::ManagedSectionNeedsUpdate { .. } => {
                "managed section needs to be updated".to_string()
            }
            Self::LinkTargetTypeMismatch {
                url,
                expected,
                actual,
                ..
            } => match actual {
                Some(t) => format!("link '{url}' must target type '{expected}', got '{t}'"),
                None => {
                    format!("link '{url}' must target type '{expected}', but target has no type")
                }
            },
            Self::MissingBacklink {
                url,
                inverse_section,
                ..
            } => {
                format!("link to '{url}' requires a backlink in section '{inverse_section}'")
            }
            Self::MissingSubsection {
                heading, container, ..
            } => {
                format!("frontmatter item requires a '{heading}' subsection under '{container}'")
            }
            Self::OrphanSubsection {
                heading, expected, ..
            } => {
                format!("subsection '{heading}' has no matching frontmatter item at '{expected}'")
            }
            Self::MissingItemLink {
                item,
                section,
                target_type,
                ..
            } => match target_type {
                Some(t) => format!(
                    "frontmatter item '{item}' requires a link to a '{t}' document in '{section}'"
                ),
                None => format!("frontmatter item '{item}' requires a link to it in '{section}'"),
            },
            Self::OrphanLink { url, expected, .. } => {
                format!("link '{url}' has no matching frontmatter item at '{expected}'")
            }
            Self::UnlinkedDocument {
                file,
                doc_type,
                section,
                ..
            } => {
                format!("'{file}' is a {doc_type} document that '{section}' doesn't link")
            }
            Self::FileTooLarge {
                size, threshold, ..
            } => {
                format!("file is {size} bytes, exceeding the {threshold}-byte warning threshold")
            }
            Self::UnknownType { message, .. } => message.clone(),
            Self::VacuousTemplate {
                type_name,
                section,
                template,
            } => {
                format!(
                    "type '{type_name}', section '{section}': template '{template}' matches any \
                     list item, so it validates nothing — add literal text, **bold**, a \
                     [link](url), or YYYY-MM-DD to constrain it"
                )
            }
            Self::InvalidDateHeading { text, .. } => {
                format!("'{text}' is not a valid date heading (expected YYYY-MM-DD or YYYY-MM-DD HH:MM, optionally followed by ' - title')")
            }
            Self::DateHeadingFileMismatch {
                heading,
                expected_period,
                ..
            } => {
                format!("date heading '{heading}' doesn't match file period '{expected_period}'")
            }
            Self::EntriesOutOfOrder { .. } => {
                "date entries are not in the expected order".to_string()
            }
            Self::MalformedLink { url, .. } => {
                format!("link '{url}' contains an unencoded space (use %20 or rename the file)")
            }
            Self::UndatedFilename { filename } => {
                format!(
                    "'{filename}' is not named YYYY-MM or YYYY-MM-DD, so 'title: from_date' \
                     cannot derive an H1 for it"
                )
            }
            Self::EmptyOptionalSection { .. } => {
                "empty optional section(s) will be removed".to_string()
            }
        }
    }

    /// Returns the 1-based source line number associated with this diagnostic, if any.
    pub fn line(&self) -> Option<usize> {
        match self {
            Self::MissingFrontmatter | Self::MissingSection { .. } | Self::MissingH1 { .. } => None,
            Self::MissingRequiredField { line, .. }
            | Self::InvalidFieldType { line, .. }
            | Self::TypeMismatch { line, .. }
            | Self::H1Mismatch { line, .. }
            | Self::UnexpectedSection { line, .. }
            | Self::SectionOutOfOrder { line, .. }
            | Self::SectionNotBullets { line, .. }
            | Self::WrongListType { line, .. }
            | Self::TemplateMismatch { line, .. }
            | Self::LinkTargetTypeMismatch { line, .. }
            | Self::MissingBacklink { line, .. }
            | Self::MissingSubsection { line, .. }
            | Self::OrphanSubsection { line, .. }
            | Self::MissingItemLink { line, .. }
            | Self::OrphanLink { line, .. }
            | Self::FileTooLarge { line, .. }
            | Self::UnknownType { line, .. }
            | Self::InvalidDateHeading { line, .. }
            | Self::DateHeadingFileMismatch { line, .. } => Some(*line),
            // Anchored to the section that should have listed the file, when the
            // document has that section — a stripped section leaves nothing to point at.
            Self::UnlinkedDocument { line, .. } => *line,
            Self::ManagedSectionNeedsUpdate { .. }
            | Self::EntriesOutOfOrder { .. }
            | Self::SectionsOutOfOrder { .. } => None,
            Self::MalformedLink { line, .. } => Some(*line),
            // Schema-level, not document-level: there's no source line to point at.
            Self::EmptyOptionalSection { .. }
            | Self::VacuousTemplate { .. }
            | Self::UndatedFilename { .. } => None,
        }
    }
}

// ── Pre-loaded link data (for bidirectional validation without I/O) ───────────

/// Type information about a linked document, pre-loaded by the orchestrator.
#[derive(Debug, Clone)]
pub struct LinkedDocInfo {
    /// Absolute path to the linked document.
    pub path: PathBuf,
    /// The `type` field from the document's frontmatter, if any.
    pub doc_type: Option<String>,
    /// Link URLs found in each named section: `section_title → [url, ...]`.
    pub section_links: std::collections::HashMap<String, Vec<String>>,
}

/// Shared context for validating a single document.
///
/// Bundles the environmental data that threads through the whole internal
/// validation call chain. Construct once per document; pass by reference.
pub struct ValidateCtx<'a> {
    /// Absolute path to the source document (for link resolution, no I/O).
    pub source_path: &'a Path,
    /// The schema root — the `.typedown/` parent that `paths:` globs are
    /// relative to. `None` when the caller doesn't know it, in which case
    /// every document is treated as living at the root.
    pub schema_root: Option<&'a Path>,
    /// The document's declared type name (for bidirectional link validation).
    pub source_type: &'a str,
    /// Full schema (for type lookups).
    pub schema: &'a Schema,
    /// Pre-loaded info about linked documents (for link target + backlink checks).
    pub linked_docs: &'a HashMap<PathBuf, LinkedDocInfo>,
    /// Pre-loaded set of all git-tracked absolute paths (for cross-project links).
    pub git_tree: Option<&'a crate::git::GitTree>,
    /// Type definitions discovered from external (cross-project) schemas.
    /// Used as a fallback for bidi validation when the target type isn't in `schema`.
    pub external_types: &'a HashMap<String, TypeDef>,
}

// ── Malformed link scanning ───────────────────────────────────────────────────

/// Scan raw markdown source for link-like patterns that failed to parse due to
/// an unencoded space in the URL.
///
/// CommonMark parsers silently drop `[text](url with space)` — the text just
/// renders as plain text.  This function catches those before the AST is built
/// by scanning line-by-line with a regex.
///
/// Skips frontmatter, fenced code blocks, and inline code spans.
pub fn detect_malformed_links(content: &str) -> Vec<Diagnostic> {
    use regex::Regex;

    // Compiled once per process via OnceLock.
    static MALFORMED_LINK_RE: OnceLock<Regex> = OnceLock::new();

    // `[text](url-containing-a-space-or-tab)`
    // Capture group 2 is the URL portion, which must contain at least one
    // space or horizontal tab.
    let re =
        MALFORMED_LINK_RE.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]*[ \t][^)]*)\)").unwrap());

    let mut out = Vec::new();
    let mut in_fenced_code = false;

    // The body only, so a `---` inside a value can't be mistaken for a
    // delimiter and a document whose `---` never closes keeps its first line.
    let split = crate::parse::split_frontmatter(content);

    // Body lines carry their source line number: the first is one past the
    // frontmatter block.
    for (line_num, line) in (split.frontmatter_lines + 1..).zip(split.body.lines()) {
        // Fenced code blocks (``` toggled)
        if line.trim_start().starts_with("```") {
            in_fenced_code = !in_fenced_code;
            continue;
        }
        if in_fenced_code {
            continue;
        }

        // Scan the line for malformed link patterns
        for cap in re.captures_iter(line) {
            // Skip matches inside an inline code span: count backticks before
            // the match start.  An odd count means we're inside an open span.
            let match_start = cap.get(0).map_or(0, |m| m.start());
            let backticks_before = line[..match_start].chars().filter(|&c| c == '`').count();
            if backticks_before % 2 == 1 {
                continue;
            }

            let url = cap.get(2).map_or("", |m| m.as_str()).to_string();
            out.push(Diagnostic::MalformedLink {
                line: line_num,
                url,
            });
        }
    }

    out
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Validate a document against a known type definition.
///
/// - `doc`: the parsed document
/// - `type_def`: the schema type to validate against
/// - `ctx`: shared environmental context (path, schema, linked docs, git tree)
/// - `file_size`: the size of the file in bytes (for size warnings)
pub fn validate(
    doc: &Document,
    type_def: &TypeDef,
    ctx: &ValidateCtx<'_>,
    file_size: Option<usize>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Size warning
    if let (Some(threshold), Some(size)) = (type_def.structure.size_warning, file_size) {
        if size > threshold {
            diagnostics.push(Diagnostic::FileTooLarge {
                line: 1,
                size,
                threshold,
            });
        }
    }

    // Frontmatter validation only applies when there is frontmatter to check.
    // A missing block is reported once and the rest of the schema -- title,
    // sections, intro, links -- is still validated below: path-matched types
    // are allowed to omit frontmatter, but never to skip their structure.
    match doc.frontmatter {
        None => diagnostics.push(Diagnostic::MissingFrontmatter),
        Some(ref fm) => {
            // Validate type field
            match &fm.doc_type {
                Some(actual) if actual != ctx.source_type => {
                    diagnostics.push(Diagnostic::TypeMismatch {
                        line: 1,
                        expected: ctx.source_type.to_string(),
                        actual: actual.clone(),
                    });
                }
                None => {
                    diagnostics.push(Diagnostic::MissingRequiredField {
                        line: 1,
                        field: format!("type (expected '{}')", ctx.source_type),
                    });
                }
                _ => {}
            }

            validate_frontmatter(fm, type_def, &mut diagnostics);
        }
    }

    validate_structure(doc, &type_def.structure, ctx, &mut diagnostics);

    crate::correspondence::validate_correspondence(
        doc,
        &type_def.correspondence,
        ctx,
        &mut diagnostics,
    );

    validate_all_links(doc, ctx, &mut diagnostics);

    diagnostics
}

/// Validate a document that has a `type` field but no matching schema type.
pub fn validate_unknown_type(doc: &Document, schema: &Schema) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let valid_types: Vec<&str> = schema.types.keys().map(|s| s.as_str()).collect();

    let Some(ref fm) = doc.frontmatter else {
        diagnostics.push(Diagnostic::MissingFrontmatter);
        return diagnostics;
    };

    match &fm.doc_type {
        None => {
            diagnostics.push(Diagnostic::MissingRequiredField {
                line: 1,
                field: format!("type (valid types: {})", valid_types.join(", ")),
            });
        }
        Some(t) => {
            diagnostics.push(Diagnostic::UnknownType {
                line: 1,
                message: format!(
                    "unknown type '{t}' (valid types: {})",
                    valid_types.join(", ")
                ),
            });
        }
    }

    diagnostics
}

// ── Field validation ──────────────────────────────────────────────────────────

/// Build the json instance validated against a `frontmatter:` schema.
///
/// `type:` is folded back in so the json-schema can constrain it like any other
/// property; everything else is the frontmatter as written, converted to json.
pub fn frontmatter_instance(fm: &Frontmatter) -> serde_json::Value {
    let map = fm
        .iter()
        .map(|(key, value)| (key.to_string(), crate::json::yaml_to_json(&value)))
        .collect();
    serde_json::Value::Object(map)
}

/// Validate frontmatter against the type's literal json-schema.
///
/// A json-schema compile failure is reported as an `UnknownType` diagnostic
/// rather than swallowed — `Schema::load` rejects such schemas up front, so
/// this only fires for `TypeDef`s built in memory.
fn validate_frontmatter(fm: &Frontmatter, type_def: &TypeDef, out: &mut Vec<Diagnostic>) {
    let validator = match type_def.frontmatter_validator() {
        Ok(Some(v)) => v,
        Ok(None) => return,
        Err(e) => {
            out.push(Diagnostic::UnknownType {
                line: 1,
                message: e.to_string(),
            });
            return;
        }
    };

    let instance = frontmatter_instance(fm);
    let mut found: Vec<Diagnostic> = Vec::new();
    for error in validator.iter_errors(&instance) {
        let path = json_pointer_segments(error.instance_path());
        let top_key = path.first().cloned();
        let line = top_key.as_deref().map_or(1, |k| fm.line_of(k));

        match error.kind() {
            jsonschema::error::ValidationErrorKind::Required { property } => {
                let name = property.as_str().unwrap_or_default();
                found.push(Diagnostic::MissingRequiredField {
                    line,
                    field: join_field_path(&path, Some(name)),
                });
            }
            _ => found.push(Diagnostic::InvalidFieldType {
                line,
                field: join_field_path(&path, None),
                message: error.to_string(),
            }),
        }
    }

    // `iter_errors` yields in schema-traversal order; sort so diagnostics are
    // stable regardless of how the json-schema happens to be laid out.
    found.sort_by_key(|d| (d.line().unwrap_or(0), field_name_of(d).to_string()));
    out.extend(found);
}

/// Split a json-schema instance location (`/tags/0`) into its segments.
fn json_pointer_segments(location: &jsonschema::paths::Location) -> Vec<String> {
    location
        .as_str()
        .split('/')
        .skip(1)
        .filter(|s| !s.is_empty())
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Render instance path segments as a typedown field name: `files[0].name`.
///
/// `extra` appends one more property segment (used for `required` errors, whose
/// instance path points at the containing object).
fn join_field_path(segments: &[String], extra: Option<&str>) -> String {
    let mut out = String::new();
    for seg in segments.iter().map(String::as_str).chain(extra) {
        if seg.chars().all(|c| c.is_ascii_digit()) && !seg.is_empty() && !out.is_empty() {
            out.push('[');
            out.push_str(seg);
            out.push(']');
        } else {
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(seg);
        }
    }
    if out.is_empty() {
        "frontmatter".to_string()
    } else {
        out
    }
}

/// The field name carried by a frontmatter diagnostic, for stable sorting.
fn field_name_of(d: &Diagnostic) -> &str {
    match d {
        Diagnostic::MissingRequiredField { field, .. }
        | Diagnostic::InvalidFieldType { field, .. } => field,
        _ => "",
    }
}

// ── Structure validation ──────────────────────────────────────────────────────

fn validate_structure(
    doc: &Document,
    structure: &StructureDef,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    validate_title(doc, structure, ctx.source_path, out);

    if let Some(ref intro_def) = structure.intro {
        validate_intro_content(doc, intro_def, out);
    }

    if let Some(ref dh) = structure.date_headings {
        // date_headings and sections are mutually exclusive
        let file_period = if matches!(structure.title, TitleMode::FromDate) {
            file_period_from_path(ctx.source_path)
        } else {
            None
        };
        validate_date_headings(doc, dh, file_period.as_deref(), out);
    } else if !structure.sections.is_empty() {
        validate_sections(doc, structure, ctx, out);

        // Managed sections
        for section_def in &structure.sections {
            if let Some(ref managed) = section_def.managed_content {
                if managed.scope == ManagedScope::Root && !at_schema_root(ctx) {
                    continue;
                }
                validate_managed_section(doc, &section_def.title, managed, out);
            }
        }
    }
}

/// Whether the document sits directly in the schema root, rather than in a
/// subdirectory a recursive `paths:` glob also reached.
///
/// A caller that doesn't supply `schema_root` gets `true`: unknown root means
/// no suppression, so `scope: root` never silently disables a section.
fn at_schema_root(ctx: &ValidateCtx<'_>) -> bool {
    match ctx.schema_root {
        Some(root) => ctx
            .source_path
            .strip_prefix(root)
            .is_ok_and(|rel| rel.components().count() == 1),
        None => true,
    }
}

fn validate_title(
    doc: &Document,
    structure: &StructureDef,
    source_path: &Path,
    out: &mut Vec<Diagnostic>,
) {
    let h1 = doc.blocks.iter().find_map(|b| match b {
        Block::Heading {
            level: 1,
            content,
            line,
        } => Some((inlines_to_string(content), *line)),
        _ => None,
    });

    match &structure.title {
        TitleMode::None => {}
        TitleMode::FromFilename => {
            let expected = source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            match h1 {
                Some((title, _)) if title == expected => {}
                Some((actual, line)) => out.push(Diagnostic::H1Mismatch {
                    line,
                    expected,
                    actual,
                }),
                None => out.push(Diagnostic::MissingH1 { expected }),
            }
        }
        TitleMode::FromDirectory => {
            let expected = source_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            // Case-insensitive: a directory name is a filesystem slug, so it
            // says which title the H1 states, not how it is capitalised.
            // `bridge/README.md` keeps `# Bridge` rather than being lowercased.
            match h1 {
                Some((title, _)) if title.eq_ignore_ascii_case(&expected) => {}
                Some((actual, line)) => out.push(Diagnostic::H1Mismatch {
                    line,
                    expected,
                    actual,
                }),
                None => out.push(Diagnostic::MissingH1 { expected }),
            }
        }
        TitleMode::FromDate => match date_title_from_path(source_path) {
            Some(expected) => match h1 {
                Some((title, _)) if title == expected => {}
                Some((actual, line)) => out.push(Diagnostic::H1Mismatch {
                    line,
                    expected,
                    actual,
                }),
                None => out.push(Diagnostic::MissingH1 { expected }),
            },
            // No derivable title: report the mismatch between schema and
            // filename rather than asserting `""` and letting `td fmt` act on it.
            None => out.push(Diagnostic::UndatedFilename {
                filename: source_path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }),
        },
        TitleMode::Fixed(expected) => match h1 {
            Some((title, _)) if title == *expected => {}
            Some((actual, line)) => out.push(Diagnostic::H1Mismatch {
                line,
                expected: expected.clone(),
                actual,
            }),
            None => out.push(Diagnostic::MissingH1 {
                expected: expected.clone(),
            }),
        },
        TitleMode::RequiredAny => {
            if h1.is_none() {
                out.push(Diagnostic::MissingH1 {
                    expected: String::new(),
                });
            }
        }
    }
}

/// Derive a human-readable title from a date-stemmed path.
///
/// `journal/2026-02.md` → `"February 2026"`; `health/2026-04-14.md` →
/// `"April 14, 2026"`.  Returns `None` if the stem is neither `YYYY-MM` nor
/// `YYYY-MM-DD` — callers must not substitute an empty title for `None`, or
/// `td fmt` will "fix" a real H1 into nothing.
pub fn date_title_from_path(path: &Path) -> Option<String> {
    let (year, month, day) = date_parts_from_path(path)?;
    let month_names = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let month_name = month_names[(month - 1) as usize];
    match day {
        Some(day) => Some(format!("{month_name} {day}, {year}")),
        None => Some(format!("{month_name} {year}")),
    }
}

/// Parse a path's stem as `YYYY-MM` or `YYYY-MM-DD`, returning
/// `(year, month, day)` with `day` set only at day precision.
fn date_parts_from_path(path: &Path) -> Option<(u32, u32, Option<u32>)> {
    let stem = path.file_stem()?.to_str()?;
    let mut parts = stem.split('-');
    let year = fixed_width_number(parts.next()?, 4)?;
    let month = fixed_width_number(parts.next()?, 2)?;
    let day = match parts.next() {
        Some(day_str) => Some(fixed_width_number(day_str, 2)?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&month) {
        return None;
    }
    if let Some(day) = day {
        // Rejects 2026-02-30 and friends, not just out-of-range days.
        chrono::NaiveDate::from_ymd_opt(year as i32, month, day)?;
    }
    Some((year, month, day))
}

/// The `YYYY-MM` period a date-stemmed file covers, for matching against the
/// `YYYY-MM` prefix of its date headings. A day-precision file still names one
/// month: `2026-04-14.md` → `2026-04`.
fn file_period_from_path(path: &Path) -> Option<String> {
    let (year, month, _) = date_parts_from_path(path)?;
    Some(format!("{year:04}-{month:02}"))
}

/// Parse an exactly-`width`-digit ASCII number, e.g. the `04` of `2026-04-14`.
///
/// Strictness is the point: without it `1-2-3.md` would derive a title.
fn fixed_width_number(s: &str, width: usize) -> Option<u32> {
    if s.len() != width || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

// ── Date heading validation ───────────────────────────────────────────────────

/// Parse an H2 text as a journal date entry.
///
/// Accepted forms:
/// - `YYYY-MM-DD`
/// - `YYYY-MM-DD HH:MM`
/// - `YYYY-MM-DD - title`
/// - `YYYY-MM-DD HH:MM - title`
///
/// Returns `(date_str, time_str_or_none, suffix_or_none)` on success, `None` on failure.
fn parse_entry_heading(text: &str) -> Option<(String, Option<String>, Option<String>)> {
    // Must start with YYYY-MM-DD
    if text.len() < 10 {
        return None;
    }
    let (date_part, rest) = text.split_at(10);
    // Validate YYYY-MM-DD shape with ascii digits / dashes
    let b = date_part.as_bytes();
    if !(b[0..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'-'
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[7] == b'-'
        && b[8..10].iter().all(|c| c.is_ascii_digit()))
    {
        return None;
    }

    // rest is everything after YYYY-MM-DD.  Four valid patterns:
    //   ""              → date only
    //   " - <suffix>"   → date + suffix
    //   " HH:MM"        → date + time
    //   " HH:MM - <s>"  → date + time + suffix
    if rest.is_empty() {
        return Some((date_part.to_string(), None, None));
    }

    // Try to parse a time component (" HH:MM")
    if rest.starts_with(' ') && rest.len() >= 6 {
        let t = &rest[1..6]; // potential "HH:MM"
        let tb = t.as_bytes();
        if tb[0..2].iter().all(|c| c.is_ascii_digit())
            && tb[2] == b':'
            && tb[3..5].iter().all(|c| c.is_ascii_digit())
        {
            let after_time = &rest[6..]; // "" or " - <suffix>"
            let suffix = parse_suffix(after_time)?;
            return Some((date_part.to_string(), Some(t.to_string()), suffix));
        }
    }

    // No time — try suffix directly (" - <suffix>")
    let suffix = parse_suffix(rest)?;
    Some((date_part.to_string(), None, suffix))
}

/// Parse the optional ` - <suffix>` tail that follows a date or time component.
///
/// - Empty string → `Some(None)` (no suffix, still valid)
/// - `" - <text>"` → `Some(Some(text))` (suffix present)
/// - Anything else → `None` (invalid trailing text)
fn parse_suffix(s: &str) -> Option<Option<String>> {
    if s.is_empty() {
        Some(None)
    } else {
        s.strip_prefix(" - ").map(|suffix| Some(suffix.to_string()))
    }
}

/// Sort key for an entry: `(date, time)` where missing time sorts last (end of day).
fn entry_sort_key(date: &str, time: Option<&str>) -> (String, String) {
    (date.to_string(), time.unwrap_or("99:99").to_string())
}

fn validate_date_headings(
    doc: &Document,
    def: &DateHeadingsDef,
    file_period: Option<&str>,
    out: &mut Vec<Diagnostic>,
) {
    // Collect all H2 headings with their block-index and line
    let h2s: Vec<(usize, String, usize)> = doc
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b {
            Block::Heading {
                level: 2,
                content,
                line,
            } => Some((i, inlines_to_string(content), *line)),
            _ => None,
        })
        .collect();

    // Validate each H2 as a date, and check file-period match
    let mut valid_entries: Vec<(String, Option<String>, Option<String>, usize)> = Vec::new(); // (date, time, suffix, block_idx)
    for (block_idx, text, line) in &h2s {
        match parse_entry_heading(text) {
            None => {
                out.push(Diagnostic::InvalidDateHeading {
                    line: *line,
                    text: text.clone(),
                });
            }
            Some((date, time, suffix)) => {
                // Check YYYY-MM prefix matches file period
                if let Some(period) = file_period {
                    let entry_period = &date[..7]; // "YYYY-MM"
                    if entry_period != period {
                        out.push(Diagnostic::DateHeadingFileMismatch {
                            line: *line,
                            heading: text.clone(),
                            expected_period: period.to_string(),
                        });
                    }
                }
                valid_entries.push((date, time, suffix, *block_idx));
            }
        }
    }

    // Check sort order
    if valid_entries.len() < 2 {
        return;
    }
    let keys: Vec<(String, String)> = valid_entries
        .iter()
        .map(|(d, t, _, _)| entry_sort_key(d, t.as_deref()))
        .collect();

    let in_order = match def.sort {
        HeadingSort::NewestFirst => keys.windows(2).all(|w| w[0] >= w[1]),
        HeadingSort::OldestFirst => keys.windows(2).all(|w| w[0] <= w[1]),
    };

    if !in_order {
        // Build the sorted entry list for the fix to use
        let preamble: Vec<Block> = {
            let first_h2_idx = h2s[0].0;
            doc.blocks[..first_h2_idx].to_vec()
        };

        // Slice each entry: from its H2 block to the next H2 (or end of doc)
        let mut entries_with_blocks: Vec<SortedEntry> = valid_entries
            .iter()
            .enumerate()
            .map(|(ei, (date, time, suffix, start_idx))| {
                // Find the next H2 block index
                let next_h2_idx = valid_entries
                    .get(ei + 1)
                    .map(|(_, _, _, idx)| *idx)
                    .unwrap_or(doc.blocks.len());
                let entry_blocks = doc.blocks[*start_idx..next_h2_idx].to_vec();
                (date.clone(), time.clone(), suffix.clone(), entry_blocks)
            })
            .collect();

        // Sort
        entries_with_blocks.sort_by(|(da, ta, _, _), (db, tb, _, _)| {
            let ka = entry_sort_key(da, ta.as_deref());
            let kb = entry_sort_key(db, tb.as_deref());
            match def.sort {
                HeadingSort::NewestFirst => kb.cmp(&ka),
                HeadingSort::OldestFirst => ka.cmp(&kb),
            }
        });

        out.push(Diagnostic::EntriesOutOfOrder {
            preamble,
            sorted_entries: entries_with_blocks,
        });
    }
}

fn validate_intro_content(doc: &Document, intro_def: &SectionDef, out: &mut Vec<Diagnostic>) {
    let h1_pos = doc
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Heading { level: 1, .. }));
    let first_h2_pos = doc
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Heading { level: 2, .. }));

    let Some(start) = h1_pos else { return };
    let end = first_h2_pos.unwrap_or(doc.blocks.len());
    let intro_blocks = &doc.blocks[start + 1..end];

    if let Some(mode) = intro_def.effective_bullet_mode() {
        validate_bullets_only(intro_blocks, "intro", mode, out);
    }
}

/// Validate that all blocks in a slice are bullet lists (or blank lines),
/// and that each list matches the expected [`BulletMode`].
fn validate_bullets_only(
    blocks: &[Block],
    context: &str,
    mode: BulletMode,
    out: &mut Vec<Diagnostic>,
) {
    for block in blocks {
        match block {
            Block::List { ordered, line, .. } => {
                let ok = match mode {
                    BulletMode::Ordered => *ordered,
                    BulletMode::Unordered => !*ordered,
                };
                if !ok {
                    let expected = match mode {
                        BulletMode::Ordered => "ordered",
                        BulletMode::Unordered => "unordered",
                    };
                    out.push(Diagnostic::WrongListType {
                        line: *line,
                        context: context.to_string(),
                        expected: expected.to_string(),
                    });
                }
            }
            Block::BlankLine => {}
            other => {
                out.push(Diagnostic::SectionNotBullets {
                    line: other.line(),
                    context: context.to_string(),
                });
            }
        }
    }
}

fn validate_sections(
    doc: &Document,
    structure: &StructureDef,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let allowed_titles: Vec<&str> = structure
        .sections
        .iter()
        .map(|s| s.title.as_str())
        .collect();

    // Collect H2 blocks: (block_index, title, line)
    let h2s: Vec<(usize, String, usize)> = doc
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b {
            Block::Heading {
                level: 2,
                content,
                line,
            } => Some((i, inlines_to_string(content), *line)),
            _ => None,
        })
        .collect();

    // Strict mode checks
    if structure.strict_sections {
        for (_, h2, line) in &h2s {
            if !allowed_titles.contains(&h2.as_str()) {
                out.push(Diagnostic::UnexpectedSection {
                    line: *line,
                    section: h2.clone(),
                    allowed: allowed_titles.iter().map(|s| s.to_string()).collect(),
                });
            }
        }

        // Ordering: each recognised section's schema index must be non-decreasing
        let mut has_order_error = false;
        let mut last_schema_idx = 0usize;
        for (_, h2, line) in &h2s {
            if let Some(idx) = allowed_titles.iter().position(|s| *s == h2.as_str()) {
                if idx < last_schema_idx {
                    has_order_error = true;
                    out.push(Diagnostic::SectionOutOfOrder {
                        line: *line,
                        section: h2.clone(),
                    });
                }
                last_schema_idx = idx;
            }
        }

        // Emit a fixable SectionsOutOfOrder with sorted blocks
        if has_order_error {
            // Preamble: everything before the first H2
            let first_h2_block = h2s.first().map(|(i, _, _)| *i).unwrap_or(doc.blocks.len());
            let preamble = doc.blocks[..first_h2_block].to_vec();

            // Extract each section as a Vec<Block> (heading + body)
            let mut raw_sections: Vec<(Option<usize>, Vec<Block>)> = Vec::new();
            for (pos_idx, (start, title, _)) in h2s.iter().enumerate() {
                let end = h2s
                    .get(pos_idx + 1)
                    .map(|(i, _, _)| *i)
                    .unwrap_or(doc.blocks.len());
                let schema_idx = allowed_titles.iter().position(|s| *s == title.as_str());
                raw_sections.push((schema_idx, doc.blocks[*start..end].to_vec()));
            }

            // Sort by schema index (unknown sections go to the end)
            raw_sections.sort_by_key(|(idx, _)| idx.unwrap_or(usize::MAX));

            let sorted_sections = raw_sections.into_iter().map(|(_, blocks)| blocks).collect();

            out.push(Diagnostic::SectionsOutOfOrder {
                preamble,
                sorted_sections,
            });
        }
    }

    // Required sections
    for section_def in &structure.sections {
        if section_def.required && !h2s.iter().any(|(_, h, _)| h == &section_def.title) {
            out.push(Diagnostic::MissingSection {
                section: section_def.title.clone(),
            });
        }
    }

    // Per-section content validation
    validate_section_content(doc, &h2s, structure, ctx, out);

    // Empty optional section detection
    //
    // A section is "empty" if its body contains only BlankLines (no substantive
    // content after the H2 heading).  A section is "optional" if its schema def
    // has `required: false`.  We batch all such sections into a single
    // diagnostic so the fix can remove them atomically in reverse index order.
    let mut empty_ranges: Vec<(usize, usize)> = Vec::new();
    for (pos_idx, (start_pos, section_title, _)) in h2s.iter().enumerate() {
        let Some(section_def) = structure
            .sections
            .iter()
            .find(|s| &s.title == section_title)
        else {
            continue; // unlisted section — not our business here
        };
        if section_def.required {
            continue; // required sections stay even when empty
        }

        let end_pos = h2s
            .get(pos_idx + 1)
            .map(|(pos, _, _)| *pos)
            .unwrap_or(doc.blocks.len());

        // Body = blocks between the H2 heading and the next section (exclusive)
        let body = &doc.blocks[start_pos + 1..end_pos];
        let is_empty = body.iter().all(|b| matches!(b, Block::BlankLine));
        if is_empty {
            // Range covers the H2 heading itself plus any blank lines
            empty_ranges.push((*start_pos, end_pos));
        }
    }
    if !empty_ranges.is_empty() {
        out.push(Diagnostic::EmptyOptionalSection {
            section_ranges: empty_ranges,
        });
    }
}

fn validate_section_content(
    doc: &Document,
    h2s: &[(usize, String, usize)],
    structure: &StructureDef,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    for (pos_idx, (start_pos, section_title, heading_line)) in h2s.iter().enumerate() {
        let Some(section_def) = structure
            .sections
            .iter()
            .find(|s| &s.title == section_title)
            .or_else(|| structure.sections.iter().find(|s| s.title == "*"))
        else {
            continue; // Unknown section; already reported in strict mode
        };

        let end_pos = h2s
            .get(pos_idx + 1)
            .map(|(pos, _, _)| *pos)
            .unwrap_or(doc.blocks.len());

        let section_blocks = &doc.blocks[start_pos + 1..end_pos];

        // Managed sections are validated separately
        if section_def.managed_content.is_some() {
            continue;
        }

        // Bullets mode: explicit `bullets:` or implied by `template:`
        if let Some(mode) = section_def.effective_bullet_mode() {
            validate_bullets_only(
                section_blocks,
                &format!("section '{section_title}'"),
                mode,
                out,
            );

            // Template matching
            if let Some(ref template) = section_def.template {
                let segments = parse_template(template);
                for block in section_blocks {
                    if let Block::List { items, .. } = block {
                        for item in items {
                            validate_item_template(
                                item,
                                *heading_line,
                                section_title,
                                template,
                                &segments,
                                out,
                            );
                        }
                    }
                }
            }
        }

        // Link constraints (always checked, regardless of content mode)
        if let Some(ref links_def) = section_def.links {
            validate_section_links(section_blocks, *heading_line, links_def, ctx, out);
        }

        // Property validation for sections that declare a property map
        if let Some(ref properties_def) = section_def.properties {
            validate_section_properties(
                section_blocks,
                properties_def,
                *heading_line,
                section_title,
                out,
            );
        }
    }
}

// ── Property validation ───────────────────────────────────────────────────────

/// Validate all top-level list items in a section against the declared property map.
fn validate_section_properties(
    section_blocks: &[Block],
    properties_def: &indexmap::IndexMap<String, FieldDef>,
    heading_line: usize,
    section_title: &str,
    out: &mut Vec<Diagnostic>,
) {
    // Collect all top-level items across list blocks.
    let all_items: Vec<&ListItem> = section_blocks
        .iter()
        .filter_map(|b| {
            if let Block::List { items, .. } = b {
                Some(items.iter())
            } else {
                None
            }
        })
        .flatten()
        .collect();

    // Detect flat section-level properties: every item is a plain `Key: Value`
    // line with no sub-items (mirrors extract_flat_section_properties in json.rs).
    let all_flat = !all_items.is_empty()
        && all_items.iter().all(|item| {
            item.children.is_empty() && inlines_to_string(&item.content).contains(": ")
        });

    if all_flat {
        validate_flat_section_properties(
            &all_items,
            properties_def,
            heading_line,
            section_title,
            out,
        );
    } else {
        for item in &all_items {
            validate_item_properties(item, properties_def, heading_line, section_title, out);
        }
    }
}

/// Validate flat `- Key: Value` bullets as section-level properties.
fn validate_flat_section_properties(
    items: &[&ListItem],
    properties_def: &indexmap::IndexMap<String, FieldDef>,
    heading_line: usize,
    section_title: &str,
    out: &mut Vec<Diagnostic>,
) {
    let mut found: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for item in items {
        let text = inlines_to_string(&item.content);
        if let Some((k, v)) = text.split_once(": ") {
            found.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    for (prop_name, prop_def) in properties_def {
        if let Some(value_str) = found.get(prop_name.as_str()) {
            if let Some(msg) = validate_property_str(value_str, prop_def) {
                out.push(Diagnostic::InvalidFieldType {
                    line: heading_line,
                    field: format!("{section_title}/{prop_name}"),
                    message: msg,
                });
            }
        } else if prop_def.required {
            out.push(Diagnostic::MissingRequiredField {
                line: heading_line,
                field: format!("{section_title}/{prop_name}"),
            });
        }
    }
}

/// Validate the sub-items of a single list item as key-value properties.
fn validate_item_properties(
    item: &ListItem,
    properties_def: &indexmap::IndexMap<String, FieldDef>,
    heading_line: usize,
    section_title: &str,
    out: &mut Vec<Diagnostic>,
) {
    // Collect all key-value pairs from child lists
    let mut found: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for child in &item.children {
        if let Block::List {
            items: sub_items, ..
        } = child
        {
            for sub_item in sub_items {
                let text = inlines_to_string(&sub_item.content);
                if let Some((k, v)) = text.split_once(": ") {
                    found.insert(k.trim().to_lowercase(), v.trim().to_string());
                }
            }
        }
    }

    for (prop_name, prop_def) in properties_def {
        if let Some(value_str) = found.get(prop_name.as_str()) {
            if let Some(msg) = validate_property_str(value_str, prop_def) {
                out.push(Diagnostic::InvalidFieldType {
                    line: heading_line,
                    field: format!("{section_title}/{prop_name}"),
                    message: msg,
                });
            }
        } else if prop_def.required {
            out.push(Diagnostic::MissingRequiredField {
                line: heading_line,
                field: format!("{section_title}/{prop_name}"),
            });
        }
    }
}

/// Validate a property value expressed as a plain string (from inline markdown content).
/// Returns `Some(error_message)` if invalid.
fn validate_property_str(value: &str, field_def: &FieldDef) -> Option<String> {
    match field_def.field_type {
        FieldType::Integer => {
            if value.parse::<i64>().is_err() && value.parse::<u64>().is_err() {
                return Some(format!("must be an integer (got '{value}')"));
            }
        }
        FieldType::Float => {
            if value.parse::<f64>().is_err() {
                return Some(format!("must be a number (got '{value}')"));
            }
        }
        FieldType::Bool => match value.to_lowercase().as_str() {
            "true" | "false" | "yes" | "no" => {}
            _ => return Some(format!("must be a boolean (got '{value}')")),
        },
        FieldType::Date => {
            if parse_date(value).is_none() {
                return Some(format!("must be a valid date (got '{value}')"));
            }
        }
        FieldType::Datetime => {
            if parse_datetime(value).is_none() {
                return Some(format!("must be a valid datetime (got '{value}')"));
            }
        }
        FieldType::Enum => {
            if let Some(valid) = &field_def.values {
                if !valid.contains(&value.to_string()) {
                    return Some(format!(
                        "must be one of: {} (got '{value}')",
                        valid.join(", ")
                    ));
                }
            }
        }
        FieldType::String => {}
        FieldType::List => return Some("list type not supported for properties".to_string()),
    }
    None
}

fn validate_item_template(
    item: &ListItem,
    heading_line: usize,
    section_title: &str,
    template: &str,
    segments: &[crate::schema::TemplateSegment],
    out: &mut Vec<Diagnostic>,
) {
    let item_text = format!("- {}", inlines_to_markdown(&item.content));
    if !matches_template(&item_text, segments) {
        out.push(Diagnostic::TemplateMismatch {
            line: heading_line,
            section: section_title.to_string(),
            template: template.to_string(),
        });
    }
    // Recurse into nested list items
    for child in &item.children {
        if let Block::List { items, .. } = child {
            for child_item in items {
                validate_item_template(
                    child_item,
                    heading_line,
                    section_title,
                    template,
                    segments,
                    out,
                );
            }
        }
    }
}

/// Compare two slices of blocks for structural equality, ignoring source line numbers.
///
/// `Block::PartialEq` includes the `line` field, so template blocks (parsed from
/// a string at line 1) would never compare equal to document blocks (parsed from a
/// file at arbitrary line numbers). This helper zeroes every line before comparing.
fn blocks_content_equal(a: &[Block], b: &[Block]) -> bool {
    fn zero_line(block: &Block) -> Block {
        match block {
            Block::Heading { level, content, .. } => Block::Heading {
                level: *level,
                content: content.clone(),
                line: 0,
            },
            Block::Paragraph { content, .. } => Block::Paragraph {
                content: content.clone(),
                line: 0,
            },
            Block::List {
                items,
                ordered,
                start,
                ..
            } => Block::List {
                items: items
                    .iter()
                    .map(|item| crate::ast::ListItem {
                        content: item.content.clone(),
                        children: item.children.iter().map(zero_line).collect(),
                    })
                    .collect(),
                ordered: *ordered,
                start: *start,
                line: 0,
            },
            Block::CodeBlock {
                language, content, ..
            } => Block::CodeBlock {
                language: language.clone(),
                content: content.clone(),
                line: 0,
            },
            Block::BlockQuote { blocks, .. } => Block::BlockQuote {
                blocks: blocks.iter().map(zero_line).collect(),
                line: 0,
            },
            Block::Table {
                alignments,
                header,
                rows,
                ..
            } => Block::Table {
                alignments: alignments.clone(),
                header: header.clone(),
                rows: rows.clone(),
                line: 0,
            },
            Block::ThematicBreak { .. } => Block::ThematicBreak { line: 0 },
            Block::Html { content, .. } => Block::Html {
                content: content.clone(),
                line: 0,
            },
            Block::BlankLine => Block::BlankLine,
        }
    }

    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| zero_line(x) == zero_line(y))
}

fn validate_managed_section(
    doc: &Document,
    section_title: &str,
    managed: &ManagedContent,
    out: &mut Vec<Diagnostic>,
) {
    let template_blocks: Vec<Block> = parse(&managed.template).blocks;

    match find_managed_section(doc, section_title, &managed.migrate_from) {
        Some((idx, legacy_sections)) => {
            let last_section_idx = legacy_sections.last().copied().unwrap_or(idx);
            let section_end = doc.blocks[last_section_idx + 1..]
                .iter()
                .position(|b| matches!(b, Block::Heading { level: 2, .. }))
                .map(|i| last_section_idx + 1 + i)
                .unwrap_or(doc.blocks.len());

            let existing: Vec<Block> = doc.blocks[idx..section_end]
                .iter()
                .filter(|b| !matches!(b, Block::BlankLine))
                .cloned()
                .collect();

            let (managed_blocks, custom_content) = match managed.merge {
                MergeMode::Replace => (template_blocks, Vec::new()),
                MergeMode::Upsert => upsert_managed_section(&template_blocks, &existing),
            };

            // The section already says what the schema asks for when the resolved
            // content matches it block for block. Blank lines are excluded: they
            // are re-derived on serialization, not authored.
            let resolved: Vec<Block> = managed_blocks
                .iter()
                .chain(custom_content.iter())
                .filter(|b| !matches!(b, Block::BlankLine))
                .cloned()
                .collect();

            let needs_update =
                !legacy_sections.is_empty() || !blocks_content_equal(&existing, &resolved);

            if needs_update {
                out.push(Diagnostic::ManagedSectionNeedsUpdate {
                    section_start: Some(idx),
                    section_end,
                    managed_blocks,
                    custom_content,
                });
            }
        }
        None => {
            out.push(Diagnostic::ManagedSectionNeedsUpdate {
                section_start: None,
                section_end: doc.blocks.len(),
                managed_blocks: template_blocks,
                custom_content: vec![],
            });
        }
    }
}

/// Merge a `managed_content` template into the blocks a section already has.
///
/// Returns `(managed_blocks, preserved_blocks)`:
///
/// - `managed_blocks` is the template, with each of its lists upserted: the
///   template's own items first (verbatim, so their wording is normalised),
///   then every existing item the template doesn't declare, in the order the
///   document had them.
/// - `preserved_blocks` is every other block the section had that the template
///   didn't account for — prose, a second list, a leftover legacy heading. The
///   fix appends these after the managed blocks.
///
/// Items are matched by identity, not position (see [`item_key`]), which is the
/// whole point: a curated list whose entries sit in a different order, or are
/// worded differently, gets its templated entries rewritten and keeps its own
/// rather than having them truncated away.
fn upsert_managed_section(
    template_blocks: &[Block],
    existing: &[Block],
) -> (Vec<Block>, Vec<Block>) {
    let mut consumed = vec![false; existing.len()];

    // The span always opens with the section's H2. The template supplies the
    // canonical spelling of it — that is how `migrate_from` renames a section.
    if matches!(existing.first(), Some(Block::Heading { level: 2, .. })) {
        consumed[0] = true;
    }

    let mut managed = Vec::with_capacity(template_blocks.len());
    for block in template_blocks {
        match block {
            Block::List {
                items,
                ordered,
                start,
                ..
            } => {
                let mut merged = items.clone();

                // Pair with the first list of the same kind the section still
                // has unclaimed, and adopt every item the template doesn't name.
                let paired = (0..existing.len()).find(|&i| {
                    !consumed[i]
                        && matches!(&existing[i], Block::List { ordered: o, .. } if o == ordered)
                });
                if let Some(i) = paired {
                    consumed[i] = true;
                    if let Block::List {
                        items: existing_items,
                        ..
                    } = &existing[i]
                    {
                        let templated: HashSet<String> = items.iter().map(item_key).collect();
                        merged.extend(
                            existing_items
                                .iter()
                                .filter(|item| !templated.contains(&item_key(item)))
                                .cloned(),
                        );
                    }
                }

                managed.push(Block::List {
                    items: merged,
                    ordered: *ordered,
                    start: *start,
                    line: 0,
                });
            }
            other => {
                // A block the section already carries verbatim is accounted for
                // by the template; don't preserve a duplicate of it below.
                let same = (0..existing.len()).find(|&i| {
                    !consumed[i]
                        && blocks_content_equal(
                            std::slice::from_ref(&existing[i]),
                            std::slice::from_ref(other),
                        )
                });
                if let Some(i) = same {
                    consumed[i] = true;
                }
                managed.push(other.clone());
            }
        }
    }

    let preserved = existing
        .iter()
        .zip(consumed)
        .filter(|(_, claimed)| !claimed)
        .map(|(block, _)| block.clone())
        .collect();

    (managed, preserved)
}

/// The identity of a managed list item, used to decide whether a template item
/// and a document item are the same entry.
///
/// The first link URL wins, then the first bold run or code span, then the
/// item's plain text. Compared case- and whitespace-insensitively, so
/// re-capitalising an entry's description doesn't fork it into two bullets.
fn item_key(item: &ListItem) -> String {
    for inline in &item.content {
        match inline {
            Inline::Link { url, .. } => return normalize_item_key(url),
            Inline::Strong(inner) => return normalize_item_key(&inlines_to_string(inner)),
            Inline::Code(span) => return normalize_item_key(&span.text),
            _ => {}
        }
    }
    normalize_item_key(&inlines_to_string(&item.content))
}

fn normalize_item_key(text: &str) -> String {
    text.trim().to_lowercase()
}

/// Find a managed section by title (or legacy titles to migrate from).
///
/// Returns `(canonical_idx, legacy_indices)`. If the canonical section is found,
/// `legacy_indices` is empty. If only legacy sections are found, `canonical_idx`
/// is the first legacy section and `legacy_indices` lists all of them.
fn find_managed_section(
    doc: &Document,
    section_title: &str,
    migrate_from: &[String],
) -> Option<(usize, Vec<usize>)> {
    let mut legacy = Vec::new();
    let mut first_legacy_idx = None;

    for (i, block) in doc.blocks.iter().enumerate() {
        if let Block::Heading {
            level: 2, content, ..
        } = block
        {
            let text = inlines_to_string(content);
            let text_lower = text.to_lowercase();

            if text_lower == section_title.to_lowercase() {
                return Some((i, vec![]));
            }

            if migrate_from.iter().any(|m| m.to_lowercase() == text_lower) {
                if first_legacy_idx.is_none() {
                    first_legacy_idx = Some(i);
                }
                legacy.push(i);
            }
        }
    }

    first_legacy_idx.map(|idx| (idx, legacy))
}

// ── Link validation ───────────────────────────────────────────────────────────

/// Validate every local link in the document exists in `ctx.linked_docs` or `ctx.git_tree`.
///
/// External links (`http://`, `https://`) and anchor-only links (`#…`) are
/// skipped. `linked_docs` covers typed docs in the walk scope. `git_tree`
/// covers everything else tracked in HEAD (e.g. cross-project links).
///
/// A link that leaves the project is only judged when the tree it points into
/// is part of this checkout — see [`foreign_root`].
fn validate_all_links(doc: &Document, ctx: &ValidateCtx<'_>, out: &mut Vec<Diagnostic>) {
    let all_urls = extract_links(&doc.blocks);
    for (url, line) in all_urls {
        if url.starts_with("http://") || url.starts_with("https://") || url.starts_with('#') {
            continue;
        }
        let Some(target) = resolve_link_path(&url, ctx.source_path) else {
            continue;
        };
        let in_linked = ctx.linked_docs.contains_key(&target);
        let in_git = ctx.git_tree.is_some_and(|t| t.files.contains(&target));
        if in_linked || in_git {
            continue;
        }
        let unverifiable = foreign_root(&target, ctx.schema_root)
            .is_some_and(|root| ctx.git_tree.is_some_and(|t| !t.dirs.contains(&root)));
        if unverifiable {
            continue;
        }
        out.push(Diagnostic::UnknownType {
            line,
            message: format!("broken link: '{url}' does not exist"),
        });
    }
}

/// The top of the foreign directory tree `target` points into, or `None` when
/// the link stays inside `schema_root`.
///
/// A link out of the project can only be called broken when the tree it lands
/// in is actually here. Under a partial checkout the neighbour isn't — it was
/// never fetched, so a missing file there says nothing about the link. This
/// names the unit that is present or absent as a whole: for a document in
/// `<parent>/here` linking `../../there/notes/A.md`, the shared ancestor is
/// `<parent>` and the foreign root is `<parent>/there`.
///
/// When the target sits *directly* in the shared ancestor (`../../README.md`)
/// there is no foreign tree to blame, so the ancestor itself is returned and
/// the link is judged normally — the ancestor is on the path to this project
/// and therefore always present.
fn foreign_root(target: &Path, schema_root: Option<&Path>) -> Option<PathBuf> {
    let schema_root = schema_root?;
    if target.starts_with(schema_root) {
        return None;
    }
    let mut shared = PathBuf::new();
    let mut root_components = schema_root.components();
    let mut target_components = target.components().peekable();
    while let Some(component) = target_components.peek() {
        if root_components.next() != Some(*component) {
            break;
        }
        shared.push(component);
        target_components.next();
    }
    // One component left means the target is a file sitting in the shared
    // ancestor, not inside a neighbouring tree.
    let mut rest = target_components;
    let first = rest.next()?;
    if rest.next().is_none() {
        return Some(shared);
    }
    shared.push(first);
    Some(shared)
}

/// Extract all link and image URLs from a block list, with the line each sits on.
pub fn extract_links(blocks: &[Block]) -> Vec<(String, usize)> {
    ast::links(blocks)
        .into_iter()
        .map(|link| (link.url.to_string(), link.line))
        .collect()
}

/// Resolve a relative link URL against a source file path (pure, no I/O).
///
/// Returns `None` for external URLs and anchor-only links.
pub fn resolve_link_path(link: &str, source_path: &Path) -> Option<PathBuf> {
    if link.starts_with("http://") || link.starts_with("https://") || link.starts_with('#') {
        return None;
    }

    let path_only = link.split_once('#').map_or(link, |(p, _)| p);
    let path_only = path_only.split_once('?').map_or(path_only, |(p, _)| p);
    if path_only.is_empty() {
        return None;
    }

    let decoded = percent_decode(path_only);
    let base_dir = source_path.parent()?;
    let resolved = base_dir.join(Path::new(&decoded));
    Some(normalize_path(&resolved))
}

/// Simple percent-decoding for URL path components (e.g. `%20` → space).
fn percent_decode(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string())
}

/// Normalize a path (resolve `..` and `.` without touching the filesystem).
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::ParentDir => {
                components.pop();
            }
            Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

fn validate_section_links(
    section_blocks: &[Block],
    heading_line: usize,
    links_def: &LinksDef,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let links = extract_links(section_blocks);

    for (url, _link_line) in &links {
        if url.starts_with("http://") || url.starts_with("https://") {
            continue;
        }

        let Some(target_path) = resolve_link_path(url, ctx.source_path) else {
            continue;
        };

        let linked = ctx.linked_docs.get(&target_path);

        // target_type constraint
        if let Some(ref expected_type) = links_def.target_type {
            let actual_type = linked.and_then(|d| d.doc_type.as_deref());
            match actual_type {
                Some(t) if t == expected_type => {}
                Some(t) => {
                    out.push(Diagnostic::LinkTargetTypeMismatch {
                        line: heading_line,
                        url: url.clone(),
                        expected: expected_type.clone(),
                        actual: Some(t.to_string()),
                    });
                    continue;
                }
                None => {
                    out.push(Diagnostic::LinkTargetTypeMismatch {
                        line: heading_line,
                        url: url.clone(),
                        expected: expected_type.clone(),
                        actual: None,
                    });
                    continue;
                }
            }
        }

        // bidirectional constraint
        if links_def.bidirectional {
            if let Some(linked) = linked {
                if let Some(ref target_type_name) = linked.doc_type {
                    validate_bidirectional_link(
                        url,
                        linked,
                        target_type_name,
                        heading_line,
                        ctx,
                        out,
                    );
                }
            }
        }
    }
}

fn validate_bidirectional_link(
    url: &str,
    linked: &LinkedDocInfo,
    target_type: &str,
    heading_line: usize,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(target_type_def) = ctx
        .schema
        .get_type(target_type)
        .or_else(|| ctx.external_types.get(target_type))
    else {
        out.push(Diagnostic::UnknownType {
            line: heading_line,
            message: format!(
                "bidirectional link to '{url}': target type '{target_type}' not in schema"
            ),
        });
        return;
    };

    // Find sections in target schema that link back to source_type
    let inverse_sections: Vec<_> = target_type_def
        .structure
        .sections
        .iter()
        .filter(|s| {
            s.links
                .as_ref()
                .and_then(|l| l.target_type.as_ref())
                .is_some_and(|t| t == ctx.source_type)
        })
        .collect();

    if inverse_sections.is_empty() {
        out.push(Diagnostic::UnknownType {
            line: heading_line,
            message: format!(
                "bidirectional link to '{url}': target type '{target_type}' has no section linking to '{}'",
                ctx.source_type
            ),
        });
        return;
    }

    // Check for backlink in any inverse section
    let source_abs = normalize_path(ctx.source_path);
    let has_backlink = inverse_sections.iter().any(|sec| {
        linked.section_links.get(&sec.title).is_some_and(|urls| {
            urls.iter().any(|target_link| {
                resolve_link_path(target_link, &linked.path).is_some_and(|p| p == source_abs)
            })
        })
    });

    if !has_backlink {
        let inverse_section_names: Vec<_> =
            inverse_sections.iter().map(|s| s.title.as_str()).collect();
        out.push(Diagnostic::MissingBacklink {
            line: heading_line,
            url: url.to_string(),
            inverse_section: inverse_section_names.join("' or '"),
        });
    }
}

// ── Date/datetime parsing ─────────────────────────────────────────────────────

/// Parse a date string (YYYY-MM-DD and common variants).
pub fn parse_date(s: &str) -> Option<()> {
    let formats: &[&str] = &[
        "%Y-%m-%d",
        "%Y/%m/%d",
        "%B %d, %Y",
        "%b %d, %Y",
        "%d %B %Y",
        "%d %b %Y",
    ];
    for fmt in formats {
        if chrono::NaiveDate::parse_from_str(s, fmt).is_ok() {
            return Some(());
        }
    }
    // Fall back: try parsing as datetime and extract date
    if parse_datetime(s).is_some() {
        return Some(());
    }
    None
}

/// Parse a datetime string (ISO 8601 and common variants).
pub fn parse_datetime(s: &str) -> Option<()> {
    use chrono::{DateTime, NaiveDateTime};

    if DateTime::parse_from_rfc3339(s).is_ok() {
        return Some(());
    }

    let formats: &[&str] = &[
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S %z",
        "%Y-%m-%d %H:%M:%S %Z",
    ];

    for fmt in formats {
        if NaiveDateTime::parse_from_str(s, fmt).is_ok() {
            return Some(());
        }
    }

    // Trim trailing UTC / +0000 UTC and retry
    let cleaned = s.trim_end_matches(" UTC").trim_end_matches(" +0000 UTC");
    for fmt in formats {
        if NaiveDateTime::parse_from_str(cleaned, fmt).is_ok() {
            return Some(());
        }
    }

    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use crate::schema::{Schema, TypeDef};

    fn make_schema(type_name: &str, yaml: &str) -> (Schema, TypeDef) {
        let type_def: TypeDef = serde_yaml::from_str(yaml).expect("type yaml should parse");
        let mut schema = Schema::default();
        schema.types.insert(type_name.to_string(), type_def.clone());
        (schema, type_def)
    }

    fn empty_path() -> &'static Path {
        Path::new("test.md")
    }

    fn empty_linked_docs() -> &'static HashMap<PathBuf, LinkedDocInfo> {
        static EMPTY: OnceLock<HashMap<PathBuf, LinkedDocInfo>> = OnceLock::new();
        EMPTY.get_or_init(HashMap::new)
    }

    fn empty_external_types() -> &'static HashMap<String, TypeDef> {
        static EMPTY: OnceLock<HashMap<String, TypeDef>> = OnceLock::new();
        EMPTY.get_or_init(HashMap::new)
    }

    /// Convenience: build a ValidateCtx for tests (no git tree, empty linked_docs by default).
    fn make_ctx<'a>(
        source_type: &'a str,
        source_path: &'a Path,
        schema: &'a Schema,
        linked_docs: &'a HashMap<PathBuf, LinkedDocInfo>,
    ) -> ValidateCtx<'a> {
        ValidateCtx {
            source_path,
            schema_root: None,
            source_type,
            schema,
            linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        }
    }

    // ── Frontmatter / fields ──────────────────────────────────────────────────

    // ── v2: json-schema frontmatter ───────────────────────────────────────────

    /// A v2 `security` type exercising required, enum, pattern, range, format
    /// number, and array-of-enum — the shapes a real frontmatter census used.
    const V2_SECURITY: &str = r#"
version: 2
frontmatter:
  type: object
  properties:
    ticker:
      type: string
      pattern: "^[A-Z.]+$"
    asset_class:
      type: string
      enum: [equity, cash]
    opened:
      type: string
      format: date
    weight:
      type: integer
      minimum: 0
      maximum: 100
    rating:
      type: number
    tags:
      type: array
      items:
        type: string
        enum: [core, satellite]
  required: [ticker, asset_class]
"#;

    fn v2_diags(doc_src: &str) -> Vec<Diagnostic> {
        let (schema, type_def) = make_schema("security", V2_SECURITY);
        let doc = parse(doc_src);
        validate(
            &doc,
            &type_def,
            &make_ctx("security", empty_path(), &schema, empty_linked_docs()),
            None,
        )
    }

    #[test]
    fn test_v2_valid_document_no_errors() {
        let diags = v2_diags(
            "---\ntype: security\nticker: AAPL\nasset_class: equity\nweight: 40\ntags: [core]\n---\n# AAPL\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_v2_missing_required_field() {
        let diags = v2_diags("---\ntype: security\nticker: AAPL\n---\n# AAPL\n");
        assert_eq!(
            diags,
            vec![Diagnostic::MissingRequiredField {
                line: 1,
                field: "asset_class".to_string(),
            }],
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_v2_diagnostics_carry_the_field_line() {
        let diags = v2_diags(
            "---\ntype: security\nticker: aapl\nasset_class: equity\nweight: 300\n---\n# AAPL\n",
        );
        let lines: Vec<(usize, &str)> = diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::InvalidFieldType { line, field, .. } => Some((*line, field.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(lines, vec![(3, "ticker"), (5, "weight")], "got: {diags:?}");
    }

    #[test]
    fn test_v2_array_item_error_names_the_index() {
        let diags = v2_diags(
            "---\ntype: security\nticker: AAPL\nasset_class: equity\ntags: [core, bogus]\n---\n# AAPL\n",
        );
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::InvalidFieldType { line: 5, field, .. } if field == "tags[1]"
            )),
            "expected a tags[1] diagnostic on line 5, got: {diags:?}"
        );
    }

    fn opened_diags(date: &str) -> Vec<Diagnostic> {
        v2_diags(&format!(
            "---\ntype: security\nticker: AAPL\nasset_class: equity\nopened: {date}\n---\n# AAPL\n"
        ))
    }

    #[test]
    fn test_v2_date_format_accepts_iso_8601() {
        let diags = opened_diags("2026-03-04");
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_v2_date_format_rejects_non_iso_spellings() {
        // v1 `type: date` took all of these; v2 is ISO 8601 only.
        for date in [
            "someday",
            "2026/03/04",
            "March 4, 2026",
            "2026-03-04 14:30",
            "2026-3-4",
            "2026-02-30", // real calendar validation, not just shape
        ] {
            let diags = opened_diags(date);
            assert!(
                diags.iter().any(|d| matches!(
                    d,
                    Diagnostic::InvalidFieldType { line: 5, field, .. } if field == "opened"
                )),
                "'{date}' should be rejected, got: {diags:?}"
            );
        }
    }

    #[test]
    fn test_v2_date_time_format_is_rfc_3339() {
        let (schema, type_def) = make_schema(
            "note",
            "version: 2\nfrontmatter:\n  type: object\n  properties:\n    seen:\n      type: string\n      format: date-time\n",
        );
        let check = |value: &str| {
            let doc = parse(&format!("---\ntype: note\nseen: {value}\n---\n# N\n"));
            validate(
                &doc,
                &type_def,
                &make_ctx("note", empty_path(), &schema, empty_linked_docs()),
                None,
            )
        };
        assert!(check("2026-03-04T14:30:00Z").is_empty());
        assert!(check("2026-03-04T14:30:00+05:00").is_empty());
        // RFC 3339 wants a separator, seconds and an offset — ISO 8601 would
        // allow a bare local time, but the crate's format doesn't.
        assert!(!check("2026-03-04 14:30:00Z").is_empty());
        assert!(!check("2026-03-04T14:30").is_empty());
    }

    #[test]
    fn test_task_preset_accepts_bare_dates_in_last_runs() {
        // `last-runs:` is documented, and written by every real recurring
        // task, as bare `YYYY-MM-DD` completion dates. The v1 field carried
        // `item_type: datetime`, and the v2 translation kept `format:
        // date-time` — which would have rejected all of that data.
        let (_, content) = crate::schema::BUILTIN_PRESETS
            .iter()
            .find(|(name, _)| *name == "task")
            .expect("task preset should exist");
        let (schema, type_def) = make_schema("task", content);
        let check = |last_runs: &str| {
            let doc = parse(&format!(
                "---\ntype: task\ndescription: Weekly sweep\nrecurring: weekly\nlast-runs:\n{last_runs}---\n\nDo the sweep.\n"
            ));
            validate(
                &doc,
                &type_def,
                &make_ctx("task", empty_path(), &schema, empty_linked_docs()),
                None,
            )
        };

        let diags = check("  - 2026-08-14\n  - 2026-08-07\n  - 2026-07-31\n");
        assert!(
            diags.is_empty(),
            "bare dates should validate, got: {diags:?}"
        );

        let diags = check("  - 2026-08-14\n  - not-a-date\n");
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::InvalidFieldType { field, .. } if field.starts_with("last-runs")
            )),
            "garbage entry should be rejected, got: {diags:?}"
        );
    }

    #[test]
    fn test_v2_type_field_is_visible_to_the_json_schema() {
        let (schema, type_def) = make_schema(
            "note",
            "version: 2\nfrontmatter:\n  type: object\n  properties:\n    type:\n      const: note\n",
        );
        let doc = parse("---\ntype: note\n---\n# Note\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("note", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_v2_additional_properties_can_be_closed() {
        let (schema, type_def) = make_schema(
            "note",
            "version: 2\nfrontmatter:\n  type: object\n  properties:\n    type: {}\n    name:\n      type: string\n  additionalProperties: false\n",
        );
        let doc = parse("---\ntype: note\nname: n\nstray: x\n---\n# Note\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("note", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert_eq!(diags.len(), 1, "expected one error, got: {diags:?}");
    }

    #[test]
    fn test_v2_structure_rules_still_apply() {
        let (schema, type_def) = make_schema(
            "note",
            "version: 2\nfrontmatter:\n  type: object\nstructure:\n  title: from_filename\n",
        );
        let doc = parse("---\ntype: note\n---\n# Wrong\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("note", Path::new("right.md"), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::H1Mismatch { .. })),
            "expected H1Mismatch, got: {diags:?}"
        );
    }

    #[test]
    fn test_missing_frontmatter() {
        let (schema, type_def) = make_schema("note", "version: 2\ndescription: a note\n");
        let doc = parse("# Hello\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("note", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingFrontmatter)),
            "expected MissingFrontmatter, got: {diags:?}"
        );
    }

    #[test]
    fn test_type_mismatch() {
        let (schema, type_def) = make_schema("note", "version: 2\ndescription: a note\n");
        let doc = parse("---\ntype: other\n---\n# Hello\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("note", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::TypeMismatch { .. })),
            "expected TypeMismatch, got: {diags:?}"
        );
    }

    fn rating_diags(rating: &str) -> Vec<Diagnostic> {
        v2_diags(&format!(
            "---\ntype: security\nticker: AAPL\nasset_class: equity\nrating: {rating}\n---\n# AAPL\n"
        ))
    }

    #[test]
    fn test_number_field_accepts_integer() {
        assert!(rating_diags("3").is_empty());
    }

    #[test]
    fn test_number_field_accepts_decimal() {
        assert!(rating_diags("3.14").is_empty());
    }

    #[test]
    fn test_number_field_rejects_a_string() {
        let diags = rating_diags("hello");
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::InvalidFieldType { field, .. } if field == "rating"
            )),
            "got: {diags:?}"
        );
    }

    // ── Structure / title ─────────────────────────────────────────────────────

    #[test]
    fn test_title_from_filename() {
        let (schema, type_def) = make_schema("page", "structure:\n  title: from_filename\n");
        let doc = parse("---\ntype: page\n---\n# my-page\n");
        let path = Path::new("my-page.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("page", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_title_from_filename_mismatch() {
        let (schema, type_def) = make_schema("page", "structure:\n  title: from_filename\n");
        let doc = parse("---\ntype: page\n---\n# wrong title\n");
        let path = Path::new("my-page.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("page", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::H1Mismatch { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_fixed_title_mismatch() {
        let (schema, type_def) = make_schema("t", "structure:\n  title: Roadmap\n");
        let doc = parse("---\ntype: t\n---\n# Wrong Title\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::H1Mismatch { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_required_any_title_missing() {
        let (schema, type_def) = make_schema("t", "structure:\n  title: required\n");
        let doc = parse("---\ntype: t\n---\nNo heading here.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingH1 { .. })),
            "got: {diags:?}"
        );
    }

    // ── Sections ──────────────────────────────────────────────────────────────

    #[test]
    fn test_unexpected_section_strict() {
        let yaml = r"
structure:
  sections:
    - title: Goals
      required: false
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Surprise\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::UnexpectedSection { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_missing_required_section() {
        let yaml = r"
structure:
  sections:
    - title: Goals
      required: true
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\nNo sections.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingSection { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_section_order_enforced() {
        let yaml = r"
structure:
  sections:
    - title: Alpha
    - title: Beta
";
        let (schema, type_def) = make_schema("t", yaml);
        // Beta appears before Alpha → out of order
        let doc = parse("---\ntype: t\n---\n## Beta\n\n## Alpha\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::SectionOutOfOrder { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_section_order_emits_fixable_diagnostic() {
        let yaml = r"
structure:
  sections:
    - title: Alpha
    - title: Beta
    - title: Gamma
";
        let (schema, type_def) = make_schema("t", yaml);
        // Gamma before Alpha → out of order
        let doc = parse("---\ntype: t\n---\n# Doc\n\n## Gamma\n\nG content.\n\n## Alpha\n\nA content.\n\n## Beta\n\nB content.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        // Individual per-section diagnostic
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::SectionOutOfOrder { .. })),
            "should have SectionOutOfOrder: {diags:?}"
        );
        // Fixable aggregate diagnostic
        let reorder = diags
            .iter()
            .find(|d| matches!(d, Diagnostic::SectionsOutOfOrder { .. }));
        assert!(
            reorder.is_some(),
            "should have SectionsOutOfOrder: {diags:?}"
        );
        // All diagnostics should be fixable
        assert!(
            diags.iter().all(crate::fix::Fix::is_fixable),
            "all diagnostics should be fixable: {diags:?}"
        );
    }

    #[test]
    fn test_section_order_correct_no_reorder_diagnostic() {
        let yaml = r"
structure:
  sections:
    - title: Alpha
    - title: Beta
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Alpha\n\n## Beta\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::SectionsOutOfOrder { .. })),
            "should not have SectionsOutOfOrder when in order: {diags:?}"
        );
    }

    #[test]
    fn test_section_not_bullets() {
        // A bullets-only section rejects paragraph content
        let yaml = r"
structure:
  sections:
    - title: Goals
      bullets: unordered
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Goals\n\nThis is a paragraph.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::SectionNotBullets { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_section_paragraph_allowed_by_default() {
        // Sections without `bullets:` allow any content (paragraphs are the default)
        let yaml = r"
structure:
  sections:
    - title: Notes
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Notes\n\nFree text here.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_section_bullets_unordered_rejects_ordered() {
        let yaml = r"
structure:
  sections:
    - title: Items
      bullets: unordered
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Items\n\n1. First\n2. Second\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::WrongListType { .. })),
            "expected WrongListType, got: {diags:?}"
        );
    }

    #[test]
    fn test_section_bullets_ordered_rejects_unordered() {
        let yaml = r"
structure:
  sections:
    - title: Steps
      bullets: ordered
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Steps\n\n- First\n- Second\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::WrongListType { .. })),
            "expected WrongListType, got: {diags:?}"
        );
    }

    #[test]
    fn test_template_implies_bullets_mode() {
        // A section with only `template:` should enforce bullets mode
        let yaml = r#"
structure:
  sections:
    - title: Features
      template: "- **Text**: Text"
"#;
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Features\n\nThis is a paragraph.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::SectionNotBullets { .. })),
            "template should imply bullets mode, got: {diags:?}"
        );
    }

    #[test]
    fn test_unknown_type() {
        let (schema, _) = make_schema("note", "description: a note\n");
        let doc = parse("---\ntype: missing\n---\n");
        let diags = validate_unknown_type(&doc, &schema);
        assert!(
            diags.iter().any(|d| d.message().contains("unknown type")),
            "got: {diags:?}"
        );
    }

    // ── Date/datetime parsing ─────────────────────────────────────────────────

    #[test]
    fn test_parse_date_formats() {
        assert!(parse_date("2024-03-15").is_some());
        assert!(parse_date("2024/03/15").is_some());
        assert!(parse_date("March 15, 2024").is_some());
        assert!(parse_date("15 March 2024").is_some());
        assert!(parse_date("not-a-date").is_none());
    }

    #[test]
    fn test_parse_datetime_formats() {
        assert!(parse_datetime("2024-03-15T08:07:43-04:00").is_some());
        assert!(parse_datetime("2024-03-15 08:07:43").is_some());
        assert!(parse_datetime("2024-03-15 08:07").is_some());
        assert!(parse_datetime("invalid").is_none());
    }

    // ── Link validation ───────────────────────────────────────────────────────

    #[test]
    fn test_link_target_type_mismatch() {
        let yaml = r"
structure:
  sections:
    - title: Related
      links:
        target_type: note
";
        let (schema, type_def) = make_schema("task", yaml);
        let source_path = Path::new("/proj/task.md");

        // linked doc is type "other", not "note"
        let linked = LinkedDocInfo {
            path: PathBuf::from("/proj/target.md"),
            doc_type: Some("other".to_string()),
            section_links: Default::default(),
        };
        let linked_docs = HashMap::from([(linked.path.clone(), linked)]);

        let doc = parse("---\ntype: task\n---\n## Related\n\n- [Target](target.md)\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("task", source_path, &schema, &linked_docs),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::LinkTargetTypeMismatch { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_resolve_link_path() {
        let source = Path::new("/proj/docs/task.md");
        let resolved = resolve_link_path("../notes/note.md", source);
        assert_eq!(resolved, Some(PathBuf::from("/proj/notes/note.md")));
    }

    #[test]
    fn test_resolve_external_link_skipped() {
        let source = Path::new("/proj/task.md");
        assert!(resolve_link_path("https://example.com", source).is_none());
        assert!(resolve_link_path("#anchor", source).is_none());
    }

    #[test]
    fn test_size_warning() {
        let (schema, type_def) = make_schema("t", "structure:\n  size_warning: 10\n");
        let doc = parse("---\ntype: t\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            Some(100),
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::FileTooLarge { .. })),
            "got: {diags:?}"
        );
    }

    // ── from_date title mode ──────────────────────────────────────────────────

    #[test]
    fn test_month_title_from_path() {
        assert_eq!(
            date_title_from_path(Path::new("journal/2026-02.md")),
            Some("February 2026".to_string())
        );
        assert_eq!(
            date_title_from_path(Path::new("2024-12.md")),
            Some("December 2024".to_string())
        );
        assert_eq!(date_title_from_path(Path::new("not-a-date.md")), None);
        assert_eq!(date_title_from_path(Path::new("2026-13.md")), None);
    }

    #[test]
    fn test_day_title_from_path() {
        assert_eq!(
            date_title_from_path(Path::new("health/2026-04-14.md")),
            Some("April 14, 2026".to_string())
        );
        // Single-digit days carry no leading zero, matching existing documents.
        assert_eq!(
            date_title_from_path(Path::new("usage/2026-04-01.md")),
            Some("April 1, 2026".to_string())
        );
        assert_eq!(
            date_title_from_path(Path::new("2024-02-29.md")),
            Some("February 29, 2024".to_string())
        );
    }

    #[test]
    fn test_date_title_from_path_rejects_non_dates() {
        for stem in [
            "2026-02-30",   // not a real date
            "2026-04-32",   // day out of range
            "2026-00-10",   // month out of range
            "1-2-3",        // too few digits
            "2026-04-14-x", // trailing component
            "2026",         // no month
            "notes",
            "",
        ] {
            assert_eq!(
                date_title_from_path(Path::new(&format!("{stem}.md"))),
                None,
                "expected '{stem}.md' to yield no title"
            );
        }
    }

    #[test]
    fn test_from_date_title_correct() {
        let (schema, type_def) = make_schema("journal", "structure:\n  title: from_date\n");
        let doc = parse("---\ntype: journal\n---\n# February 2026\n");
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        // No link validation on this small doc, no other issues
        let structural: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::MissingH1 { .. } | Diagnostic::H1Mismatch { .. }
                )
            })
            .collect();
        assert!(structural.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_from_date_title_mismatch() {
        let (schema, type_def) = make_schema("journal", "structure:\n  title: from_date\n");
        let doc = parse("---\ntype: journal\n---\n# Wrong Title\n");
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::H1Mismatch { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_from_date_title_day_precision() {
        let (schema, type_def) = make_schema("health", "structure:\n  title: from_date\n");
        let doc = parse("---\ntype: health\n---\n# April 14, 2026\n");
        let path = Path::new("health/2026-04-14.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("health", path, &schema, empty_linked_docs()),
            None,
        );
        let structural: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::MissingH1 { .. }
                        | Diagnostic::H1Mismatch { .. }
                        | Diagnostic::UndatedFilename { .. }
                )
            })
            .collect();
        assert!(structural.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_from_date_undated_filename_does_not_expect_empty_title() {
        let (schema, type_def) = make_schema("health", "structure:\n  title: from_date\n");
        let doc = parse("---\ntype: health\n---\n# Notes\n");
        let path = Path::new("health/notes.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("health", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::UndatedFilename { .. })),
            "got: {diags:?}"
        );
        // Crucially: no H1 diagnostic, so no fix can rewrite the existing H1.
        assert!(
            !diags.iter().any(|d| matches!(
                d,
                Diagnostic::MissingH1 { .. } | Diagnostic::H1Mismatch { .. }
            )),
            "an underivable title must not assert an expected H1, got: {diags:?}"
        );
        assert!(
            !diags.iter().any(crate::fix::Fix::is_fixable),
            "an underivable title must not be fixable, got: {diags:?}"
        );
    }

    #[test]
    fn test_from_date_title_missing() {
        let (schema, type_def) = make_schema("journal", "structure:\n  title: from_date\n");
        let doc = parse("---\ntype: journal\n---\nNo heading.\n");
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingH1 { .. })),
            "got: {diags:?}"
        );
    }

    // ── date_headings validation ──────────────────────────────────────────────

    #[test]
    fn test_date_headings_valid() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        let doc = parse(
            "---\ntype: journal\n---\n# February 2026\n\n## 2026-02-23\n\n- Entry.\n\n## 2026-02-20\n\n- Older.\n",
        );
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        let date_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::InvalidDateHeading { .. }
                        | Diagnostic::DateHeadingFileMismatch { .. }
                        | Diagnostic::EntriesOutOfOrder { .. }
                )
            })
            .collect();
        assert!(date_diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_date_headings_invalid_heading() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        let doc = parse("---\ntype: journal\n---\n# February 2026\n\n## Not a date\n\n- Entry.\n");
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::InvalidDateHeading { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_date_headings_file_mismatch() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        // Entry date is 2026-01 but file is 2026-02
        let doc = parse("---\ntype: journal\n---\n# February 2026\n\n## 2026-01-15\n\n- Entry.\n");
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::DateHeadingFileMismatch { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_date_headings_period_of_a_day_precision_file() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("log", yaml);
        // A day-precision file still covers the month its headings name.
        let doc = parse("---\ntype: log\n---\n# April 14, 2026\n\n## 2026-04-14\n\n- Entry.\n");
        let path = Path::new("2026-04-14.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("log", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::DateHeadingFileMismatch { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_date_headings_out_of_order() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        // Older entry (02-20) appears before newer entry (02-23) -- wrong for newest_first
        let doc = parse(
            "---\ntype: journal\n---\n# February 2026\n\n## 2026-02-20\n\n- Older.\n\n## 2026-02-23\n\n- Newer.\n",
        );
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::EntriesOutOfOrder { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_date_headings_with_time() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        // Both same day, time sub-sort: 21:14 then 09:00 is newest first
        let doc = parse(
            "---\ntype: journal\n---\n# February 2026\n\n## 2026-02-23 21:14\n\n- Later.\n\n## 2026-02-23 09:00\n\n- Earlier.\n",
        );
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        let order_diags: Vec<_> = diags
            .iter()
            .filter(|d| matches!(d, Diagnostic::EntriesOutOfOrder { .. }))
            .collect();
        assert!(order_diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_date_headings_with_suffix() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        // Date-only headings with a suffix label
        let doc = parse(
            "---\ntype: journal\n---\n# February 2026\n\n## 2026-02-23 - standup\n\n- Notes.\n\n## 2026-02-22 - retro\n\n- More notes.\n",
        );
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        let date_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::InvalidDateHeading { .. } | Diagnostic::EntriesOutOfOrder { .. }
                )
            })
            .collect();
        assert!(date_diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_date_headings_with_time_and_suffix() {
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        // Date + time headings with a suffix label; sort order should still work
        let doc = parse(
            "---\ntype: journal\n---\n# February 2026\n\n## 2026-02-23 21:14 - evening\n\n- Later.\n\n## 2026-02-23 09:00 - morning standup\n\n- Earlier.\n",
        );
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        let date_diags: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::InvalidDateHeading { .. } | Diagnostic::EntriesOutOfOrder { .. }
                )
            })
            .collect();
        assert!(date_diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_date_headings_invalid_suffix_no_separator() {
        // A suffix without the " - " separator must be rejected
        let yaml = "structure:\n  title: from_date\n  date_headings:\n    sort: newest_first\n";
        let (schema, type_def) = make_schema("journal", yaml);
        let doc = parse(
            "---\ntype: journal\n---\n# February 2026\n\n## 2026-02-23 morning\n\n- Notes.\n",
        );
        let path = Path::new("2026-02.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("journal", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::InvalidDateHeading { .. })),
            "expected InvalidDateHeading, got: {diags:?}"
        );
    }

    // ── detect_malformed_links ────────────────────────────────────────────────

    #[test]
    fn test_malformed_link_space_in_url() {
        let content = "# Title\n\nSee [my link](file with space.md) here.\n";
        let diags = detect_malformed_links(content);
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        assert!(
            matches!(&diags[0], Diagnostic::MalformedLink { line: 3, url } if url == "file with space.md"),
            "got: {:?}",
            diags[0]
        );
    }

    #[test]
    fn test_malformed_link_tab_in_url() {
        let content = "# Title\n\nSee [my link](file\twith\ttab.md) here.\n";
        let diags = detect_malformed_links(content);
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        assert!(matches!(&diags[0], Diagnostic::MalformedLink { .. }));
    }

    #[test]
    fn test_malformed_link_valid_ignored() {
        let content = "# Title\n\nSee [my link](valid-file.md) here.\n";
        let diags = detect_malformed_links(content);
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_malformed_link_skips_frontmatter() {
        // The malformed link is on line 6 (after 3-line frontmatter + blank + title)
        let content = "---\ncreated: 2024-01-01\n---\n# Title\n\nSee [link](has space.md) here.\n";
        let diags = detect_malformed_links(content);
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        assert!(
            matches!(&diags[0], Diagnostic::MalformedLink { line: 6, .. }),
            "got: {:?}",
            diags[0]
        );
    }

    #[test]
    fn test_malformed_link_scans_a_document_opening_on_a_thematic_break() {
        // An unterminated `---` is a thematic break, not an open frontmatter
        // block — the scan used to swallow the whole document behind it.
        let content = "---\n\n# Title\n\nSee [link](has space.md) here.\n";
        let diags = detect_malformed_links(content);
        assert_eq!(diags.len(), 1, "got: {diags:?}");
        assert!(
            matches!(&diags[0], Diagnostic::MalformedLink { line: 5, .. }),
            "got: {:?}",
            diags[0]
        );
    }

    #[test]
    fn test_malformed_link_skips_fenced_code_block() {
        let content = "# Title\n\n```\n[not a link](has spaces.md)\n```\n\nText.\n";
        let diags = detect_malformed_links(content);
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_malformed_link_skips_inline_code() {
        let content = "# Title\n\nSee `[not a link](has spaces.md)` here.\n";
        let diags = detect_malformed_links(content);
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_malformed_link_multiple_on_one_line() {
        let content = "# Title\n\n[a](x y.md) and [b](p q.md).\n";
        let diags = detect_malformed_links(content);
        assert_eq!(diags.len(), 2, "got: {diags:?}");
    }

    // ── EmptyOptionalSection detection ────────────────────────────────────────

    #[test]
    fn test_empty_optional_section_detected() {
        // "Notes" is optional (required: false); it has no content
        let yaml = "structure:\n  sections:\n    - title: Notes\n      required: false\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\n## Notes\n");
        let path = Path::new("test.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::EmptyOptionalSection { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_non_empty_optional_section_not_flagged() {
        let yaml = "structure:\n  sections:\n    - title: Notes\n      required: false\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\n## Notes\n\nSome content here.\n");
        let path = Path::new("test.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::EmptyOptionalSection { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_required_empty_section_not_flagged_for_removal() {
        // "Goals" is required: true — empty but must not be auto-removed
        let yaml = "structure:\n  sections:\n    - title: Goals\n      required: true\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\n## Goals\n");
        let path = Path::new("test.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::EmptyOptionalSection { .. })),
            "got: {diags:?}"
        );
    }

    // ── URL encoding / decoding ───────────────────────────────────────────────

    #[test]
    fn test_percent_decode_space() {
        // %20 in a link URL must decode to a space when resolving path
        let path = resolve_link_path("other%20doc.md", Path::new("/project/src/file.md"));
        assert_eq!(
            path,
            Some(PathBuf::from("/project/src/other doc.md")),
            "percent-decode failed"
        );
    }

    #[test]
    fn test_percent_decode_percent_literal() {
        // %25 must decode to a literal %
        let path = resolve_link_path("100%25.md", Path::new("/project/file.md"));
        assert_eq!(path, Some(PathBuf::from("/project/100%.md")));
    }

    #[test]
    fn test_unencoded_space_not_a_link() {
        // A URL with a literal space won't be parsed as a link by the parser;
        // validate_all_links only sees already-parsed links, so this is a
        // no-op here — the malformed link scanner handles it.  Confirm
        // resolve_link_path does NOT produce a path with a literal space from
        // a properly percent-encoded URL.
        let decoded = resolve_link_path("file%20name.md", Path::new("/root/doc.md"));
        assert_eq!(decoded, Some(PathBuf::from("/root/file name.md")));
    }

    #[test]
    fn test_broken_link_reported() {
        // A link to a file not in linked_docs and no git_tree → UnknownType diagnostic
        let yaml = "description: doc\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\nSee [this](missing.md).\n");
        let path = Path::new("/project/doc.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", path, &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::UnknownType { message, .. }
                if message.contains("broken link"))),
            "expected broken link diagnostic, got: {diags:?}"
        );
    }

    /// Build a `GitTree` from paths relative to `/repo`, deriving the
    /// directory set the way a HEAD walk would.
    fn make_git_tree(files: &[&str]) -> crate::git::GitTree {
        let root = PathBuf::from("/repo");
        let mut tree = crate::git::GitTree::default();
        tree.dirs.insert(root.clone());
        for rel in files {
            let path = root.join(rel);
            let mut dir = path.parent();
            while let Some(d) = dir {
                if !d.starts_with(&root) {
                    break;
                }
                tree.dirs.insert(d.to_path_buf());
                dir = d.parent();
            }
            tree.files.insert(path);
        }
        tree
    }

    /// Validate a document in `projects/here` that links out to a document in
    /// the sibling tree `projects/there`.
    fn outbound_link_diags(git_tree: &crate::git::GitTree) -> Vec<Diagnostic> {
        let (schema, type_def) = make_schema("doc", "description: doc\n");
        let doc = parse("---\ntype: doc\n---\n# Title\n\nSee [A](../../there/notes/A.md).\n");
        let source_path = Path::new("/repo/projects/here/entries/doc.md");
        let schema_root = PathBuf::from("/repo/projects/here");
        let ctx = ValidateCtx {
            source_path,
            schema_root: Some(&schema_root),
            source_type: "doc",
            schema: &schema,
            linked_docs: empty_linked_docs(),
            git_tree: Some(git_tree),
            external_types: empty_external_types(),
        };
        validate(&doc, &type_def, &ctx, None)
    }

    fn has_broken_link(diags: &[Diagnostic]) -> bool {
        diags.iter().any(|d| {
            matches!(d, Diagnostic::UnknownType { message, .. }
            if message.contains("broken link"))
        })
    }

    #[test]
    fn test_outbound_link_skipped_when_foreign_tree_absent() {
        // Partial checkout: HEAD holds only the linking project, so nothing
        // can be said about a link into the sibling tree.
        let tree = make_git_tree(&["projects/here/entries/doc.md"]);
        let diags = outbound_link_diags(&tree);
        assert!(
            !has_broken_link(&diags),
            "link into an absent tree is unverifiable, not broken: {diags:?}"
        );
    }

    #[test]
    fn test_outbound_link_reported_when_foreign_tree_present() {
        // The sibling tree is in HEAD and the target file is not, so the
        // link really is broken.
        let tree = make_git_tree(&["projects/here/entries/doc.md", "projects/there/notes/B.md"]);
        let diags = outbound_link_diags(&tree);
        assert!(
            has_broken_link(&diags),
            "link into a present tree must still be checked: {diags:?}"
        );
    }

    #[test]
    fn test_outbound_link_resolves_when_target_tracked() {
        let tree = make_git_tree(&["projects/here/entries/doc.md", "projects/there/notes/A.md"]);
        let diags = outbound_link_diags(&tree);
        assert!(
            !has_broken_link(&diags),
            "tracked outbound target should resolve: {diags:?}"
        );
    }

    #[test]
    fn test_foreign_root_names_the_neighbouring_tree() {
        let schema_root = PathBuf::from("/repo/projects/here");
        assert_eq!(
            foreign_root(
                Path::new("/repo/projects/there/notes/A.md"),
                Some(&schema_root)
            ),
            Some(PathBuf::from("/repo/projects/there"))
        );
    }

    #[test]
    fn test_foreign_root_none_for_links_inside_the_project() {
        let schema_root = PathBuf::from("/repo/projects/here");
        assert_eq!(
            foreign_root(
                Path::new("/repo/projects/here/entries/X.md"),
                Some(&schema_root)
            ),
            None
        );
    }

    #[test]
    fn test_foreign_root_falls_back_to_shared_ancestor_for_sibling_file() {
        // `../../README.md` sits in the shared ancestor, which is always on
        // the path to this project — nothing to excuse a missing file.
        let schema_root = PathBuf::from("/repo/projects/here");
        assert_eq!(
            foreign_root(Path::new("/repo/README.md"), Some(&schema_root)),
            Some(PathBuf::from("/repo"))
        );
    }

    #[test]
    fn test_percent_encoded_link_resolves_to_known_doc() {
        // A %20-encoded link should resolve and match a pre-loaded LinkedDocInfo
        let yaml = "description: doc\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\nSee [this](other%20doc.md).\n");
        let source_path = Path::new("/project/doc.md");
        let linked = LinkedDocInfo {
            path: PathBuf::from("/project/other doc.md"),
            doc_type: None,
            section_links: Default::default(),
        };
        let linked_docs = HashMap::from([(linked.path.clone(), linked)]);
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", source_path, &schema, &linked_docs),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::UnknownType { message, .. }
                if message.contains("broken link"))),
            "percent-encoded link should resolve, got: {diags:?}"
        );
    }

    #[test]
    fn test_validate_all_internal_links_across_blocks() {
        let (schema, type_def) = make_schema("doc", "description: doc\n");
        let doc = parse(
            "---\ntype: doc\n---\n# [Head](missing-head.md)\n\n> [Quote](missing-quote.md)\n\n| Col |\n| --- |\n| [Cell](missing-table.md) |\n\n![Alt](missing-image.png)\n",
        );
        let path = Path::new("/project/doc.md");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", path, &schema, empty_linked_docs()),
            None,
        );

        for url in [
            "missing-head.md",
            "missing-quote.md",
            "missing-table.md",
            "missing-image.png",
        ] {
            assert!(
                diags.iter().any(
                    |d| matches!(d, Diagnostic::UnknownType { message, .. } if message.contains(url))
                ),
                "expected broken link diagnostic for {url}, got: {diags:?}"
            );
        }
    }

    #[test]
    fn test_resolve_link_path_strips_query_and_fragment() {
        let source = Path::new("/project/doc.md");
        let resolved = resolve_link_path("guide.md?view=full#section-1", source);
        assert_eq!(resolved, Some(PathBuf::from("/project/guide.md")));
    }

    // ── List field validation ─────────────────────────────────────────────────

    /// `tags` is an array of a two-value enum; `counts` an array of integers.
    const V2_LISTS: &str = r#"
version: 2
frontmatter:
  type: object
  properties:
    tags:
      type: array
      items:
        type: string
        enum: [a, b]
    counts:
      type: array
      items:
        type: integer
"#;

    fn list_diags(doc_src: &str) -> Vec<Diagnostic> {
        let (schema, type_def) = make_schema("doc", V2_LISTS);
        let doc = parse(doc_src);
        validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        )
    }

    fn list_field_errors<'a>(diags: &'a [Diagnostic], field: &str) -> Vec<&'a Diagnostic> {
        diags
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::InvalidFieldType { field: f, .. } if f.contains(field)),
            )
            .collect()
    }

    #[test]
    fn test_list_of_strings_valid() {
        let diags = list_diags("---\ntype: doc\ntags:\n  - a\n  - b\n---\n# Title\n");
        assert!(
            list_field_errors(&diags, "tags").is_empty(),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_list_of_integers_valid() {
        let diags = list_diags("---\ntype: doc\ncounts:\n  - 1\n  - 42\n---\n# Title\n");
        assert!(
            list_field_errors(&diags, "counts").is_empty(),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_list_of_integers_invalid_item() {
        let diags = list_diags("---\ntype: doc\ncounts:\n  - 1\n  - not-a-number\n---\n# Title\n");
        assert!(
            list_field_errors(&diags, "counts[1]").len() == 1,
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_list_empty_is_valid() {
        let diags = list_diags("---\ntype: doc\ntags: []\n---\n# Title\n");
        assert!(
            list_field_errors(&diags, "tags").is_empty(),
            "empty list should be valid, got: {diags:?}"
        );
    }

    #[test]
    fn test_list_multiple_invalid_items_each_reported() {
        let diags = list_diags(
            "---\ntype: doc\ntags:\n  - a\n  - bad1\n  - bad2\n  - bad3\n---\n# Title\n",
        );
        assert_eq!(
            list_field_errors(&diags, "tags").len(),
            3,
            "expected one error per bad item, got: {diags:?}"
        );
    }

    // ── Bidirectional link validation ─────────────────────────────────────────

    #[test]
    fn test_bidirectional_link_passes_when_backlink_exists() {
        // doc A (type "project") links to doc B (type "person") in "Team" section.
        // doc B has a "Projects" section that links back to A.
        // Both schemas declare bidirectional: true — should produce no MissingBacklink.
        let project_yaml = "structure:\n  sections:\n    - title: Team\n      links:\n        target_type: person\n        bidirectional: true\n";
        let person_yaml = "structure:\n  sections:\n    - title: Projects\n      links:\n        target_type: project\n        bidirectional: true\n";

        let mut schema = Schema::default();
        let project_def: TypeDef = serde_yaml::from_str(project_yaml).unwrap();
        let person_def: TypeDef = serde_yaml::from_str(person_yaml).unwrap();
        schema
            .types
            .insert("project".to_string(), project_def.clone());
        schema.types.insert("person".to_string(), person_def);

        let project_path = Path::new("/proj/project.md");
        let person_path = PathBuf::from("/proj/person.md");

        // B links back to A in its "Projects" section
        let mut section_links = std::collections::HashMap::new();
        section_links.insert("Projects".to_string(), vec!["project.md".to_string()]);
        let linked_person = LinkedDocInfo {
            path: person_path.clone(),
            doc_type: Some("person".to_string()),
            section_links,
        };
        let linked_docs = HashMap::from([(linked_person.path.clone(), linked_person)]);

        let doc =
            parse("---\ntype: project\n---\n# My Project\n\n## Team\n\n- [Alice](person.md)\n");
        let ctx = ValidateCtx {
            source_path: project_path,
            schema_root: None,
            source_type: "project",
            schema: &schema,
            linked_docs: &linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        };
        let diags = validate(&doc, &project_def, &ctx, None);
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingBacklink { .. })),
            "expected no MissingBacklink, got: {diags:?}"
        );
    }

    #[test]
    fn test_bidirectional_link_fails_when_backlink_missing() {
        // Same setup but B has no "Projects" section links → MissingBacklink
        let project_yaml = "structure:\n  sections:\n    - title: Team\n      links:\n        target_type: person\n        bidirectional: true\n";
        let person_yaml = "structure:\n  sections:\n    - title: Projects\n      links:\n        target_type: project\n        bidirectional: true\n";

        let mut schema = Schema::default();
        let project_def: TypeDef = serde_yaml::from_str(project_yaml).unwrap();
        let person_def: TypeDef = serde_yaml::from_str(person_yaml).unwrap();
        schema
            .types
            .insert("project".to_string(), project_def.clone());
        schema.types.insert("person".to_string(), person_def);

        let project_path = Path::new("/proj/project.md");
        let person_path = PathBuf::from("/proj/person.md");

        // B exists but has no backlink in "Projects"
        let linked_person = LinkedDocInfo {
            path: person_path,
            doc_type: Some("person".to_string()),
            section_links: Default::default(), // empty — no backlink
        };
        let linked_docs = HashMap::from([(linked_person.path.clone(), linked_person)]);

        let doc =
            parse("---\ntype: project\n---\n# My Project\n\n## Team\n\n- [Alice](person.md)\n");
        let ctx = ValidateCtx {
            source_path: project_path,
            schema_root: None,
            source_type: "project",
            schema: &schema,
            linked_docs: &linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        };
        let diags = validate(&doc, &project_def, &ctx, None);
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingBacklink { .. })),
            "expected MissingBacklink, got: {diags:?}"
        );
    }

    #[test]
    fn test_bidirectional_link_target_has_no_type_field() {
        // B is a known linked doc but has no type — bidirectional check can't run
        // (no target_type_def in schema), should emit UnknownType not panic
        let project_yaml = "structure:\n  sections:\n    - title: Team\n      links:\n        target_type: person\n        bidirectional: true\n";

        let mut schema = Schema::default();
        let project_def: TypeDef = serde_yaml::from_str(project_yaml).unwrap();
        schema
            .types
            .insert("project".to_string(), project_def.clone());
        // Note: "person" type is NOT in the schema

        let project_path = Path::new("/proj/project.md");
        let linked_person = LinkedDocInfo {
            path: PathBuf::from("/proj/person.md"),
            doc_type: Some("person".to_string()), // type field present but not in schema
            section_links: Default::default(),
        };
        let linked_docs = HashMap::from([(linked_person.path.clone(), linked_person)]);

        let doc =
            parse("---\ntype: project\n---\n# My Project\n\n## Team\n\n- [Alice](person.md)\n");
        let ctx = ValidateCtx {
            source_path: project_path,
            schema_root: None,
            source_type: "project",
            schema: &schema,
            linked_docs: &linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        };
        let diags = validate(&doc, &project_def, &ctx, None);
        // Should not panic; should emit some diagnostic
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::UnknownType { .. })),
            "expected UnknownType for missing schema entry, got: {diags:?}"
        );
    }

    #[test]
    fn test_same_type_cross_section_bidirectional() {
        // threat A "Leads To" threat B; threat B "Enabled By" threat A
        let threat_yaml = "structure:\n  sections:\n    - title: Leads To\n      links:\n        target_type: threat\n        bidirectional: true\n    - title: Enabled By\n      links:\n        target_type: threat\n        bidirectional: true\n";

        let mut schema = Schema::default();
        let threat_def: TypeDef = serde_yaml::from_str(threat_yaml).unwrap();
        schema
            .types
            .insert("threat".to_string(), threat_def.clone());

        let a_path = Path::new("/proj/threat-a.md");
        let b_path = PathBuf::from("/proj/threat-b.md");

        // B has A in "Enabled By"
        let mut b_links = std::collections::HashMap::new();
        b_links.insert("Enabled By".to_string(), vec!["threat-a.md".to_string()]);
        let linked_b = LinkedDocInfo {
            path: b_path,
            doc_type: Some("threat".to_string()),
            section_links: b_links,
        };
        let linked_docs = HashMap::from([(linked_b.path.clone(), linked_b)]);

        let doc = parse(
            "---\ntype: threat\n---\n# Threat A\n\n## Leads To\n\n- [Threat B](threat-b.md)\n",
        );
        let ctx = ValidateCtx {
            source_path: a_path,
            schema_root: None,
            source_type: "threat",
            schema: &schema,
            linked_docs: &linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        };
        let diags = validate(&doc, &threat_def, &ctx, None);
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingBacklink { .. })),
            "expected no MissingBacklink for cross-section same-type bidir, got: {diags:?}"
        );
    }

    // ── Managed section edge cases ────────────────────────────────────────────

    /// Validate `doc_md` against `type_yaml`, apply every fix, and return the
    /// re-serialized markdown — i.e. exactly what `td fmt` would leave on disk.
    fn managed_fmt(type_yaml: &str, doc_md: &str) -> String {
        let (schema, type_def) = make_schema("doc", type_yaml);
        let mut doc = parse(doc_md);
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        crate::fix::apply_fixes(&mut doc, &diags);
        crate::parse::serialize(&doc)
    }

    /// The agents preset's `Related Documents` template, two canonical bullets.
    const RELATED_DOCS_SCHEMA: &str = "structure:\n  title: none\n  strict_sections: false\n  sections:\n    - title: Related Documents\n      managed_content:\n        template: |\n          ## Related Documents\n\n          - **README.md** — what this project is and why.\n          - **GOALS.md** — goals and non-goals. Consult during planning.\n";

    #[test]
    fn test_managed_section_upsert_keeps_curated_entries() {
        // Regression: `td fmt` matched the template against the section
        // positionally, so a curated Related Documents list whose entries sat in
        // a different order lost its first N bullets — an instruction file's
        // curated list lost its `**BUGS.md**` entry this way. The template's
        // own entries must be normalised in place and everything else kept.
        let out = managed_fmt(
            RELATED_DOCS_SCHEMA,
            "---\ntype: doc\n---\n## Related Documents\n\n- **GOALS.md** — Committed goals and explicit non-goals.\n- **BUGS.md** — Known defects, one per bullet.\n- **collections.md** — Account-wide collection counts. Never hand-edit.\n- **README.md** — What this project is and how it's laid out.\n",
        );

        assert!(
            out.contains("- **BUGS.md** — Known defects, one per bullet."),
            "curated entry must survive, got: {out}"
        );
        assert!(
            out.contains("- **collections.md** — Account-wide collection counts. Never hand-edit."),
            "curated entry must survive, got: {out}"
        );
        // The templated entries are normalised to the template's wording…
        assert!(
            out.contains("- **GOALS.md** — goals and non-goals. Consult during planning."),
            "templated entry must be normalised, got: {out}"
        );
        assert!(
            !out.contains("Committed goals and explicit non-goals"),
            "stale wording of a templated entry must be replaced, got: {out}"
        );
        // …and land ahead of the curated ones, in template order.
        let readme = out.find("**README.md**").expect("README entry");
        let goals = out.find("**GOALS.md**").expect("GOALS entry");
        let bugs = out.find("**BUGS.md**").expect("BUGS entry");
        assert!(
            readme < goals && goals < bugs,
            "templated entries come first, in template order, got: {out}"
        );
    }

    #[test]
    fn test_managed_section_upsert_is_idempotent() {
        // Once upserted, a second pass must be a no-op — otherwise `td fmt`
        // churns the file on every run.
        let once = managed_fmt(
            RELATED_DOCS_SCHEMA,
            "---\ntype: doc\n---\n## Related Documents\n\n- **characters/** — One doc per character.\n",
        );
        let twice = managed_fmt(RELATED_DOCS_SCHEMA, &once);
        assert_eq!(once, twice, "second fmt pass should change nothing");
    }

    #[test]
    fn test_managed_section_already_canonical_is_untouched() {
        // A list that already leads with the template's entries, in the
        // template's wording, must come back byte-for-byte — ten CLAUDE.md
        // files were hand-normalized to exactly this shape.
        let input = "---\ntype: doc\n---\n## Related Documents\n\n- **README.md** — what this project is and why.\n- **GOALS.md** — goals and non-goals. Consult during planning.\n- **characters/** — One doc per character.\n";
        assert_eq!(managed_fmt(RELATED_DOCS_SCHEMA, input), input);
    }

    #[test]
    fn test_managed_section_upsert_keeps_prose_and_extra_blocks() {
        // Content the template says nothing about — a paragraph, a second list —
        // is appended after the managed blocks rather than dropped.
        let out = managed_fmt(
            RELATED_DOCS_SCHEMA,
            "---\ntype: doc\n---\n## Related Documents\n\nRead these in order.\n\n- **README.md** — what this project is and why.\n- **GOALS.md** — goals and non-goals. Consult during planning.\n",
        );
        assert!(
            out.contains("Read these in order."),
            "authored prose must survive, got: {out}"
        );
    }

    #[test]
    fn test_managed_section_replace_discards_curated_entries() {
        // `merge: replace` is the opt-in clobber: the section becomes the
        // template and nothing else.
        let yaml = format!("{RELATED_DOCS_SCHEMA}        merge: replace\n");
        let out = managed_fmt(
            &yaml,
            "---\ntype: doc\n---\n## Related Documents\n\n- **README.md** — what this project is and why.\n- **GOALS.md** — goals and non-goals. Consult during planning.\n- **characters/** — One doc per character.\n",
        );
        assert!(
            !out.contains("**characters/**"),
            "merge: replace should discard non-templated entries, got: {out}"
        );
        assert!(out.contains("**README.md**"), "got: {out}");
    }

    #[test]
    fn test_managed_section_migrate_from_keeps_legacy_entries() {
        // Renaming a legacy section must carry its curated entries across, not
        // swap them for the template.
        let yaml = format!("{RELATED_DOCS_SCHEMA}        migrate_from:\n          - Journal\n");
        let out = managed_fmt(
            &yaml,
            "---\ntype: doc\n---\n## Journal\n\n- **characters/** — One doc per character.\n",
        );
        assert!(
            out.contains("## Related Documents"),
            "legacy heading should be renamed, got: {out}"
        );
        assert!(
            !out.contains("## Journal"),
            "legacy heading should be gone, got: {out}"
        );
        assert!(
            out.contains("- **characters/** — One doc per character."),
            "legacy entry must survive the rename, got: {out}"
        );
    }

    #[test]
    fn test_managed_section_item_key_matches_by_link_url() {
        // Link-shaped templates key off the URL, so a re-worded link text is
        // rewritten rather than duplicated.
        let yaml = "structure:\n  title: none\n  strict_sections: false\n  sections:\n    - title: Related\n      managed_content:\n        template: |\n          ## Related\n\n          - [doc A](a.md) — canonical blurb\n";
        let out = managed_fmt(
            yaml,
            "---\ntype: doc\n---\n## Related\n\n- [Doc A](a.md) — stale blurb\n- [custom](custom.md) — mine\n",
        );
        assert_eq!(
            out.matches("(a.md)").count(),
            1,
            "the same entry must not be duplicated, got: {out}"
        );
        assert!(out.contains("canonical blurb"), "got: {out}");
        assert!(
            out.contains("- [custom](custom.md) — mine"),
            "unknown entry must survive, got: {out}"
        );
    }

    #[test]
    fn test_managed_section_migrate_from() {
        // A document has a legacy "## Old Name" section — validate_managed_section
        // should detect it needs migration and emit ManagedSectionNeedsUpdate.
        let yaml = "structure:\n  sections:\n    - title: New Name\n      managed_content:\n        template: |\n          ## New Name\n\n          - placeholder\n        migrate_from:\n          - Old Name\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\n## Old Name\n\n- some content\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::ManagedSectionNeedsUpdate { .. })),
            "expected ManagedSectionNeedsUpdate for migrate_from, got: {diags:?}"
        );
    }

    #[test]
    fn test_managed_section_preserves_custom_content_after_template() {
        // After applying a managed section fix, user content beyond the template
        // should be preserved in custom_content and re-inserted after the template.
        //
        // Schema: the "Related" section has a managed_content template with one list item.
        // Document: the section has the template item PLUS a user-added item.
        // Expected: if the validator emits ManagedSectionNeedsUpdate, it must carry
        // the extra user item in custom_content so it isn't lost on re-render.
        let yaml = "structure:\n  sections:\n    - title: Related\n      managed_content:\n        template: |\n          ## Related\n\n          - [doc A](a.md)\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse(
            "---\ntype: doc\n---\n# Title\n\n## Related\n\n- [doc A](a.md)\n- [custom](custom.md)\n",
        );
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        // If the validator emits an update diagnostic, custom_content must be non-empty
        // so the user's extra item is preserved across the re-render.
        let update_diags: Vec<_> = diags
            .iter()
            .filter(|d| matches!(d, Diagnostic::ManagedSectionNeedsUpdate { .. }))
            .collect();
        if !update_diags.is_empty() {
            if let Diagnostic::ManagedSectionNeedsUpdate { custom_content, .. } = &update_diags[0] {
                assert!(
                    !custom_content.is_empty(),
                    "custom content should be preserved in the diagnostic"
                );
            }
        }
    }

    #[test]
    fn test_managed_section_detects_content_drift() {
        // Changing a word inside a managed section's body must trigger
        // ManagedSectionNeedsUpdate even when the block count is unchanged.
        // Regression test: the old implementation only compared block count,
        // so single-word changes were silently accepted.
        let yaml = "structure:\n  sections:\n    - title: How It Works\n      managed_content:\n        template: |\n          ## How It Works\n\n          These are the canonical instructions.\n";
        let (schema, type_def) = make_schema("doc", yaml);

        // Exact match — no diagnostic expected.
        let doc_clean = parse(
            "---\ntype: doc\n---\n# Title\n\n## How It Works\n\nThese are the canonical instructions.\n",
        );
        let diags_clean = validate(
            &doc_clean,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags_clean
                .iter()
                .any(|d| matches!(d, Diagnostic::ManagedSectionNeedsUpdate { .. })),
            "clean document should not trigger ManagedSectionNeedsUpdate"
        );

        // One word changed — drift must be detected.
        let doc_drifted = parse(
            "---\ntype: doc\n---\n# Title\n\n## How It Works\n\nThese are the updated instructions.\n",
        );
        let diags_drifted = validate(
            &doc_drifted,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags_drifted
                .iter()
                .any(|d| matches!(d, Diagnostic::ManagedSectionNeedsUpdate { .. })),
            "word change in managed section body should trigger ManagedSectionNeedsUpdate"
        );
    }

    #[test]
    fn test_percent_decode_ascii() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("no-encoding"), "no-encoding");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
    }

    #[test]
    fn test_percent_decode_multibyte() {
        // Em dash U+2014 encodes as %E2%80%94 in UTF-8.
        assert_eq!(percent_decode("%E2%80%94"), "\u{2014}");
        // Curly left double-quote U+201C encodes as %E2%80%9C.
        assert_eq!(percent_decode("%E2%80%9C"), "\u{201C}");
        // Mixed: ASCII + em dash + ASCII.
        assert_eq!(percent_decode("a%E2%80%94b"), "a\u{2014}b");
    }

    // ── Property validation tests ─────────────────────────────────────────────

    fn validate_props(doc_md: &str, props_yaml: &str) -> Vec<Diagnostic> {
        let type_yaml = format!(
            "structure:\n  sections:\n    - title: Media\n      bullets: unordered\n      properties:\n{props_yaml}"
        );
        let (schema, type_def) = make_schema("item", &type_yaml);
        // Wrap in frontmatter so validate() doesn't short-circuit with MissingFrontmatter
        let doc = parse(&format!("---\ntype: item\n---\n{doc_md}"));
        let ctx = make_ctx("item", empty_path(), &schema, empty_linked_docs());
        validate(&doc, &type_def, &ctx, None)
    }

    #[test]
    fn test_properties_valid_passes() {
        // Schema uses lowercase keys; document may use any case
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Bluray\n  - Size: 42\n  - Audio: English\n",
            "        size:\n          type: integer\n          required: true\n        audio:\n          type: string\n          required: true\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_properties_missing_required() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Bluray\n  - Audio: English\n",
            "        size:\n          type: integer\n          required: true\n        audio:\n          type: string\n",
        );
        assert!(
            diags.iter().any(|d| matches!(d,
                Diagnostic::MissingRequiredField { field, .. } if field.contains("size")
            )),
            "expected missing size, got: {diags:?}"
        );
    }

    #[test]
    fn test_properties_invalid_integer() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Bluray\n  - Size: not-a-number\n",
            "        size:\n          type: integer\n          required: true\n",
        );
        assert!(
            diags.iter().any(|d| matches!(d,
                Diagnostic::InvalidFieldType { field, .. } if field.contains("size")
            )),
            "expected invalid integer for size, got: {diags:?}"
        );
    }

    #[test]
    fn test_properties_invalid_enum() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Bluray\n  - Format: webm\n",
            "        format:\n          type: enum\n          required: true\n          values: [bluray, remux, web]\n",
        );
        assert!(
            diags.iter().any(|d| matches!(d,
                Diagnostic::InvalidFieldType { field, .. } if field.contains("format")
            )),
            "expected invalid enum for format, got: {diags:?}"
        );
    }

    #[test]
    fn test_properties_optional_absent_ok() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Bluray\n  - Size: 42\n",
            "        size:\n          type: integer\n          required: true\n        subtitles:\n          type: string\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_properties_invalid_date() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Bluray\n  - Released: yesterday\n",
            "        released:\n          type: date\n          required: true\n",
        );
        assert!(
            diags.iter().any(|d| matches!(d,
                Diagnostic::InvalidFieldType { field, .. } if field.contains("released")
            )),
            "expected invalid date for released, got: {diags:?}"
        );
    }

    #[test]
    fn test_properties_valid_date() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Bluray\n  - Released: 2001-07-20\n",
            "        released:\n          type: date\n          required: true\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_flat_properties_valid_passes() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Size: 42\n- Audio: English\n",
            "        size:\n          type: integer\n          required: true\n        audio:\n          type: string\n          required: true\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_flat_properties_missing_required() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Audio: English\n",
            "        size:\n          type: integer\n          required: true\n        audio:\n          type: string\n",
        );
        assert!(
            diags.iter().any(|d| matches!(d,
                Diagnostic::MissingRequiredField { field, .. } if field.contains("size")
            )),
            "expected missing size, got: {diags:?}"
        );
    }

    #[test]
    fn test_flat_properties_invalid_integer() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Size: not-a-number\n- Audio: English\n",
            "        size:\n          type: integer\n          required: true\n        audio:\n          type: string\n          required: true\n",
        );
        assert!(
            diags.iter().any(|d| matches!(d,
                Diagnostic::InvalidFieldType { field, .. } if field.contains("size")
            )),
            "expected invalid integer for size, got: {diags:?}"
        );
    }

    #[test]
    fn test_flat_properties_optional_absent_ok() {
        let diags = validate_props(
            "# Title\n\n## Media\n\n- Size: 42\n",
            "        size:\n          type: integer\n          required: true\n        subtitles:\n          type: string\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    // ── Correspondence rules ──────────────────────────────────────────────────

    /// A `version: 2` movie type: `files[]` in frontmatter, `## Files` in prose,
    /// and correspondence rules tying them together in both directions.
    const MOVIE_CORRESPONDENCE_YAML: &str = "\
version: 2
frontmatter:
  type: object
  properties:
    files:
      type: array
      items:
        type: object
        properties:
          quality: { type: string }
structure:
  strict_sections: false
  sections:
    - title: Files
correspondence:
  - each: frontmatter.files[]
    requires:
      subsection-under: \"## Files\"
      heading: \"### {quality}\"
  - each: subsections-under \"## Files\"
    requires:
      frontmatter-item: files[].quality
";

    fn validate_movie(md: &str) -> Vec<Diagnostic> {
        let type_def: TypeDef = serde_yaml::from_str(MOVIE_CORRESPONDENCE_YAML).unwrap();
        type_def.validate("movie").expect("schema is valid");
        let mut schema = Schema::default();
        schema.types.insert("movie".to_string(), type_def.clone());
        let doc = parse(md);
        validate(
            &doc,
            &type_def,
            &make_ctx("movie", empty_path(), &schema, empty_linked_docs()),
            None,
        )
    }

    #[test]
    fn test_correspondence_matching_item_and_subsection_passes() {
        let diags = validate_movie(
            "---\ntype: movie\nfiles:\n  - quality: WEBRip-1080p\n---\n\n\
             # Title\n\n## Files\n\n### WEBRip-1080p\n\nA sentence about the file.\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_correspondence_item_without_subsection() {
        let diags = validate_movie(
            "---\ntype: movie\nfiles:\n  - quality: WEBRip-1080p\n  - quality: 4k\n---\n\n\
             # Title\n\n## Files\n\n### WEBRip-1080p\n\nProse.\n",
        );
        let missing: Vec<_> = diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::MissingSubsection { heading, line, .. } => Some((heading, *line)),
                _ => None,
            })
            .collect();
        // Anchored to the `files:` key on line 3 (`---` is line 1).
        assert_eq!(missing, [(&"### 4k".to_string(), 3)], "got: {diags:?}");
    }

    #[test]
    fn test_correspondence_subsection_without_item() {
        let diags = validate_movie(
            "---\ntype: movie\nfiles:\n  - quality: WEBRip-1080p\n---\n\n\
             # Title\n\n## Files\n\n### WEBRip-1080p\n\nProse.\n\n### 4k\n\nOrphan prose.\n",
        );
        let orphans: Vec<_> = diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::OrphanSubsection {
                    heading, expected, ..
                } => Some((heading.as_str(), expected.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(orphans, [("### 4k", "files[].quality")], "got: {diags:?}");
    }

    #[test]
    fn test_correspondence_orphan_subsection_carries_its_own_line() {
        let diags = validate_movie(
            "---\ntype: movie\n---\n\n# Title\n\n## Files\n\n### 4k\n\nOrphan prose.\n",
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::OrphanSubsection { line: 9, .. })),
            "expected the orphan at line 9, got: {diags:?}"
        );
    }

    #[test]
    fn test_correspondence_subsections_outside_the_container_are_not_counted() {
        // `### 4k` sits under `## Details`, so neither rule sees it.
        let diags = validate_movie(
            "---\ntype: movie\nfiles:\n  - quality: WEBRip-1080p\n---\n\n\
             # Title\n\n## Files\n\n### WEBRip-1080p\n\nProse.\n\n## Details\n\n### 4k\n\nCast.\n",
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_correspondence_empty_array_is_vacuous() {
        let diags =
            validate_movie("---\ntype: movie\nfiles: []\n---\n\n# Title\n\n## Files\n\nNone.\n");
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_correspondence_absent_array_is_vacuous() {
        let diags = validate_movie("---\ntype: movie\n---\n\n# Title\n\n## Files\n\nNone.\n");
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_correspondence_missing_container_reported_once() {
        let diags = validate_movie(
            "---\ntype: movie\nfiles:\n  - quality: a\n  - quality: b\n---\n\n# Title\n",
        );
        let missing_sections: Vec<_> = diags
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::MissingSection { section, .. } if section == "Files"),
            )
            .collect();
        assert_eq!(missing_sections.len(), 1, "got: {diags:?}");
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingSubsection { .. })),
            "the absent container should not also fire per item: {diags:?}"
        );
    }

    #[test]
    fn test_correspondence_item_missing_the_templated_field() {
        let diags =
            validate_movie("---\ntype: movie\nfiles:\n  - size: 12\n---\n\n# Title\n\n## Files\n");
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::InvalidFieldType { field, message, .. }
                    if field == "files[0]" && message.contains("no scalar 'quality'")
            )),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_correspondence_is_never_auto_fixed() {
        for diag in [
            Diagnostic::MissingSubsection {
                line: 3,
                heading: "### 4k".to_string(),
                container: "## Files".to_string(),
            },
            Diagnostic::OrphanSubsection {
                line: 9,
                heading: "### 4k".to_string(),
                expected: "files[].quality".to_string(),
            },
        ] {
            assert!(!crate::fix::Fix::is_fixable(&diag), "{diag:?}");
            assert!(
                crate::fix::Fix::from_diagnostic(&diag).is_none(),
                "{diag:?}"
            );
        }
    }

    // ── Correspondence: links (the v2 home for target_type / bidirectional) ────

    /// Build the two-document fixture the link rules are checked against: a
    /// `movie` linking to a `personality` from `## Cast`, and the personality's
    /// own `## Movies` section holding `backlinks`.
    fn link_fixture(
        movie_yaml: &str,
        movie_md: &str,
        personality_type: Option<&str>,
        backlinks: &[&str],
    ) -> Vec<Diagnostic> {
        let movie_def: TypeDef = serde_yaml::from_str(movie_yaml).unwrap();
        movie_def.validate("movie").expect("schema is valid");
        let mut schema = Schema::default();
        schema.types.insert("movie".to_string(), movie_def.clone());

        let personality_path = PathBuf::from("/proj/Ada.md");
        let mut section_links = std::collections::HashMap::new();
        if !backlinks.is_empty() {
            section_links.insert(
                "Movies".to_string(),
                backlinks.iter().map(|s| s.to_string()).collect(),
            );
        }
        let linked_docs = HashMap::from([(
            personality_path.clone(),
            LinkedDocInfo {
                path: personality_path,
                doc_type: personality_type.map(str::to_string),
                section_links,
            },
        )]);

        let doc = parse(movie_md);
        let ctx = ValidateCtx {
            source_path: Path::new("/proj/Movie.md"),
            schema_root: None,
            source_type: "movie",
            schema: &schema,
            linked_docs: &linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        };
        validate(&doc, &movie_def, &ctx, None)
    }

    const CAST_LINKS_YAML: &str = "\
version: 2
structure:
  strict_sections: false
  sections:
    - title: Cast
correspondence:
  - each: links-in-section \"## Cast\"
    requires:
      target_type: personality
      backlink-in: \"## Movies\"
";

    const CAST_MD: &str =
        "---\ntype: movie\n---\n\n# Movie\n\n## Cast\n\n- [Ada](Ada.md) as herself\n";

    #[test]
    fn test_correspondence_links_pass_with_right_type_and_backlink() {
        let diags = link_fixture(CAST_LINKS_YAML, CAST_MD, Some("personality"), &["Movie.md"]);
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_correspondence_links_report_the_wrong_target_type() {
        let diags = link_fixture(CAST_LINKS_YAML, CAST_MD, Some("movie"), &["Movie.md"]);
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::LinkTargetTypeMismatch { url, expected, actual, line: 9 }
                    if url == "Ada.md" && expected == "personality" && actual.as_deref() == Some("movie")
            )),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_correspondence_links_report_the_missing_backlink() {
        let diags = link_fixture(CAST_LINKS_YAML, CAST_MD, Some("personality"), &[]);
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::MissingBacklink { url, inverse_section, line: 9 }
                    if url == "Ada.md" && inverse_section == "Movies"
            )),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_correspondence_backlink_accepts_any_of_several_sections() {
        // The same-type cross-section case `bidirectional: true` covered by
        // scanning the target schema, spelled out explicitly instead.
        let yaml = "\
version: 2
structure:
  strict_sections: false
correspondence:
  - each: links-in-section \"## Cast\"
    requires:
      backlink-in: [\"## Appearances\", \"## Movies\"]
";
        let diags = link_fixture(yaml, CAST_MD, Some("personality"), &["Movie.md"]);
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingBacklink { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_correspondence_links_ignore_external_urls() {
        let yaml = CAST_LINKS_YAML;
        let md = "---\ntype: movie\n---\n\n# Movie\n\n## Cast\n\n- [IMDB](https://imdb.com/x)\n";
        let diags = link_fixture(yaml, md, Some("personality"), &[]);
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    // ── Correspondence: docs on disk nothing links (disk → prose) ─────────────

    const SEASONS_YAML: &str = "\
version: 2
structure:
  strict_sections: false
  sections:
    - title: Seasons
correspondence:
  - each: docs-of-type tvseason in-directory \".\"
    requires:
      link-in: \"## Seasons\"
";

    /// A show README beside season documents. `docs` is `(path, type)` for
    /// everything the walk would have preloaded around it.
    fn seasons_fixture(yaml: &str, show_md: &str, docs: &[(&str, &str)]) -> Vec<Diagnostic> {
        let show_def: TypeDef = serde_yaml::from_str(yaml).unwrap();
        show_def.validate("tvshow").expect("schema is valid");
        let mut schema = Schema::default();
        schema.types.insert("tvshow".to_string(), show_def.clone());

        let linked_docs: HashMap<PathBuf, LinkedDocInfo> = docs
            .iter()
            .map(|(path, doc_type)| {
                let path = PathBuf::from(path);
                (
                    path.clone(),
                    LinkedDocInfo {
                        path,
                        doc_type: Some(doc_type.to_string()),
                        section_links: std::collections::HashMap::new(),
                    },
                )
            })
            .collect();

        let doc = parse(show_md);
        let ctx = ValidateCtx {
            source_path: Path::new("/proj/3rd Rock/README.md"),
            schema_root: None,
            source_type: "tvshow",
            schema: &schema,
            linked_docs: &linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        };
        validate(&doc, &show_def, &ctx, None)
    }

    /// Every `UnlinkedDocument` a run produced, as `(file, line)`.
    fn unlinked(diags: &[Diagnostic]) -> Vec<(String, Option<usize>)> {
        diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::UnlinkedDocument { file, line, .. } => Some((file.clone(), *line)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_docs_of_type_passes_when_every_sibling_is_linked() {
        let diags = seasons_fixture(
            SEASONS_YAML,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n\n\
             - [Season 1](Season%201.md)\n- [Season 2](Season%202.md)\n",
            &[
                ("/proj/3rd Rock/Season 1.md", "tvseason"),
                ("/proj/3rd Rock/Season 2.md", "tvseason"),
            ],
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_docs_of_type_names_the_file_no_link_points_at() {
        let diags = seasons_fixture(
            SEASONS_YAML,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n\n- [Season 1](Season%201.md)\n",
            &[
                ("/proj/3rd Rock/Season 1.md", "tvseason"),
                ("/proj/3rd Rock/Season 2.md", "tvseason"),
            ],
        );
        // Anchored to the `## Seasons` heading, which is where the link goes.
        assert_eq!(unlinked(&diags), [("Season 2.md".to_string(), Some(7))]);
    }

    #[test]
    fn test_docs_of_type_flags_an_empty_section() {
        let diags = seasons_fixture(
            SEASONS_YAML,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n",
            &[("/proj/3rd Rock/Season 1.md", "tvseason")],
        );
        assert_eq!(unlinked(&diags), [("Season 1.md".to_string(), Some(7))]);
    }

    #[test]
    fn test_docs_of_type_flags_a_section_that_isnt_there_at_all() {
        // The case an `EmptyOptionalSection` strip used to erase: no heading,
        // no links, and a season doc sitting right beside the README.
        let diags = seasons_fixture(
            SEASONS_YAML,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n",
            &[("/proj/3rd Rock/Season 1.md", "tvseason")],
        );
        assert_eq!(unlinked(&diags), [("Season 1.md".to_string(), None)]);
    }

    #[test]
    fn test_docs_of_type_ignores_other_types_and_other_directories() {
        let diags = seasons_fixture(
            SEASONS_YAML,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n",
            &[
                ("/proj/3rd Rock/Cast.md", "personality"),
                ("/proj/Other Show/Season 1.md", "tvseason"),
                ("/proj/3rd Rock/Season 1/Episodes.md", "tvseason"),
            ],
        );
        assert!(unlinked(&diags).is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_docs_of_type_under_directory_descends() {
        let yaml = SEASONS_YAML.replace("in-directory", "under-directory");
        let diags = seasons_fixture(
            &yaml,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n",
            &[("/proj/3rd Rock/Season 1/Episodes.md", "tvseason")],
        );
        assert_eq!(
            unlinked(&diags),
            [("Season 1/Episodes.md".to_string(), Some(7))]
        );
    }

    #[test]
    fn test_docs_of_type_reports_files_in_a_stable_order() {
        let diags = seasons_fixture(
            SEASONS_YAML,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n",
            &[
                ("/proj/3rd Rock/Season 3.md", "tvseason"),
                ("/proj/3rd Rock/Season 1.md", "tvseason"),
                ("/proj/3rd Rock/Season 2.md", "tvseason"),
            ],
        );
        let files: Vec<String> = unlinked(&diags).into_iter().map(|(f, _)| f).collect();
        assert_eq!(files, ["Season 1.md", "Season 2.md", "Season 3.md"]);
    }

    #[test]
    fn test_docs_of_type_never_demands_a_link_to_the_document_itself() {
        // A type that indexes its own kind — the index doesn't have to link
        // itself just because it matches its own selector.
        let yaml = SEASONS_YAML.replace("docs-of-type tvseason", "docs-of-type tvshow");
        let diags = seasons_fixture(
            &yaml,
            "---\ntype: tvshow\n---\n\n# 3rd Rock\n\n## Seasons\n",
            &[("/proj/3rd Rock/README.md", "tvshow")],
        );
        assert!(unlinked(&diags).is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_docs_of_type_is_never_auto_fixed() {
        let diag = Diagnostic::UnlinkedDocument {
            line: Some(7),
            file: "Season 2.md".to_string(),
            doc_type: "tvseason".to_string(),
            section: "## Seasons".to_string(),
        };
        assert!(!crate::fix::Fix::is_fixable(&diag), "{diag:?}");
        assert!(
            crate::fix::Fix::from_diagnostic(&diag).is_none(),
            "{diag:?}"
        );
    }

    // ── Correspondence: frontmatter values and the links that answer them ──────

    /// A photo document whose frontmatter names the species and the place it
    /// was taken, with the body pointing at each of them from its own section.
    /// The species live one directory over, so their links are `%20`-encoded
    /// while the frontmatter values are not.
    const PHOTO_YAML: &str = "\
version: 2
structure:
  strict_sections: false
  sections:
    - title: Species
    - title: Location
correspondence:
  - each: frontmatter.species[]
    requires:
      link-in: \"## Species\"
      target_type: species
  - each: links-in-section \"## Species\"
    requires:
      frontmatter-item: species[]
  - each: frontmatter.location
    requires:
      link-in: \"## Location\"
  - each: links-in-section \"## Location\"
    requires:
      frontmatter-item: location
";

    /// The photo, plus the species documents sitting in `../species/`.
    fn photo_fixture(yaml: &str, photo_md: &str, species: &[(&str, &str)]) -> Vec<Diagnostic> {
        let photo_def: TypeDef = serde_yaml::from_str(yaml).unwrap();
        photo_def.validate("photo").expect("schema is valid");
        let mut schema = Schema::default();
        schema.types.insert("photo".to_string(), photo_def.clone());

        let linked_docs: HashMap<PathBuf, LinkedDocInfo> = species
            .iter()
            .map(|(path, doc_type)| {
                let path = PathBuf::from(path);
                (
                    path.clone(),
                    LinkedDocInfo {
                        path,
                        doc_type: Some(doc_type.to_string()),
                        section_links: std::collections::HashMap::new(),
                    },
                )
            })
            .collect();

        let doc = parse(photo_md);
        let ctx = ValidateCtx {
            source_path: Path::new("/birds/photos/2026-08-02.md"),
            schema_root: None,
            source_type: "photo",
            schema: &schema,
            linked_docs: &linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        };
        validate(&doc, &photo_def, &ctx, None)
    }

    const SPECIES_DOCS: &[(&str, &str)] = &[
        ("/birds/species/Blue Jay.md", "species"),
        ("/birds/species/Steller's Jay.md", "species"),
        ("/birds/places/Sunnyvale.md", "place"),
    ];

    #[test]
    fn test_frontmatter_item_matched_by_an_encoded_link_passes() {
        let diags = photo_fixture(
            PHOTO_YAML,
            "---\ntype: photo\nspecies:\n  - Blue Jay\nlocation: Sunnyvale\n---\n\n\
             # Photo\n\n## Species\n\n- [Blue Jay](../species/Blue%20Jay.md)\n\n\
             ## Location\n\n- [Sunnyvale](../places/Sunnyvale.md)\n",
            SPECIES_DOCS,
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_frontmatter_item_without_a_link() {
        let diags = photo_fixture(
            PHOTO_YAML,
            "---\ntype: photo\nspecies:\n  - Blue Jay\n  - Steller's Jay\nlocation: Sunnyvale\n---\n\n\
             # Photo\n\n## Species\n\n- [Blue Jay](../species/Blue%20Jay.md)\n\n\
             ## Location\n\n- [Sunnyvale](../places/Sunnyvale.md)\n",
            SPECIES_DOCS,
        );
        let missing: Vec<_> = diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::MissingItemLink {
                    item,
                    section,
                    line,
                    ..
                } => Some((item.as_str(), section.as_str(), *line)),
                _ => None,
            })
            .collect();
        // Anchored to the `species:` key on line 3 (`---` is line 1).
        assert_eq!(
            missing,
            [("Steller's Jay", "## Species", 3)],
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_link_without_a_frontmatter_item() {
        let diags = photo_fixture(
            PHOTO_YAML,
            "---\ntype: photo\nspecies:\n  - Blue Jay\nlocation: Sunnyvale\n---\n\n\
             # Photo\n\n## Species\n\n- [Blue Jay](../species/Blue%20Jay.md)\n\
             - [Steller's Jay](../species/Steller's%20Jay.md)\n\n\
             ## Location\n\n- [Sunnyvale](../places/Sunnyvale.md)\n",
            SPECIES_DOCS,
        );
        let orphans: Vec<_> = diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::OrphanLink { url, expected, .. } => {
                    Some((url.as_str(), expected.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            orphans,
            [("../species/Steller's%20Jay.md", "species[]")],
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_a_link_of_the_wrong_target_type_does_not_answer_the_item() {
        // The value is named by a link, but that link points at a place, not a
        // species — the rule asked for the species document.
        let diags = photo_fixture(
            PHOTO_YAML,
            "---\ntype: photo\nspecies:\n  - Sunnyvale\n---\n\n\
             # Photo\n\n## Species\n\n- [Sunnyvale](../places/Sunnyvale.md)\n",
            SPECIES_DOCS,
        );
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::MissingItemLink { item, target_type: Some(t), .. }
                    if item == "Sunnyvale" && t == "species"
            )),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_a_scalar_frontmatter_field_works_in_both_directions() {
        let diags = photo_fixture(
            PHOTO_YAML,
            "---\ntype: photo\nlocation: Sunnyvale\n---\n\n\
             # Photo\n\n## Location\n\n- [Mountain View](../places/Mountain%20View.md)\n",
            SPECIES_DOCS,
        );
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::MissingItemLink { item, section, .. }
                    if item == "Sunnyvale" && section == "## Location"
            )),
            "the scalar needs its link: {diags:?}"
        );
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::OrphanLink { url, expected, .. }
                    if url == "../places/Mountain%20View.md" && expected == "location"
            )),
            "the link needs its scalar: {diags:?}"
        );
    }

    #[test]
    fn test_a_field_of_each_record_can_name_the_link() {
        // `species[]` holds records, and it's one field of each that the link
        // has to answer — the same path spelling the subsection rules take.
        let yaml = "\
version: 2
structure:
  strict_sections: false
  sections:
    - title: Species
correspondence:
  - each: frontmatter.species[].name
    requires:
      link-in: \"## Species\"
";
        let diags = photo_fixture(
            yaml,
            "---\ntype: photo\nspecies:\n  - name: Blue Jay\n    count: 2\n  - name: Steller's Jay\n    count: 1\n---\n\n\
             # Photo\n\n## Species\n\n- [Blue Jay](../species/Blue%20Jay.md)\n",
            SPECIES_DOCS,
        );
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::MissingItemLink { item, .. } if item == "Steller's Jay"
            )),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_a_record_with_no_scalar_to_name_is_reported_against_the_item() {
        let yaml = "\
version: 2
structure:
  strict_sections: false
  sections:
    - title: Species
correspondence:
  - each: frontmatter.species[].name
    requires:
      link-in: \"## Species\"
";
        let diags = photo_fixture(
            yaml,
            "---\ntype: photo\nspecies:\n  - count: 2\n---\n\n# Photo\n\n## Species\n",
            SPECIES_DOCS,
        );
        assert!(
            diags.iter().any(|d| matches!(
                d,
                Diagnostic::InvalidFieldType { field, message, .. }
                    if field == "species[0]" && message.contains("no scalar 'name'")
            )),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_frontmatter_link_rules_are_vacuous_without_data() {
        // No `species:`, no `location:` — and no sections to answer them.
        let diags = photo_fixture(
            PHOTO_YAML,
            "---\ntype: photo\n---\n\n# Photo\n",
            SPECIES_DOCS,
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_frontmatter_link_rule_reports_a_missing_section_once() {
        let diags = photo_fixture(
            PHOTO_YAML,
            "---\ntype: photo\nspecies:\n  - Blue Jay\n  - Steller's Jay\n---\n\n# Photo\n",
            SPECIES_DOCS,
        );
        assert_eq!(
            diags
                .iter()
                .filter(
                    |d| matches!(d, Diagnostic::MissingSection { section } if section == "Species")
                )
                .count(),
            1,
            "got: {diags:?}"
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::MissingItemLink { .. })),
            "the absent section should not also fire per item: {diags:?}"
        );
    }

    #[test]
    fn test_frontmatter_link_correspondence_is_never_auto_fixed() {
        for diag in [
            Diagnostic::MissingItemLink {
                line: 3,
                item: "Blue Jay".to_string(),
                section: "## Species".to_string(),
                target_type: Some("species".to_string()),
            },
            Diagnostic::OrphanLink {
                line: 11,
                url: "../species/Blue%20Jay.md".to_string(),
                expected: "species[]".to_string(),
            },
        ] {
            assert!(!crate::fix::Fix::is_fixable(&diag), "{diag:?}");
            assert!(
                crate::fix::Fix::from_diagnostic(&diag).is_none(),
                "{diag:?}"
            );
        }
    }
}
