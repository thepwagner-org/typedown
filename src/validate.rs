//! Pure document validation.
//!
//! Takes `&Document` + `&TypeDef` (and optionally pre-loaded link data), returns
//! `Vec<Diagnostic>`. No filesystem access, no I/O, no thread-local caches.

use std::path::{Path, PathBuf};

use crate::{
    ast::{inlines_to_markdown, inlines_to_string, Block, Document, Frontmatter, Inline, ListItem},
    parse::parse,
    schema::{
        matches_template, parse_template, DateHeadingsDef, FieldDef, FieldType, HeadingSort,
        LinksDef, ManagedContent, Schema, SectionDef, StructureDef, TitleMode, TypeDef,
    },
};

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
    /// A section appears out of order.
    SectionOutOfOrder { line: usize, section: String },
    /// A section contains non-bullet content where only bullets are expected.
    SectionNotBullets { line: usize, context: String },
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
        /// Entries sorted into correct order: `(date_str, time_str_opt, blocks)`.
        sorted_entries: Vec<(String, Option<String>, Vec<Block>)>,
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
            Self::SectionNotBullets { context, .. } => {
                format!("{context}: only bullet lists are allowed here")
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
                format!("'{text}' is not a valid date heading (expected YYYY-MM-DD or YYYY-MM-DD HH:MM)")
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
            Self::ManagedSectionNeedsUpdate { .. } | Self::EntriesOutOfOrder { .. } => None,
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
    pub linked_docs: &'a [LinkedDocInfo],
    /// Pre-loaded set of all git-tracked absolute paths (for cross-project links).
    pub git_tree: Option<&'a std::collections::HashSet<PathBuf>>,
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

    // `[text](url-containing-a-space-or-tab)`
    // Capture group 2 is the URL portion, which must contain at least one
    // space or horizontal tab.
    let Ok(re) = Regex::new(r"\[([^\]]+)\]\(([^)]*[ \t][^)]*)\)") else {
        return vec![];
    };

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

    if type_def.structure.validate_all_links {
        validate_all_links(doc, ctx, &mut diagnostics);
    }

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

fn validate_field_type(
    field_name: &str,
    value: &serde_yaml::Value,
    field_def: &FieldDef,
    out: &mut Vec<Diagnostic>,
) {
    use serde_yaml::Value;

    let bad = |msg: &str| Diagnostic::InvalidFieldType {
        line: 1,
        field: field_name.to_string(),
        message: msg.to_string(),
    };

    match field_def.field_type {
        FieldType::String => {
            if !value.is_string() && !matches!(value, Value::Null) {
                out.push(bad("must be a string"));
            }
        }
        FieldType::Date => match value {
            Value::String(s) => {
                if parse_date(s).is_none() {
                    out.push(bad(&format!("must be a valid date (got '{s}')")));
                }
            }
            Value::Null => {}
            _ => out.push(bad("must be a date string")),
        },
        FieldType::Datetime => match value {
            Value::String(s) => {
                if parse_datetime(s).is_none() {
                    out.push(bad(&format!("must be a valid datetime (got '{s}')")));
                }
            }
            Value::Null => {}
            _ => out.push(bad("must be a datetime string")),
        },
        FieldType::Integer => {
            if !value.is_i64() && !value.is_u64() && !matches!(value, Value::Null) {
                out.push(bad("must be an integer"));
            }
        }
        FieldType::Bool => {
            if !value.is_bool() && !matches!(value, Value::Null) {
                out.push(bad("must be a boolean"));
            }
        }
        FieldType::Enum => {
            if let Some(valid) = &field_def.values {
                match value {
                    Value::String(s) => {
                        if !valid.contains(s) {
                            out.push(bad(&format!(
                                "must be one of: {} (got '{s}')",
                                valid.join(", ")
                            )));
                        }
                    }
                    Value::Null => {}
                    _ => out.push(bad("must be a string enum value")),
                }
            }
        }
        FieldType::Link => {
            if !value.is_string() && !matches!(value, Value::Null) {
                out.push(bad("must be a link string"));
            }
        }
        FieldType::List => match value {
            Value::Sequence(seq) => {
                if let Some(item_type) = &field_def.item_type {
                    for (i, item) in seq.iter().enumerate() {
                        validate_list_item(field_name, i, item, item_type, field_def, out);
                    }
                }
            }
            Value::Null => {}
            _ => out.push(bad("must be a list")),
        },
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
    use serde_yaml::Value;

    let item_name = format!("{field_name}[{index}]");
    let bad = |msg: &str| Diagnostic::InvalidFieldType {
        line: 1,
        field: item_name.clone(),
        message: msg.to_string(),
    };

    match item_type {
        FieldType::String => {
            if !item.is_string() {
                out.push(bad("must be a string"));
            }
        }
        FieldType::Date => match item {
            Value::String(s) => {
                if parse_date(s).is_none() {
                    out.push(bad(&format!("must be a valid date (got '{s}')")));
                }
            }
            _ => out.push(bad("must be a date string")),
        },
        FieldType::Datetime => match item {
            Value::String(s) => {
                if parse_datetime(s).is_none() {
                    out.push(bad(&format!("must be a valid datetime (got '{s}')")));
                }
            }
            _ => out.push(bad("must be a datetime string")),
        },
        FieldType::Integer => {
            if !item.is_i64() && !item.is_u64() {
                out.push(bad("must be an integer"));
            }
        }
        FieldType::Bool => {
            if !item.is_bool() {
                out.push(bad("must be a boolean"));
            }
        }
        FieldType::Enum => {
            if let Some(valid) = &field_def.values {
                match item {
                    Value::String(s) => {
                        if !valid.contains(s) {
                            out.push(bad(&format!(
                                "must be one of: {} (got '{s}')",
                                valid.join(", ")
                            )));
                        }
                    }
                    _ => out.push(bad("must be a string enum value")),
                }
            }
        }
        FieldType::Link => {
            if !item.is_string() {
                out.push(bad("must be a link string"));
            }
        }
        FieldType::List => {
            out.push(bad("nested lists are not supported"));
        }
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
        TitleMode::Fixed(expected) => {
            if h1.is_none() {
                out.push(Diagnostic::MissingH1 {
                    expected: expected.clone(),
                });
            }
        }
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

/// Parse an H2 text as a journal date entry: `YYYY-MM-DD` or `YYYY-MM-DD HH:MM`.
///
/// Returns `(date_str, time_str_or_none)` on success, `None` on failure.
fn parse_entry_heading(text: &str) -> Option<(String, Option<String>)> {
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
    // Optionally followed by " HH:MM"
    let time_part = if rest.is_empty() {
        None
    } else if rest.starts_with(' ') && rest.len() == 6 {
        let t = &rest[1..]; // "HH:MM"
        let tb = t.as_bytes();
        if tb[0..2].iter().all(|c| c.is_ascii_digit())
            && tb[2] == b':'
            && tb[3..5].iter().all(|c| c.is_ascii_digit())
        {
            Some(t.to_string())
        } else {
            return None;
        }
    } else {
        return None;
    };
    Some((date_part.to_string(), time_part))
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
    let mut valid_entries: Vec<(String, Option<String>, usize)> = Vec::new(); // (date, time, block_idx)
    for (block_idx, text, line) in &h2s {
        match parse_entry_heading(text) {
            None => {
                out.push(Diagnostic::InvalidDateHeading {
                    line: *line,
                    text: text.clone(),
                });
            }
            Some((date, time)) => {
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
                valid_entries.push((date, time, *block_idx));
            }
        }
    }

    // Check sort order
    if valid_entries.len() < 2 {
        return;
    }
    let keys: Vec<(String, String)> = valid_entries
        .iter()
        .map(|(d, t, _)| entry_sort_key(d, t.as_deref()))
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
        let mut entries_with_blocks: Vec<(String, Option<String>, Vec<Block>)> = valid_entries
            .iter()
            .enumerate()
            .map(|(ei, (date, time, start_idx))| {
                // Find the next H2 block index
                let next_h2_idx = valid_entries
                    .get(ei + 1)
                    .map(|(_, _, idx)| *idx)
                    .unwrap_or(doc.blocks.len());
                let entry_blocks = doc.blocks[*start_idx..next_h2_idx].to_vec();
                (date.clone(), time.clone(), entry_blocks)
            })
            .collect();

        // Sort
        entries_with_blocks.sort_by(|(da, ta, _), (db, tb, _)| {
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

    if intro_def.paragraph {
        return; // paragraphs allowed
    }

    validate_bullets_only(intro_blocks, "intro", out);
}

/// Validate that all blocks in a slice are bullet lists (or blank lines).
fn validate_bullets_only(blocks: &[Block], context: &str, out: &mut Vec<Diagnostic>) {
    for block in blocks {
        match block {
            Block::List { .. } | Block::BlankLine => {}
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
        let mut last_schema_idx = 0usize;
        for (_, h2, line) in &h2s {
            if let Some(idx) = allowed_titles.iter().position(|s| *s == h2.as_str()) {
                if idx < last_schema_idx {
                    out.push(Diagnostic::SectionOutOfOrder {
                        line: *line,
                        section: h2.clone(),
                    });
                }
                last_schema_idx = idx;
            }
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

        // Paragraphs allowed → skip bullet/template checks
        if section_def.paragraph {
            // Still validate links if configured
            if let Some(ref links_def) = section_def.links {
                validate_section_links(section_blocks, *heading_line, links_def, ctx, out);
            }
            continue;
        }

        // Bullets only
        validate_bullets_only(section_blocks, &format!("section '{section_title}'"), out);

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

        // Link constraints
        if let Some(ref links_def) = section_def.links {
            validate_section_links(section_blocks, *heading_line, links_def, ctx, out);
        }
    }
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

            let custom_content: Vec<Block> = if existing_content.len() > template_content_count {
                existing_content[template_content_count..].to_vec()
            } else {
                vec![]
            };

            let needs_update = !legacy_sections.is_empty() || {
                let existing_portion: Vec<_> = existing_content
                    .iter()
                    .take(template_content_count)
                    .collect();
                let expected_portion: Vec<_> = template_blocks
                    .iter()
                    .filter(|b| !matches!(b, Block::BlankLine))
                    .collect();
                existing_portion.len() != expected_portion.len()
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
    for url in all_urls {
        if url.starts_with("http://") || url.starts_with("https://") || url.starts_with('#') {
            continue;
        }
        let Some(target) = resolve_link_path(&url, ctx.source_path) else {
            continue;
        };
        let in_linked = ctx.linked_docs.iter().any(|d| d.path == target);
        let in_git = ctx.git_tree.is_some_and(|t| t.contains(&target));
        if !in_linked && !in_git {
            out.push(Diagnostic::UnknownType {
                line: 0,
                message: format!("broken link: '{url}' does not exist"),
            });
        }
    }
}

/// Extract all link URLs from the bullet items within a section's blocks.
fn extract_links(blocks: &[Block]) -> Vec<String> {
    let mut links = Vec::new();
    for block in blocks {
        collect_links_from_block(block, &mut links);
    }
    links
}

fn collect_links_from_block(block: &Block, links: &mut Vec<String>) {
    match block {
        Block::List { items, .. } => {
            for item in items {
                collect_links_from_inlines(&item.content, links);
                for child in &item.children {
                    collect_links_from_block(child, links);
                }
            }
        }
        Block::Paragraph { content, .. } => collect_links_from_inlines(content, links),
        _ => {}
    }
}

fn collect_links_from_inlines(inlines: &[Inline], links: &mut Vec<String>) {
    for inline in inlines {
        match inline {
            Inline::Link { url, .. } => links.push(url.clone()),
            Inline::Strong(inner) | Inline::Emphasis(inner) => {
                collect_links_from_inlines(inner, links)
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

    let decoded = percent_decode(link);
    let base_dir = source_path.parent()?;
    let resolved = base_dir.join(Path::new(&decoded));
    Some(normalize_path(&resolved))
}

/// Simple percent-decoding for URL path components (e.g. `%20` → space).
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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

    for url in &links {
        if url.starts_with("http://") || url.starts_with("https://") {
            continue;
        }

        let Some(target_path) = resolve_link_path(url, ctx.source_path) else {
            continue;
        };

        let linked = ctx.linked_docs.iter().find(|d| d.path == target_path);

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
    let Some(target_type_def) = ctx.schema.get_type(target_type) else {
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

    /// Convenience: build a ValidateCtx for tests (no git tree, empty linked_docs by default).
    fn make_ctx<'a>(
        source_type: &'a str,
        source_path: &'a Path,
        schema: &'a Schema,
        linked_docs: &'a [LinkedDocInfo],
    ) -> ValidateCtx<'a> {
        ValidateCtx {
            source_path,
            source_type,
            schema,
            linked_docs,
            git_tree: None,
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
            &make_ctx("note", empty_path(), &schema, &[]),
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
            &make_ctx("note", empty_path(), &schema, &[]),
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
            &make_ctx("person", empty_path(), &schema, &[]),
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
            &make_ctx("person", empty_path(), &schema, &[]),
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
    fn test_enum_field_invalid_value() {
        let (schema, type_def) = make_schema(
            "show",
            "fields:\n  status:\n    type: enum\n    required: true\n    values: [watching, done]\n",
        );
        let doc = parse("---\ntype: show\nstatus: unknown\n---\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("show", empty_path(), &schema, &[]),
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
            &make_ctx("show", empty_path(), &schema, &[]),
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
        let diags = validate(&doc, &type_def, &make_ctx("page", path, &schema, &[]), None);
        assert!(diags.is_empty(), "got: {diags:?}");
    }

    #[test]
    fn test_title_from_filename_mismatch() {
        let (schema, type_def) = make_schema("page", "structure:\n  title: from_filename\n");
        let doc = parse("---\ntype: page\n---\n# wrong title\n");
        let path = Path::new("my-page.md");
        let diags = validate(&doc, &type_def, &make_ctx("page", path, &schema, &[]), None);
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
    fn test_section_not_bullets() {
        let yaml = r"
structure:
  sections:
    - title: Goals
";
        let (schema, type_def) = make_schema("t", yaml);
        // Goals section contains a paragraph, not a bullet
        let doc = parse("---\ntype: t\n---\n## Goals\n\nThis is a paragraph.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, &[]),
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
    fn test_section_paragraph_allowed() {
        let yaml = r"
structure:
  sections:
    - title: Notes
      paragraph: true
";
        let (schema, type_def) = make_schema("t", yaml);
        let doc = parse("---\ntype: t\n---\n## Notes\n\nFree text here.\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("t", empty_path(), &schema, &[]),
            None,
        );
        assert!(diags.is_empty(), "got: {diags:?}");
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

        let doc = parse("---\ntype: task\n---\n## Related\n\n- [Target](target.md)\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("task", source_path, &schema, &[linked]),
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
            &make_ctx("t", empty_path(), &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
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
            &make_ctx("journal", path, &schema, &[]),
            None,
        );
        let order_diags: Vec<_> = diags
            .iter()
            .filter(|d| matches!(d, Diagnostic::EntriesOutOfOrder { .. }))
            .collect();
        assert!(order_diags.is_empty(), "got: {diags:?}");
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
        let yaml =
            "structure:\n  sections:\n    - title: Notes\n      required: false\n      paragraph: true\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\n## Notes\n");
        let path = Path::new("test.md");
        let diags = validate(&doc, &type_def, &make_ctx("doc", path, &schema, &[]), None);
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::EmptyOptionalSection { .. })),
            "got: {diags:?}"
        );
    }

    #[test]
    fn test_non_empty_optional_section_not_flagged() {
        let yaml =
            "structure:\n  sections:\n    - title: Notes\n      required: false\n      paragraph: true\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\n## Notes\n\nSome content here.\n");
        let path = Path::new("test.md");
        let diags = validate(&doc, &type_def, &make_ctx("doc", path, &schema, &[]), None);
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
        let yaml =
            "structure:\n  sections:\n    - title: Goals\n      required: true\n      paragraph: true\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\n## Goals\n");
        let path = Path::new("test.md");
        let diags = validate(&doc, &type_def, &make_ctx("doc", path, &schema, &[]), None);
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
        let yaml = "structure:\n  validate_all_links: true\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\nSee [this](missing.md).\n");
        let path = Path::new("/project/doc.md");
        let diags = validate(&doc, &type_def, &make_ctx("doc", path, &schema, &[]), None);
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
        let yaml = "structure:\n  validate_all_links: true\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\n---\n# Title\n\nSee [this](other%20doc.md).\n");
        let source_path = Path::new("/project/doc.md");
        let linked = LinkedDocInfo {
            path: PathBuf::from("/project/other doc.md"),
            doc_type: None,
            section_links: Default::default(),
        };
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", source_path, &schema, &[linked]),
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

    // ── List field validation ─────────────────────────────────────────────────

    #[test]
    fn test_list_of_strings_valid() {
        let yaml = "fields:\n  tags:\n    type: list\n    item_type: string\n";
        let (schema, type_def) = make_schema("doc", yaml);
        let doc = parse("---\ntype: doc\ntags:\n  - alpha\n  - beta\n---\n# Title\n");
        let diags = validate(
            &doc,
            &type_def,
            &make_ctx("doc", empty_path(), &schema, &[]),
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
            &make_ctx("doc", empty_path(), &schema, &[]),
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
            &make_ctx("doc", empty_path(), &schema, &[]),
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
            &make_ctx("doc", empty_path(), &schema, &[]),
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
            &make_ctx("doc", empty_path(), &schema, &[]),
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

        let doc =
            parse("---\ntype: project\n---\n# My Project\n\n## Team\n\n- [Alice](person.md)\n");
        let ctx = ValidateCtx {
            source_path: project_path,
            source_type: "project",
            schema: &schema,
            linked_docs: &[linked_person],
            git_tree: None,
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

        let doc =
            parse("---\ntype: project\n---\n# My Project\n\n## Team\n\n- [Alice](person.md)\n");
        let ctx = ValidateCtx {
            source_path: project_path,
            source_type: "project",
            schema: &schema,
            linked_docs: &[linked_person],
            git_tree: None,
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

        let doc =
            parse("---\ntype: project\n---\n# My Project\n\n## Team\n\n- [Alice](person.md)\n");
        let ctx = ValidateCtx {
            source_path: project_path,
            source_type: "project",
            schema: &schema,
            linked_docs: &[linked_person],
            git_tree: None,
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

        let doc = parse(
            "---\ntype: threat\n---\n# Threat A\n\n## Leads To\n\n- [Threat B](threat-b.md)\n",
        );
        let ctx = ValidateCtx {
            source_path: a_path,
            source_type: "threat",
            schema: &schema,
            linked_docs: &[linked_b],
            git_tree: None,
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
            &make_ctx("doc", empty_path(), &schema, &[]),
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
            &make_ctx("doc", empty_path(), &schema, &[]),
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
}
