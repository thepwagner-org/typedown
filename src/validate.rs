//! Pure document validation.
//!
//! Takes `&Document` + `&TypeDef` (and optionally pre-loaded link data), returns
//! `Vec<Diagnostic>`. No filesystem access, no I/O, no thread-local caches.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    ast::{inlines_to_markdown, inlines_to_string, Block, Document, Frontmatter, Inline, ListItem},
    parse::parse,
    schema::{
        matches_template, parse_template, BulletMode, DateHeadingsDef, FieldDef, FieldType,
        HeadingSort, LinksDef, ManagedContent, Schema, SectionDef, StructureDef, TitleMode,
        TypeDef,
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
    /// A section that requires an intro paragraph is missing one.
    SectionNeedsIntro {
        insert_after_line: usize,
        text: String,
    },
    /// A managed section needs to be updated (created or template mismatch).
    ManagedSectionNeedsUpdate {
        /// Block index of section start (None if section is absent).
        section_start: Option<usize>,
        /// Block index of section end (exclusive).
        section_end: usize,
        /// Rendered template blocks that should replace the section.
        template_blocks: Vec<Block>,
        /// Custom content appended after the template (preserved on fix).
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
    /// The file exceeds the schema's size warning threshold.
    FileTooLarge {
        line: usize,
        size: usize,
        threshold: usize,
    },
    /// An unknown schema type was referenced.
    UnknownType { line: usize, message: String },
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
            Self::SectionNeedsIntro { text, .. } => {
                format!("section is missing required intro paragraph starting with '{text}'")
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
            Self::FileTooLarge {
                size, threshold, ..
            } => {
                format!("file is {size} bytes, exceeding the {threshold}-byte warning threshold")
            }
            Self::UnknownType { message, .. } => message.clone(),
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
            | Self::SectionNeedsIntro {
                insert_after_line: line,
                ..
            }
            | Self::LinkTargetTypeMismatch { line, .. }
            | Self::MissingBacklink { line, .. }
            | Self::FileTooLarge { line, .. }
            | Self::UnknownType { line, .. }
            | Self::InvalidDateHeading { line, .. }
            | Self::DateHeadingFileMismatch { line, .. } => Some(*line),
            Self::ManagedSectionNeedsUpdate { .. }
            | Self::EntriesOutOfOrder { .. }
            | Self::SectionsOutOfOrder { .. } => None,
            Self::MalformedLink { line, .. } => Some(*line),
            Self::EmptyOptionalSection { .. } => None,
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
    /// The document's declared type name (for bidirectional link validation).
    pub source_type: &'a str,
    /// Full schema (for type lookups).
    pub schema: &'a Schema,
    /// Pre-loaded info about linked documents (for link target + backlink checks).
    pub linked_docs: &'a HashMap<PathBuf, LinkedDocInfo>,
    /// Pre-loaded set of all git-tracked absolute paths (for cross-project links).
    pub git_tree: Option<&'a std::collections::HashSet<PathBuf>>,
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
    let mut in_frontmatter = false;
    let mut frontmatter_opened = false;
    let mut in_fenced_code = false;
    let mut line_num = 0usize;

    for line in content.lines() {
        line_num += 1;

        // Frontmatter: opening `---` on line 1, closing `---` on any later line
        if line == "---" {
            if !frontmatter_opened && line_num == 1 {
                in_frontmatter = true;
                frontmatter_opened = true;
                continue;
            } else if in_frontmatter {
                in_frontmatter = false;
                continue;
            }
        }
        if in_frontmatter {
            continue;
        }

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

    let Some(ref fm) = doc.frontmatter else {
        diagnostics.push(Diagnostic::MissingFrontmatter);
        return diagnostics;
    };

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

    validate_fields(fm, type_def, &mut diagnostics);
    validate_structure(doc, &type_def.structure, ctx, &mut diagnostics);

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

fn validate_fields(fm: &Frontmatter, type_def: &TypeDef, out: &mut Vec<Diagnostic>) {
    for (field_name, field_def) in &type_def.fields {
        let exists = fm.fields.contains_key(field_name.as_str());

        if field_def.required && !exists {
            out.push(Diagnostic::MissingRequiredField {
                line: 1,
                field: field_name.clone(),
            });
            continue;
        }

        if let Some(val) = fm.fields.get(field_name.as_str()) {
            validate_field_type(field_name, val, field_def, out);
        }
    }
}

/// Returns `Some(error_message)` if `value` fails the type check for the given
/// scalar field type. Does NOT recurse into `FieldType::List` items — callers
/// handle those themselves.
///
/// `allow_null`: when true, a YAML null value is silently accepted (frontmatter
/// fields are optional by default). Use `false` for list items and properties.
fn check_typed_value(
    value: &serde_yaml::Value,
    field_type: &FieldType,
    enum_values: Option<&[String]>,
    allow_null: bool,
) -> Option<String> {
    use serde_yaml::Value;
    let null_ok = allow_null && matches!(value, Value::Null);
    match field_type {
        FieldType::String | FieldType::Link => {
            if !value.is_string() && !null_ok {
                return Some("must be a string".to_string());
            }
        }
        FieldType::Date => match value {
            Value::String(s) => {
                if parse_date(s).is_none() {
                    return Some(format!("must be a valid date (got '{s}')"));
                }
            }
            _ if null_ok => {}
            _ => return Some("must be a date string".to_string()),
        },
        FieldType::Datetime => match value {
            Value::String(s) => {
                if parse_datetime(s).is_none() {
                    return Some(format!("must be a valid datetime (got '{s}')"));
                }
            }
            _ if null_ok => {}
            _ => return Some("must be a datetime string".to_string()),
        },
        FieldType::Integer => {
            if !value.is_i64() && !value.is_u64() && !null_ok {
                return Some("must be an integer".to_string());
            }
        }
        FieldType::Float => {
            if !value.is_f64() && !value.is_i64() && !value.is_u64() && !null_ok {
                return Some("must be a number".to_string());
            }
        }
        FieldType::Bool => {
            if !value.is_bool() && !null_ok {
                return Some("must be a boolean".to_string());
            }
        }
        FieldType::Enum => {
            if let Some(valid) = enum_values {
                match value {
                    Value::String(s) => {
                        if !valid.contains(s) {
                            return Some(format!(
                                "must be one of: {} (got '{s}')",
                                valid.join(", ")
                            ));
                        }
                    }
                    _ if null_ok => {}
                    _ => return Some("must be a string enum value".to_string()),
                }
            }
        }
        FieldType::List => {} // callers handle list recursion
    }
    None
}

fn validate_field_type(
    field_name: &str,
    value: &serde_yaml::Value,
    field_def: &FieldDef,
    out: &mut Vec<Diagnostic>,
) {
    // List fields need special handling: check outer sequence, then recurse into items.
    if field_def.field_type == FieldType::List {
        match value {
            serde_yaml::Value::Sequence(seq) => {
                if let Some(item_type) = &field_def.item_type {
                    for (i, item) in seq.iter().enumerate() {
                        validate_list_item(field_name, i, item, item_type, field_def, out);
                    }
                }
            }
            serde_yaml::Value::Null => {}
            _ => out.push(Diagnostic::InvalidFieldType {
                line: 1,
                field: field_name.to_string(),
                message: "must be a list".to_string(),
            }),
        }
        return;
    }

    if let Some(msg) = check_typed_value(
        value,
        &field_def.field_type,
        field_def.values.as_deref(),
        true,
    ) {
        out.push(Diagnostic::InvalidFieldType {
            line: 1,
            field: field_name.to_string(),
            message: msg,
        });
    }
}

fn validate_list_item(
    field_name: &str,
    index: usize,
    item: &serde_yaml::Value,
    item_type: &FieldType,
    field_def: &FieldDef,
    out: &mut Vec<Diagnostic>,
) {
    let item_name = format!("{field_name}[{index}]");

    if *item_type == FieldType::List {
        out.push(Diagnostic::InvalidFieldType {
            line: 1,
            field: item_name,
            message: "nested lists are not supported".to_string(),
        });
        return;
    }

    if let Some(msg) = check_typed_value(item, item_type, field_def.values.as_deref(), false) {
        out.push(Diagnostic::InvalidFieldType {
            line: 1,
            field: item_name,
            message: msg,
        });
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
            ctx.source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        } else {
            None
        };
        validate_date_headings(doc, dh, file_period.as_deref(), out);
    } else if !structure.sections.is_empty() {
        validate_sections(doc, structure, ctx, out);

        // Managed sections
        for section_def in &structure.sections {
            if let Some(ref managed) = section_def.managed_content {
                validate_managed_section(doc, &section_def.title, managed, out);
            }
        }
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
        TitleMode::FromProject => {
            // Project name: frontmatter `name` field or filename stem.
            let expected = doc
                .frontmatter
                .as_ref()
                .and_then(|fm| fm.fields.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    source_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                });
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
        TitleMode::FromDate => {
            let expected = month_title_from_path(source_path).unwrap_or_default();
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

/// Derive a human-readable month title from a `YYYY-MM`-stemmed path.
///
/// `journal/2026-02.md` → `"February 2026"`.  Returns `None` if the stem
/// can't be parsed as `YYYY-MM`.
pub fn month_title_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let (year_str, month_str) = stem.split_once('-')?;
    let year: i32 = year_str.parse().ok()?;
    let month: u32 = month_str.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
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
    Some(format!("{month_name} {year}"))
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
                    BulletMode::Any => true,
                    BulletMode::Ordered => *ordered,
                    BulletMode::Unordered => !*ordered,
                };
                if !ok {
                    let expected = match mode {
                        BulletMode::Ordered => "ordered",
                        BulletMode::Unordered => "unordered",
                        BulletMode::Any => unreachable!(),
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

        // intro_text check
        if let Some(ref intro_text) = section_def.intro_text {
            validate_section_intro(doc, *start_pos, end_pos, intro_text, out);
        }

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
        FieldType::String | FieldType::Link => {}
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

fn validate_section_intro(
    doc: &Document,
    section_start: usize,
    section_end: usize,
    intro_text: &str,
    out: &mut Vec<Diagnostic>,
) {
    let content_start = section_start + 1;
    let content_idx = doc.blocks[content_start..section_end]
        .iter()
        .position(|b| !matches!(b, Block::BlankLine))
        .map(|i| content_start + i);

    let has_intro = content_idx.is_some_and(|i| {
        if let Block::Paragraph { content, .. } = &doc.blocks[i] {
            inlines_to_string(content).starts_with(intro_text)
        } else {
            false
        }
    });

    if !has_intro {
        // insert_after_line is the heading line; the fix inserts the paragraph
        // immediately after the heading (before any existing section content).
        let insert_line = doc.blocks.get(section_start).map(|b| b.line()).unwrap_or(1);
        out.push(Diagnostic::SectionNeedsIntro {
            insert_after_line: insert_line,
            text: intro_text.to_string(),
        });
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
            Block::List { items, ordered, .. } => Block::List {
                items: items
                    .iter()
                    .map(|item| crate::ast::ListItem {
                        content: item.content.clone(),
                        children: item.children.iter().map(zero_line).collect(),
                    })
                    .collect(),
                ordered: *ordered,
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
    let template_doc = parse(&managed.template);
    let template_blocks: Vec<Block> = template_doc.blocks;
    let template_content_count = template_blocks
        .iter()
        .filter(|b| !matches!(b, Block::BlankLine))
        .count();

    match find_managed_section(doc, section_title, &managed.migrate_from) {
        Some((idx, legacy_sections)) => {
            let last_section_idx = legacy_sections.last().copied().unwrap_or(idx);
            let section_end = doc.blocks[last_section_idx + 1..]
                .iter()
                .position(|b| matches!(b, Block::Heading { level: 2, .. }))
                .map(|i| last_section_idx + 1 + i)
                .unwrap_or(doc.blocks.len());

            let existing_content: Vec<Block> = doc.blocks[idx..section_end]
                .iter()
                .filter(|b| !matches!(b, Block::BlankLine))
                .cloned()
                .collect();

            let expected_blocks: Vec<Block> = template_blocks
                .iter()
                .filter(|b| !matches!(b, Block::BlankLine))
                .cloned()
                .collect();

            // Extract user-added content that lives beyond (or appended to) the
            // template blocks.  Two cases:
            //
            // 1. Extra whole blocks after the template: collect them directly.
            // 2. Extra list items appended to the last template block (which is a
            //    list): lift them out as a separate list block so the fix can
            //    re-append them without losing them.
            let custom_content: Vec<Block> = {
                let mut custom = Vec::new();

                // Whole blocks beyond the template prefix
                if existing_content.len() > template_content_count {
                    custom.extend_from_slice(&existing_content[template_content_count..]);
                }

                // Extra items appended to the last template list block
                if let (
                    Some(Block::List {
                        items: tmpl_items,
                        ordered: tmpl_ordered,
                        ..
                    }),
                    Some(Block::List {
                        items: exist_items,
                        ordered: exist_ordered,
                        ..
                    }),
                ) = (
                    expected_blocks.last(),
                    existing_content.get(template_content_count.saturating_sub(1)),
                ) {
                    if tmpl_ordered == exist_ordered && exist_items.len() > tmpl_items.len() {
                        let extra_items = exist_items[tmpl_items.len()..].to_vec();
                        custom.push(Block::List {
                            items: extra_items,
                            ordered: *exist_ordered,
                            line: 0,
                        });
                    }
                }

                custom
            };

            let needs_update = !legacy_sections.is_empty() || {
                // Compare the template prefix against the existing content,
                // ignoring any extra items that may have been appended to the
                // last list block (those are captured in custom_content above).
                let existing_prefix: Vec<Block> = {
                    let mut prefix = existing_content
                        .iter()
                        .take(template_content_count)
                        .cloned()
                        .collect::<Vec<_>>();

                    // If the last block of the prefix is a list with more items
                    // than the template expects, truncate it to the template
                    // length for the comparison (the extras are custom content).
                    if let (
                        Some(Block::List {
                            items: tmpl_items, ..
                        }),
                        Some(Block::List {
                            items: exist_items,
                            ordered,
                            line,
                        }),
                    ) = (expected_blocks.last(), prefix.last_mut())
                    {
                        if exist_items.len() > tmpl_items.len() {
                            let truncated = exist_items[..tmpl_items.len()].to_vec();
                            *prefix.last_mut().unwrap() = Block::List {
                                items: truncated,
                                ordered: *ordered,
                                line: *line,
                            };
                        }
                    }

                    prefix
                };

                !blocks_content_equal(&existing_prefix, &expected_blocks)
            };

            if needs_update {
                out.push(Diagnostic::ManagedSectionNeedsUpdate {
                    section_start: Some(idx),
                    section_end,
                    template_blocks,
                    custom_content,
                });
            }
        }
        None => {
            out.push(Diagnostic::ManagedSectionNeedsUpdate {
                section_start: None,
                section_end: doc.blocks.len(),
                template_blocks,
                custom_content: vec![],
            });
        }
    }
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
        let in_git = ctx.git_tree.is_some_and(|t| t.contains(&target));
        if !in_linked && !in_git {
            out.push(Diagnostic::UnknownType {
                line,
                message: format!("broken link: '{url}' does not exist"),
            });
        }
    }
}

/// Extract all link and image URLs from a block list.
fn extract_links(blocks: &[Block]) -> Vec<(String, usize)> {
    let mut links = Vec::new();
    for block in blocks {
        collect_links_from_block(block, &mut links);
    }
    links
}

fn collect_links_from_block(block: &Block, links: &mut Vec<(String, usize)>) {
    let line = block.line();
    match block {
        Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
            collect_links_from_inlines(content, line, links)
        }
        Block::List { items, .. } => {
            for item in items {
                collect_links_from_inlines(&item.content, line, links);
                for child in &item.children {
                    collect_links_from_block(child, links);
                }
            }
        }
        Block::BlockQuote { blocks, .. } => {
            for inner in blocks {
                collect_links_from_block(inner, links);
            }
        }
        Block::Table { header, rows, .. } => {
            for cell in header {
                collect_links_from_inlines(cell, line, links);
            }
            for row in rows {
                for cell in row {
                    collect_links_from_inlines(cell, line, links);
                }
            }
        }
        _ => {}
    }
}

fn collect_links_from_inlines(inlines: &[Inline], line: usize, links: &mut Vec<(String, usize)>) {
    for inline in inlines {
        match inline {
            Inline::Link { url, .. } | Inline::Image { url, .. } => links.push((url.clone(), line)),
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strikethrough(inner) => {
                collect_links_from_inlines(inner, line, links)
            }
            _ => {}
        }
    }
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
            source_type,
            schema,
            linked_docs,
            git_tree: None,
            external_types: empty_external_types(),
        }
    }

    // ── Frontmatter / fields ──────────────────────────────────────────────────

    #[test]
    fn test_missing_frontmatter() {
        let (schema, type_def) = make_schema("note", "description: a note\n");
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
        let (schema, type_def) = make_schema("note", "description: a note\n");
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

    #[test]
    fn test_valid_document_no_errors() {
        let (schema, type_def) = make_schema(
            "person",
            "fields:\n  name:\n    type: string\n    required: true\n",
        );
        let doc = parse("---\ntype: person\nname: Alice\n---\n# Alice\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("person", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(diags.is_empty(), "expected no errors, got: {diags:?}");
    }

    #[test]
    fn test_missing_required_field() {
        let (schema, type_def) = make_schema(
            "person",
            "fields:\n  name:\n    type: string\n    required: true\n",
        );
        let doc = parse("---\ntype: person\n---\n# Person\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("person", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message().contains("missing required field 'name'")),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_invalid_integer_field() {
        let (schema, type_def) = make_schema(
            "t",
            "fields:\n  count:\n    type: integer\n    required: true\n",
        );
        let doc = parse("---\ntype: t\ncount: hello\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message().contains("must be an integer")),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_invalid_float_field() {
        let (schema, type_def) = make_schema(
            "t",
            "fields:\n  rating:\n    type: float\n    required: true\n",
        );
        let doc = parse("---\ntype: t\nrating: hello\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message().contains("must be a number")),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_valid_float_field_accepts_integer() {
        let (schema, type_def) = make_schema(
            "t",
            "fields:\n  rating:\n    type: float\n    required: true\n",
        );
        let doc = parse("---\ntype: t\nrating: 3\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags.iter().any(|d| d.message().contains("must be")),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_valid_float_field_accepts_decimal() {
        let (schema, type_def) = make_schema(
            "t",
            "fields:\n  rating:\n    type: float\n    required: true\n",
        );
        let doc = parse("---\ntype: t\nrating: 3.14\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags.iter().any(|d| d.message().contains("must be")),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_enum_field_invalid_value() {
        let (schema, type_def) = make_schema(
            "show",
            "fields:\n  status:\n    type: enum\n    required: true\n    values: [watching, done]\n",
        );
        let doc = parse("---\ntype: show\nstatus: unknown\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("show", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags.iter().any(|d| d.message().contains("must be one of")),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_enum_field_valid_value() {
        let (schema, type_def) = make_schema(
            "show",
            "fields:\n  status:\n    type: enum\n    required: true\n    values: [watching, done]\n",
        );
        let doc = parse("---\ntype: show\nstatus: watching\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("show", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_date_field_valid() {
        let (schema, type_def) = make_schema(
            "t",
            "fields:\n  created:\n    type: date\n    required: true\n",
        );
        let doc = parse("---\ntype: t\ncreated: 2024-03-15\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_date_field_invalid() {
        let (schema, type_def) = make_schema(
            "t",
            "fields:\n  created:\n    type: date\n    required: true\n",
        );
        let doc = parse("---\ntype: t\ncreated: not-a-date\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags.iter().any(|d| d.message().contains("valid date")),
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
            diags.iter().all(|d| crate::fix::Fix::is_fixable(d)),
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
        // Section with `bullets: any` rejects paragraph content
        let yaml = r"
structure:
  sections:
    - title: Goals
      bullets: any
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
    fn test_section_bullets_any_accepts_both() {
        let yaml = r"
structure:
  strict_sections: false
  sections:
    - title: Mixed
      bullets: any
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Mixed\n\n- Unordered item\n\n1. Ordered item\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags.iter().any(|d| matches!(
                d,
                Diagnostic::SectionNotBullets { .. } | Diagnostic::WrongListType { .. }
            )),
            "got: {diags:?}"
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
            month_title_from_path(Path::new("journal/2026-02.md")),
            Some("February 2026".to_string())
        );
        assert_eq!(
            month_title_from_path(Path::new("2024-12.md")),
            Some("December 2024".to_string())
        );
        assert_eq!(month_title_from_path(Path::new("not-a-date.md")), None);
        assert_eq!(month_title_from_path(Path::new("2026-13.md")), None);
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

    #[test]
    fn test_list_of_strings_valid() {
        let yaml = "fields:\n  tags:\n    type: list\n    item_type: string\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\ntags:\n  - alpha\n  - beta\n---\n# Title\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::InvalidFieldType { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_list_of_integers_valid() {
        let yaml = "fields:\n  counts:\n    type: list\n    item_type: integer\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\ncounts:\n  - 1\n  - 42\n---\n# Title\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::InvalidFieldType { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_list_of_integers_invalid_item() {
        let yaml = "fields:\n  counts:\n    type: list\n    item_type: integer\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\ncounts:\n  - 1\n  - not-a-number\n---\n# Title\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::InvalidFieldType { field, .. }
                if field.contains("counts"))),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_list_empty_is_valid() {
        let yaml =
            "fields:\n  tags:\n    type: list\n    item_type: enum\n    values:\n      - a\n      - b\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\ntags: []\n---\n# Title\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d, Diagnostic::InvalidFieldType { .. })),
            "empty list should be valid, got: {diags:?}"
        );
    }

    #[test]
    fn test_list_multiple_invalid_items_each_reported() {
        let yaml =
            "fields:\n  tags:\n    type: list\n    item_type: enum\n    values:\n      - good\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc =
            parse("---\ntype: doc\ntags:\n  - good\n  - bad1\n  - bad2\n  - bad3\n---\n# Title\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, empty_linked_docs()),
            None,
        );
        let field_errs: Vec<_> = diags
            .iter()
            .filter(|d| matches!(d, Diagnostic::InvalidFieldType { field, .. } if field.contains("tags")))
            .collect();
        assert_eq!(
            field_errs.len(),
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
}
