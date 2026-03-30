//! Schema system: TypeDef, FieldDef, StructureDef, SectionDef.
//!
//! Schemas are loaded from `.typedown/` directories containing YAML type files.
//! Every schema feature is expressible in YAML -- no built-in-only knobs.

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Schema directory name.
pub const SCHEMA_DIR: &str = ".typedown";

/// Resolve the XDG presets directory (`$XDG_CONFIG_HOME/typedown/presets/`).
///
/// Falls back to `~/.config/typedown/presets/` when `XDG_CONFIG_HOME` is unset.
/// Returns `None` if the directory doesn't exist.
pub fn presets_dir() -> Option<PathBuf> {
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        });
    presets_dir_under(&config_home)
}

/// Return the presets directory under a given config home, if it exists.
fn presets_dir_under(config_home: &Path) -> Option<PathBuf> {
    let dir = config_home.join("typedown").join("presets");
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

/// Built-in presets embedded at compile time from `presets/*.yaml`.
const BUILTIN_PRESETS: &[(&str, &str)] = &[
    ("agent", include_str!("../presets/agent.yaml")),
    ("agents", include_str!("../presets/agents.yaml")),
    ("command", include_str!("../presets/command.yaml")),
    ("journal", include_str!("../presets/journal.yaml")),
    ("readme", include_str!("../presets/readme.yaml")),
    ("skill", include_str!("../presets/skill.yaml")),
    ("roadmap", include_str!("../presets/roadmap.yaml")),
];

/// Load built-in presets, then overlay any XDG presets on top.
///
/// Built-in presets ship with the binary. XDG presets (`~/.config/typedown/presets/`)
/// override built-ins by type name — a local `readme.yaml` replaces the built-in one.
pub fn load_presets() -> Option<Schema> {
    let mut schema = Schema::default();

    // 1. Load built-ins
    for (name, content) in BUILTIN_PRESETS {
        if let Ok(type_def) = serde_yaml::from_str::<TypeDef>(content) {
            schema.types.insert((*name).to_string(), type_def);
        }
    }

    // 2. Overlay XDG presets (override by type name)
    if let Some(dir) = presets_dir() {
        if let Ok(xdg) = Schema::load(&dir) {
            for (name, type_def) in xdg.types {
                schema.types.insert(name, type_def);
            }
        }
    }

    if schema.types.is_empty() {
        None
    } else {
        Some(schema)
    }
}

/// A schema: a collection of named type definitions loaded from a `.typedown/` dir.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub types: IndexMap<String, TypeDef>,
}

impl Schema {
    /// Load a schema from a `.typedown/` directory.
    ///
    /// Each `{type}.yaml` or `{type}.yml` file defines one document type.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut schema = Schema::default();

        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("failed to read schema dir: {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_none_or(|e| e != "yaml" && e != "yml") {
                continue;
            }

            let Some(type_name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };

            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read: {}", path.display()))?;
            let type_def: TypeDef = serde_yaml::from_str(&content)
                .with_context(|| format!("failed to parse: {}", path.display()))?;

            type_def
                .validate(type_name)
                .with_context(|| format!("invalid schema: {}", path.display()))?;

            schema.types.insert(type_name.to_string(), type_def);
        }

        Ok(schema)
    }

    /// Get a type definition by name.
    pub fn get_type(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name)
    }

    /// Build a [`PathMatcher`] from all `paths` patterns across loaded types.
    ///
    /// Fails if any glob pattern is invalid or if two types share an exact
    /// duplicate pattern (runtime overlap detection happens at match time).
    pub fn build_path_matcher(&self) -> Result<PathMatcher> {
        let mut builder = GlobSetBuilder::new();
        let mut pattern_owners: Vec<String> = Vec::new();
        let mut seen_patterns: IndexMap<String, String> = IndexMap::new(); // pattern → type_name

        for (type_name, type_def) in &self.types {
            for pattern in &type_def.paths {
                // Exact duplicate detection at load time
                if let Some(prev_type) = seen_patterns.get(pattern) {
                    anyhow::bail!(
                        "duplicate path pattern '{pattern}': claimed by both '{prev_type}' and '{type_name}'"
                    );
                }
                seen_patterns.insert(pattern.clone(), type_name.clone());

                let glob = GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .with_context(|| format!("invalid glob in type '{type_name}': {pattern}"))?;
                builder.add(glob);
                pattern_owners.push(type_name.clone());
            }
        }

        let glob_set = builder
            .build()
            .context("failed to compile path patterns into GlobSet")?;

        Ok(PathMatcher {
            glob_set,
            pattern_owners,
        })
    }
}

/// Compiled path-pattern matcher built from all schemas' `paths` fields.
///
/// Maps file paths (relative to the schema root) to type names.  When multiple
/// patterns match the same file, the caller treats it as a conflict diagnostic.
#[derive(Debug)]
pub struct PathMatcher {
    glob_set: GlobSet,
    /// For each pattern in the GlobSet (by index), the type name that owns it.
    pattern_owners: Vec<String>,
}

impl PathMatcher {
    /// Match a file path and return the matching type name(s).
    ///
    /// The path should be relative to the `.typedown/` parent directory.
    /// Returns an empty vec if nothing matches.  Returns multiple entries if
    /// patterns from different types both match (a conflict).
    pub fn match_path(&self, relative_path: &str) -> Vec<&str> {
        let matches = self.glob_set.matches(relative_path);
        let mut types: Vec<&str> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for idx in matches {
            let type_name = self.pattern_owners[idx].as_str();
            if seen.insert(type_name) {
                types.push(type_name);
            }
        }
        types
    }

    /// Returns `true` if the matcher has no patterns at all.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.pattern_owners.is_empty()
    }
}

/// Definition of a document type.
#[derive(Debug, Clone, Deserialize)]
pub struct TypeDef {
    /// Glob patterns for files this schema applies to (relative to project root).
    ///
    /// When a file matches a pattern and has no `type:` in frontmatter, this
    /// schema is used automatically.  Supports `*`, `**`, `?` via `globset`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Frontmatter field definitions (order matters for serialization).
    #[serde(default)]
    pub fields: IndexMap<String, FieldDef>,
    /// Document structure rules.
    #[serde(default)]
    pub structure: StructureDef,
}

impl TypeDef {
    /// Validate that the type definition is internally consistent.
    pub fn validate(&self, type_name: &str) -> Result<()> {
        for (field_name, field_def) in &self.fields {
            field_def
                .validate(field_name)
                .with_context(|| format!("in type '{type_name}'"))?;
        }
        Ok(())
    }
}

/// Document structure rules.
///
/// All fields are settable from YAML -- no built-in-only knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructureDef {
    /// H1 title validation mode (default: `none`).
    #[serde(default)]
    pub title: TitleMode,
    /// Content between H1 and the first H2.
    #[serde(default)]
    pub intro: Option<SectionDef>,
    /// Section definitions.
    #[serde(default)]
    pub sections: Vec<SectionDef>,
    /// Whether sections form a strict ordered allowlist (default: `true`).
    /// When `false`, unlisted sections are allowed and ordering is not enforced.
    #[serde(default = "default_true")]
    pub strict_sections: bool,
    /// Emit a warning if the file exceeds this many bytes.
    #[serde(default)]
    pub size_warning: Option<usize>,
    /// Date-based heading validation (e.g. journal entries).
    ///
    /// When set, every H2 is expected to be a date (`YYYY-MM-DD` or
    /// `YYYY-MM-DD HH:MM`) rather than a named section.  Mutually exclusive
    /// with `sections`.
    #[serde(default)]
    pub date_headings: Option<DateHeadingsDef>,
}

fn default_true() -> bool {
    true
}

impl Default for StructureDef {
    fn default() -> Self {
        Self {
            title: TitleMode::None,
            intro: None,
            sections: Vec::new(),
            strict_sections: true,
            size_warning: None,
            date_headings: None,
        }
    }
}

/// H1 title validation mode.
///
/// Deserialized from a YAML string:
/// - `"none"` → no validation
/// - `"from_filename"` → H1 must match the filename (without `.md`)
/// - `"from_directory"` → H1 must match the immediate parent directory name
/// - `"from_project"` → H1 must match the project name or frontmatter `name`
/// - `"from_date"` → H1 is derived from the filename parsed as `YYYY-MM` (e.g. `"February 2026"`)
/// - `"required"` → H1 must exist (unfixable if missing)
/// - anything else → `Fixed("…")`: H1 auto-created with that text if missing
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TitleMode {
    #[default]
    None,
    FromFilename,
    /// H1 must match the name of the file's immediate parent directory.
    ///
    /// `typedown/README.md` → `# typedown`.
    FromDirectory,
    FromProject,
    /// Derive H1 from the filename parsed as a `YYYY-MM` date.
    ///
    /// `2026-02.md` → `# February 2026`.  Also implies that each
    /// `date_headings` entry's `YYYY-MM` prefix must match the filename.
    FromDate,
    Fixed(String),
    RequiredAny,
}

impl<'de> Deserialize<'de> for TitleMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "none" => TitleMode::None,
            "from_filename" => TitleMode::FromFilename,
            "from_directory" => TitleMode::FromDirectory,
            "from_project" => TitleMode::FromProject,
            "from_date" => TitleMode::FromDate,
            "required" => TitleMode::RequiredAny,
            other => TitleMode::Fixed(other.to_string()),
        })
    }
}

// ── Date headings ─────────────────────────────────────────────────────────────

/// Date-based heading validation for documents where H2s are dates (journals,
/// changelogs, meeting notes) rather than fixed named sections.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DateHeadingsDef {
    /// Sort order for date entries (default: `newest_first`).
    #[serde(default)]
    pub sort: HeadingSort,
}

/// Sort order for date headings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadingSort {
    /// Most recent entry first (reverse chronological). Default.
    #[default]
    NewestFirst,
    /// Oldest entry first (chronological, e.g. changelogs).
    OldestFirst,
}

/// Bullet-list mode for a section.
///
/// Deserialized from YAML: `any`, `ordered`, `unordered`, or `true` (→ `Any`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulletMode {
    /// Any list type (ordered or unordered).
    Any,
    /// Only ordered (numbered) lists.
    Ordered,
    /// Only unordered (dash/bullet) lists.
    Unordered,
}

impl<'de> Deserialize<'de> for BulletMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;

        struct BulletModeVisitor;

        impl<'de> de::Visitor<'de> for BulletModeVisitor {
            type Value = BulletMode;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("\"any\", \"ordered\", \"unordered\", or true")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                if v {
                    Ok(BulletMode::Any)
                } else {
                    Err(E::custom(
                        "bullets: false is not valid; omit the field instead",
                    ))
                }
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "any" => Ok(BulletMode::Any),
                    "ordered" => Ok(BulletMode::Ordered),
                    "unordered" => Ok(BulletMode::Unordered),
                    other => Err(E::custom(format!(
                        "unknown bullet mode '{other}'; expected any, ordered, or unordered"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(BulletModeVisitor)
    }
}

/// Definition of a document section.
///
/// All fields are settable from YAML.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SectionDef {
    /// Section heading text (not used for `intro`).
    #[serde(default)]
    pub title: String,
    /// Human-readable description. Ignored by the engine; useful for LLM guidance.
    #[serde(default)]
    #[allow(dead_code)]
    pub description: Option<String>,
    /// Restrict content to bullet lists only (default: `None` → any content allowed).
    ///
    /// `any` — any list type; `ordered` — numbered lists only; `unordered` — dash lists only.
    /// `true` is accepted as shorthand for `any`.
    #[serde(default)]
    pub bullets: Option<BulletMode>,
    /// Whether this section is required.
    #[serde(default)]
    pub required: bool,
    /// Template showing expected format (for LLM reference and validation).
    #[serde(default)]
    pub template: Option<String>,
    /// Link constraints for this section.
    #[serde(default)]
    pub links: Option<LinksDef>,
    /// Auto-managed section content (template + legacy migration).
    #[serde(default)]
    pub managed_content: Option<ManagedContent>,
    /// Required intro paragraph prefix; auto-inserted if missing.
    #[serde(default)]
    pub intro_text: Option<String>,
    /// Property map for top-level list items: each item's sub-items are parsed
    /// as `Key: Value` pairs, validated against these field definitions, and
    /// extracted into a `properties` object in `td json` output.
    #[serde(default)]
    pub properties: Option<IndexMap<String, FieldDef>>,
}

impl SectionDef {
    /// Whether this section enforces bullets-only content.
    ///
    /// True when `bullets` is explicitly set, or when a `template` is present
    /// (templates describe bullet item formats, so they imply bullet mode).
    #[allow(dead_code)]
    pub fn is_bullets_mode(&self) -> bool {
        self.bullets.is_some() || self.template.is_some()
    }

    /// The effective bullet mode, resolving template-implied defaults.
    ///
    /// - Explicit `bullets` takes precedence.
    /// - A `template` without `bullets` defaults to `Unordered`.
    /// - Neither returns `None`.
    pub fn effective_bullet_mode(&self) -> Option<BulletMode> {
        self.bullets.or_else(|| {
            if self.template.is_some() {
                Some(BulletMode::Unordered)
            } else {
                None
            }
        })
    }
}

/// Link constraints for a section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LinksDef {
    /// Links in this section must point to files of this schema type.
    #[serde(default)]
    pub target_type: Option<String>,
    /// Whether links must be bidirectional (target must link back).
    #[serde(default)]
    pub bidirectional: bool,
}

/// Typed frontmatter field definition.
#[derive(Debug, Clone, Deserialize)]
pub struct FieldDef {
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    /// Valid values for `enum` fields.
    #[serde(default)]
    pub values: Option<Vec<String>>,
    /// Item type for `list` fields.
    #[serde(default)]
    pub item_type: Option<FieldType>,
}

impl FieldDef {
    /// Validate that the field definition is internally consistent.
    pub fn validate(&self, field_name: &str) -> Result<()> {
        match self.field_type {
            FieldType::Enum => {
                if self.values.as_ref().is_none_or(|v| v.is_empty()) {
                    anyhow::bail!("field '{field_name}': enum type requires non-empty 'values'");
                }
            }
            FieldType::List => {
                if self.item_type == Some(FieldType::Enum)
                    && self.values.as_ref().is_none_or(|v| v.is_empty())
                {
                    anyhow::bail!("field '{field_name}': list of enum requires non-empty 'values'");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Frontmatter field types.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Date,
    Datetime,
    Integer,
    Bool,
    Enum,
    Link,
    List,
}

/// Auto-managed section content.
///
/// When set, the validator checks the section matches the template and
/// auto-fixes it on `td fmt`. Custom content appended after the template
/// is preserved.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ManagedContent {
    /// Markdown template the section must match.
    pub template: String,
    /// Legacy section titles to migrate away from.
    #[serde(default)]
    pub migrate_from: Vec<String>,
}

// ── template matching ─────────────────────────────────────────────────────────

/// A parsed segment of a section template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateSegment {
    /// Literal text that must match exactly.
    Literal(String),
    /// A markdown link `[text](url)`.
    Link,
    /// Bold text `**...**`.
    Bold,
    /// A date `YYYY-MM-DD`.
    Date,
    /// Free text (matches any characters).
    Text,
}

/// Parse a template string into segments for [`matches_template`].
///
/// Recognises:
/// - `[...](...)`  → `Link`
/// - `**...**`     → `Bold`
/// - `YYYY-MM-DD` or actual date → `Date`
/// - Common separators (` - `, `, `, etc.) → `Literal`
/// - Other spans → `Text`
pub fn parse_template(template: &str) -> Vec<TemplateSegment> {
    let mut segments = Vec::new();
    let mut remaining = template;

    while !remaining.is_empty() {
        if remaining.starts_with('[') {
            if let Some(end) = find_link_end(remaining) {
                segments.push(TemplateSegment::Link);
                remaining = &remaining[end..];
                continue;
            }
        }

        if let Some(end) = match_bold_end(remaining) {
            segments.push(TemplateSegment::Bold);
            remaining = &remaining[end..];
            continue;
        }

        if let Some(end) = match_date_pattern(remaining) {
            segments.push(TemplateSegment::Date);
            remaining = &remaining[end..];
            continue;
        }

        if let Some(end) = match_separator(remaining) {
            segments.push(TemplateSegment::Literal(remaining[..end].to_string()));
            remaining = &remaining[end..];
            continue;
        }

        let end = find_text_end(remaining);
        if end > 0 {
            segments.push(TemplateSegment::Text);
            remaining = &remaining[end..];
        } else {
            let c = remaining.chars().next().unwrap_or(' ');
            segments.push(TemplateSegment::Literal(c.to_string()));
            remaining = &remaining[c.len_utf8()..];
        }
    }

    segments
}

/// Check whether `s` matches the given template segments.
pub fn matches_template(s: &str, segments: &[TemplateSegment]) -> bool {
    matches_recursive(s, segments)
}

fn matches_recursive(s: &str, segments: &[TemplateSegment]) -> bool {
    if segments.is_empty() {
        return s.trim().is_empty();
    }

    match &segments[0] {
        TemplateSegment::Literal(lit) => {
            s.starts_with(lit.as_str()) && matches_recursive(&s[lit.len()..], &segments[1..])
        }
        TemplateSegment::Link => {
            find_link_end(s).is_some_and(|end| matches_recursive(&s[end..], &segments[1..]))
        }
        TemplateSegment::Bold => {
            match_bold_end(s).is_some_and(|end| matches_recursive(&s[end..], &segments[1..]))
        }
        TemplateSegment::Date => {
            match_date_pattern(s).is_some_and(|end| matches_recursive(&s[end..], &segments[1..]))
        }
        TemplateSegment::Text => {
            // Iterate only over valid char boundaries to avoid panicking on
            // multi-byte characters (e.g. em dash is 3 bytes in UTF-8).
            std::iter::once(0)
                .chain(s.char_indices().map(|(i, c)| i + c.len_utf8()))
                .any(|end| matches_recursive(&s[end..], &segments[1..]))
        }
    }
}

fn find_link_end(s: &str) -> Option<usize> {
    if !s.starts_with('[') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_url = false;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '[' if !in_url => depth += 1,
            ']' if !in_url => {
                depth -= 1;
                if depth == 0 {
                    match chars.next() {
                        Some((_, '(')) => in_url = true,
                        _ => return None,
                    }
                }
            }
            ')' if in_url => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// Find the end of a bold span `**...**`, returning the byte offset past the
/// closing `**`.  Returns `None` if `s` doesn't start with `**` or has no
/// closing pair.
fn match_bold_end(s: &str) -> Option<usize> {
    if !s.starts_with("**") {
        return None;
    }
    // Find closing ** after the opening one.
    let inner = &s[2..];
    let close = inner.find("**")?;
    if close == 0 {
        return None; // empty bold `****` is not valid
    }
    Some(2 + close + 2) // opening ** + inner + closing **
}

fn match_date_pattern(s: &str) -> Option<usize> {
    if s.starts_with("YYYY-MM-DD") {
        return Some(10);
    }
    if s.len() >= 10 {
        let b = s.as_bytes();
        if b[0].is_ascii_digit()
            && b[1].is_ascii_digit()
            && b[2].is_ascii_digit()
            && b[3].is_ascii_digit()
            && b[4] == b'-'
            && b[5].is_ascii_digit()
            && b[6].is_ascii_digit()
            && b[7] == b'-'
            && b[8].is_ascii_digit()
            && b[9].is_ascii_digit()
        {
            return Some(10);
        }
    }
    None
}

fn match_separator(s: &str) -> Option<usize> {
    for sep in [
        " - ", "- ", ", ", "; ", ": ", " (", ") ", "(", ")", " – ", "–",
    ] {
        if s.starts_with(sep) {
            return Some(sep.len());
        }
    }
    None
}

fn find_text_end(s: &str) -> usize {
    for (i, c) in s.char_indices() {
        if c == '[' {
            return i;
        }
        if c == '*' && s[i..].starts_with("**") && match_bold_end(&s[i..]).is_some() {
            return i;
        }
        if matches!(c, '-' | ',' | ';' | ':' | '(' | ')') && match_separator(&s[i..]).is_some() {
            return i;
        }
        if c.is_ascii_digit() && match_date_pattern(&s[i..]).is_some() {
            return i;
        }
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── TypeDef deserialization ───────────────────────────────────────────────

    #[test]
    fn test_parse_basic_type() {
        let yaml = r#"
description: A recipe document
fields:
  servings:
    type: integer
    required: true
  cuisine:
    type: enum
    values: [italian, mexican, japanese]
  source:
    type: link
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.fields.len(), 3);
        assert_eq!(td.fields["servings"].field_type, FieldType::Integer);
        assert!(td.fields["servings"].required);
        assert_eq!(td.fields["cuisine"].field_type, FieldType::Enum);
        assert_eq!(td.fields["source"].field_type, FieldType::Link);
    }

    #[test]
    fn test_field_order_preserved() {
        let yaml = r#"
fields:
  zebra:
    type: string
  alpha:
    type: string
  middle:
    type: string
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let keys: Vec<&str> = td.fields.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, ["zebra", "alpha", "middle"]);
    }

    #[test]
    fn test_all_field_types() {
        let yaml = r#"
fields:
  a: { type: string }
  b: { type: date }
  c: { type: datetime }
  d: { type: integer }
  e: { type: bool }
  f: { type: enum, values: [x] }
  g: { type: link }
  h: { type: list, item_type: string }
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.fields["a"].field_type, FieldType::String);
        assert_eq!(td.fields["b"].field_type, FieldType::Date);
        assert_eq!(td.fields["c"].field_type, FieldType::Datetime);
        assert_eq!(td.fields["d"].field_type, FieldType::Integer);
        assert_eq!(td.fields["e"].field_type, FieldType::Bool);
        assert_eq!(td.fields["f"].field_type, FieldType::Enum);
        assert_eq!(td.fields["g"].field_type, FieldType::Link);
        assert_eq!(td.fields["h"].field_type, FieldType::List);
        assert_eq!(td.fields["h"].item_type, Some(FieldType::String));
    }

    // ── FieldDef validation ───────────────────────────────────────────────────

    #[test]
    fn test_enum_without_values_fails() {
        let yaml = "fields:\n  status:\n    type: enum\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.validate("test").is_err());
    }

    #[test]
    fn test_enum_with_empty_values_fails() {
        let yaml = "fields:\n  status:\n    type: enum\n    values: []\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.validate("test").is_err());
    }

    #[test]
    fn test_enum_with_values_ok() {
        let yaml = "fields:\n  status:\n    type: enum\n    values: [a, b]\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.validate("test").is_ok());
    }

    #[test]
    fn test_list_of_enum_without_values_fails() {
        let yaml = "fields:\n  tags:\n    type: list\n    item_type: enum\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.validate("test").is_err());
    }

    #[test]
    fn test_list_of_string_ok() {
        let yaml = "fields:\n  names:\n    type: list\n    item_type: string\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.validate("test").is_ok());
    }

    // ── StructureDef ─────────────────────────────────────────────────────────

    #[test]
    fn test_structure_defaults() {
        let yaml = "description: foo\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::None);
        assert!(td.structure.strict_sections);
        assert!(td.structure.size_warning.is_none());
        assert!(td.structure.sections.is_empty());
    }

    #[test]
    fn test_structure_all_fields_from_yaml() {
        let yaml = r#"
fields:
  category:
    type: string
    required: true
structure:
  title: from_filename
  strict_sections: false
  size_warning: 4000
  sections:
    - title: Notes
      required: false
      bullets: any
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::FromFilename);
        assert!(!td.structure.strict_sections);
        assert_eq!(td.structure.size_warning, Some(4000));
        assert_eq!(td.fields.len(), 1);
        assert!(td.fields["category"].required);
        assert_eq!(td.structure.sections.len(), 1);
        assert_eq!(td.structure.sections[0].bullets, Some(BulletMode::Any));
    }

    #[test]
    fn test_managed_content_from_yaml() {
        let yaml = r#"
structure:
  sections:
    - title: Related Documents
      required: true
      managed_content:
        template: |
          ## Related Documents

          - **journal/** - daily notes
        migrate_from:
          - Journal
          - Roadmap
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let sec = &td.structure.sections[0];
        assert_eq!(sec.title, "Related Documents");
        let mc = sec.managed_content.as_ref().unwrap();
        assert!(mc.template.contains("## Related Documents"));
        assert_eq!(mc.migrate_from, ["Journal", "Roadmap"]);
    }

    #[test]
    fn test_intro_text_from_yaml() {
        let yaml = r#"
structure:
  sections:
    - title: Non-Goals
      required: true
      intro_text: "Explicitly out of scope to keep the project focused:"
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let sec = &td.structure.sections[0];
        assert_eq!(
            sec.intro_text.as_deref(),
            Some("Explicitly out of scope to keep the project focused:")
        );
    }

    // ── BulletMode deserialization ────────────────────────────────────────────

    #[test]
    fn test_bullet_mode_string_values() {
        let yaml = "structure:\n  sections:\n    - title: A\n      bullets: any\n    - title: B\n      bullets: ordered\n    - title: C\n      bullets: unordered\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.sections[0].bullets, Some(BulletMode::Any));
        assert_eq!(td.structure.sections[1].bullets, Some(BulletMode::Ordered));
        assert_eq!(
            td.structure.sections[2].bullets,
            Some(BulletMode::Unordered)
        );
    }

    #[test]
    fn test_bullet_mode_true_is_any() {
        let yaml = "structure:\n  sections:\n    - title: A\n      bullets: true\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.sections[0].bullets, Some(BulletMode::Any));
    }

    #[test]
    fn test_bullet_mode_false_rejected() {
        let yaml = "structure:\n  sections:\n    - title: A\n      bullets: false\n";
        let result: Result<TypeDef, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "bullets: false should be rejected");
    }

    #[test]
    fn test_bullet_mode_omitted_is_none() {
        let yaml = "structure:\n  sections:\n    - title: A\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.sections[0].bullets, None);
    }

    #[test]
    fn test_is_bullets_mode_template_implies_bullets() {
        let yaml = "structure:\n  sections:\n    - title: A\n      template: '- **Text**: Text'\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.structure.sections[0].is_bullets_mode());
        assert_eq!(
            td.structure.sections[0].effective_bullet_mode(),
            Some(BulletMode::Unordered)
        );
    }

    #[test]
    fn test_is_bullets_mode_explicit_overrides_template_default() {
        let yaml = "structure:\n  sections:\n    - title: A\n      bullets: ordered\n      template: '- **Text**: Text'\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            td.structure.sections[0].effective_bullet_mode(),
            Some(BulletMode::Ordered)
        );
    }

    // ── TitleMode ─────────────────────────────────────────────────────────────

    #[test]
    fn test_title_mode_none() {
        let yaml = "structure:\n  title: none\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::None);
    }

    #[test]
    fn test_title_mode_from_filename() {
        let yaml = "structure:\n  title: from_filename\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::FromFilename);
    }

    #[test]
    fn test_title_mode_from_project() {
        let yaml = "structure:\n  title: from_project\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::FromProject);
    }

    #[test]
    fn test_title_mode_required() {
        let yaml = "structure:\n  title: required\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::RequiredAny);
    }

    #[test]
    fn test_title_mode_fixed() {
        let yaml = "structure:\n  title: \"My Project Roadmap\"\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            td.structure.title,
            TitleMode::Fixed("My Project Roadmap".to_string())
        );
    }

    // ── Schema::load ─────────────────────────────────────────────────────────

    #[test]
    fn test_schema_load_from_dir() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(".typedown");
        fs::create_dir(&schema_dir).unwrap();

        fs::write(
            schema_dir.join("recipe.yaml"),
            "description: A recipe\nfields:\n  servings:\n    type: integer\n",
        )
        .unwrap();
        fs::write(
            schema_dir.join("note.yaml"),
            "description: A note\nfields:\n  tags:\n    type: list\n    item_type: string\n",
        )
        .unwrap();

        let schema = Schema::load(&schema_dir).unwrap();
        assert_eq!(schema.types.len(), 2);
        assert!(schema.get_type("recipe").is_some());
        assert!(schema.get_type("note").is_some());
        assert!(schema.get_type("nonexistent").is_none());
    }

    #[test]
    fn test_schema_load_ignores_non_yaml() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(".typedown");
        fs::create_dir(&schema_dir).unwrap();

        fs::write(schema_dir.join("recipe.yaml"), "description: A recipe\n").unwrap();
        fs::write(schema_dir.join("README.md"), "# Schemas\n").unwrap();
        fs::write(schema_dir.join("notes.txt"), "some notes\n").unwrap();

        let schema = Schema::load(&schema_dir).unwrap();
        assert_eq!(schema.types.len(), 1);
    }

    #[test]
    fn test_schema_load_invalid_yaml_errors() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(".typedown");
        fs::create_dir(&schema_dir).unwrap();

        fs::write(schema_dir.join("bad.yaml"), "fields: [\ninvalid yaml").unwrap();

        assert!(Schema::load(&schema_dir).is_err());
    }

    #[test]
    fn test_schema_load_invalid_type_def_errors() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(".typedown");
        fs::create_dir(&schema_dir).unwrap();

        // enum without values fails validate()
        fs::write(
            schema_dir.join("broken.yaml"),
            "fields:\n  status:\n    type: enum\n",
        )
        .unwrap();

        assert!(Schema::load(&schema_dir).is_err());
    }

    // ── template matching ─────────────────────────────────────────────────────

    #[test]
    fn test_matches_template_literal() {
        let segs = parse_template("Hello, world");
        assert!(matches_template("Hello, world", &segs));
        assert!(!matches_template("Hello world", &segs));
    }

    #[test]
    fn test_matches_template_link() {
        let segs = parse_template("[text](url)");
        assert!(matches_template("[click here](https://example.com)", &segs));
        assert!(!matches_template("not a link", &segs));
    }

    #[test]
    fn test_matches_template_date() {
        let segs = parse_template("YYYY-MM-DD");
        assert!(matches_template("2024-03-15", &segs));
        assert!(!matches_template("15-03-2024", &segs));
    }

    #[test]
    fn test_matches_template_text() {
        let segs = parse_template("[link](url) - Text");
        assert!(matches_template("[foo](bar) - anything goes here", &segs));
    }

    #[test]
    fn test_matches_template_text_multibyte() {
        // Text wildcard must not panic on 3-byte UTF-8 chars (e.g. em dash U+2014).
        let segs = parse_template("- Text");
        assert!(matches_template("- em\u{2014}dash", &segs));
        assert!(matches_template("- \u{2014}", &segs));
        assert!(!matches_template("no prefix", &segs));

        // Wildcard at the end: any suffix including multi-byte chars.
        let segs2 = parse_template("prefix Text");
        assert!(matches_template("prefix \u{2014}emdash\u{2014}", &segs2));
    }

    #[test]
    fn test_matches_template_bold() {
        let segs = parse_template("- **Text** - Text");
        assert!(matches_template(
            "- **Field types** - string, date, integer",
            &segs
        ));
        assert!(matches_template(
            "- **LSP** - diagnostics on open and change",
            &segs
        ));
        assert!(!matches_template("- no bold here - something", &segs));
        assert!(!matches_template("- *italic* - not bold", &segs));
    }

    #[test]
    fn test_matches_template_bold_no_separator() {
        let segs = parse_template("**Text**");
        assert!(matches_template("**hello**", &segs));
        assert!(!matches_template("hello", &segs));
        assert!(!matches_template("****", &segs)); // empty bold
    }

    #[test]
    fn test_parse_template_bold_segments() {
        let segs = parse_template("- **Text** - Text");
        assert_eq!(
            segs,
            vec![
                TemplateSegment::Literal("- ".to_string()),
                TemplateSegment::Bold,
                TemplateSegment::Literal(" - ".to_string()),
                TemplateSegment::Text,
            ]
        );
    }

    #[test]
    fn test_parse_template_segments() {
        let segs = parse_template("[text](url) - YYYY-MM-DD");
        assert!(segs.contains(&TemplateSegment::Link));
        assert!(segs.contains(&TemplateSegment::Date));
        assert!(segs
            .iter()
            .any(|s| matches!(s, TemplateSegment::Literal(_))));
    }

    // ── paths field deserialization ───────────────────────────────────────────

    #[test]
    fn test_paths_field_deserializes() {
        let yaml = r#"
paths:
  - "**/*.md"
  - ".claude/commands/*.md"
structure:
  title: required
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.paths, vec!["**/*.md", ".claude/commands/*.md"]);
    }

    #[test]
    fn test_paths_defaults_to_empty() {
        let yaml = "description: no paths\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.paths.is_empty());
    }

    // ── PathMatcher ──────────────────────────────────────────────────────────

    #[test]
    fn test_path_matcher_single_pattern() {
        let yaml = "paths:\n  - \"**/AGENTS.md\"\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let mut schema = Schema::default();
        schema.types.insert("agents".to_string(), td);

        let matcher = schema.build_path_matcher().unwrap();
        assert_eq!(matcher.match_path("AGENTS.md"), vec!["agents"]);
        assert_eq!(matcher.match_path("sub/AGENTS.md"), vec!["agents"]);
        assert_eq!(matcher.match_path("deep/sub/AGENTS.md"), vec!["agents"]);
        assert!(matcher.match_path("README.md").is_empty());
    }

    #[test]
    fn test_path_matcher_star_glob() {
        let yaml = "paths:\n  - \"journal/*.md\"\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let mut schema = Schema::default();
        schema.types.insert("journal".to_string(), td);

        let matcher = schema.build_path_matcher().unwrap();
        assert_eq!(matcher.match_path("journal/2026-02.md"), vec!["journal"]);
        // Single * does not match nested dirs
        assert!(matcher.match_path("journal/sub/2026-02.md").is_empty());
        assert!(matcher.match_path("other/2026-02.md").is_empty());
    }

    #[test]
    fn test_path_matcher_multiple_types() {
        let mut schema = Schema::default();
        let agents: TypeDef = serde_yaml::from_str("paths:\n  - \"**/AGENTS.md\"\n").unwrap();
        let readme: TypeDef = serde_yaml::from_str("paths:\n  - \"**/README.md\"\n").unwrap();
        schema.types.insert("agents".to_string(), agents);
        schema.types.insert("readme".to_string(), readme);

        let matcher = schema.build_path_matcher().unwrap();
        assert_eq!(matcher.match_path("AGENTS.md"), vec!["agents"]);
        assert_eq!(matcher.match_path("README.md"), vec!["readme"]);
        assert!(matcher.match_path("other.md").is_empty());
    }

    #[test]
    fn test_path_matcher_conflict_detection() {
        // Two types with overlapping patterns -- both match the same file
        let mut schema = Schema::default();
        let a: TypeDef = serde_yaml::from_str("paths:\n  - \"**/*.md\"\n").unwrap();
        let b: TypeDef = serde_yaml::from_str("paths:\n  - \"docs/*.md\"\n").unwrap();
        schema.types.insert("a".to_string(), a);
        schema.types.insert("b".to_string(), b);

        let matcher = schema.build_path_matcher().unwrap();
        let matched = matcher.match_path("docs/hello.md");
        assert_eq!(matched.len(), 2, "should detect overlap: {matched:?}");
    }

    #[test]
    fn test_path_matcher_exact_duplicate_rejected() {
        // Exact same pattern in two types -- rejected at build time
        let mut schema = Schema::default();
        let a: TypeDef = serde_yaml::from_str("paths:\n  - \"**/README.md\"\n").unwrap();
        let b: TypeDef = serde_yaml::from_str("paths:\n  - \"**/README.md\"\n").unwrap();
        schema.types.insert("a".to_string(), a);
        schema.types.insert("b".to_string(), b);

        let result = schema.build_path_matcher();
        assert!(
            result.is_err(),
            "duplicate patterns should fail at build time"
        );
    }

    #[test]
    fn test_path_matcher_multiple_patterns_per_type() {
        let yaml = "paths:\n  - \".claude/commands/*.md\"\n  - \".opencode/commands/*.md\"\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let mut schema = Schema::default();
        schema.types.insert("command".to_string(), td);

        let matcher = schema.build_path_matcher().unwrap();
        assert_eq!(
            matcher.match_path(".claude/commands/review.md"),
            vec!["command"]
        );
        assert_eq!(
            matcher.match_path(".opencode/commands/review.md"),
            vec!["command"]
        );
        assert!(matcher.match_path("commands/review.md").is_empty());
    }

    #[test]
    fn test_path_matcher_empty_when_no_paths() {
        let td: TypeDef = serde_yaml::from_str("description: no paths\n").unwrap();
        let mut schema = Schema::default();
        schema.types.insert("plain".to_string(), td);

        let matcher = schema.build_path_matcher().unwrap();
        assert!(matcher.is_empty());
        assert!(matcher.match_path("anything.md").is_empty());
    }

    #[test]
    fn test_schema_load_with_paths() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(".typedown");
        fs::create_dir(&schema_dir).unwrap();

        fs::write(
            schema_dir.join("command.yaml"),
            "paths:\n  - \".claude/commands/*.md\"\nstructure:\n  title: required\n",
        )
        .unwrap();

        let schema = Schema::load(&schema_dir).unwrap();
        let command = schema.get_type("command").unwrap();
        assert_eq!(command.paths, vec![".claude/commands/*.md"]);

        let matcher = schema.build_path_matcher().unwrap();
        assert_eq!(
            matcher.match_path(".claude/commands/test.md"),
            vec!["command"]
        );
    }

    // ── presets_dir ───────────────────────────────────────────────────────────

    #[test]
    fn test_presets_dir_respects_xdg_config_home() {
        let dir = TempDir::new().unwrap();
        let presets = dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();

        let result = presets_dir_under(dir.path());
        assert_eq!(result, Some(presets));
    }

    #[test]
    fn test_presets_dir_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let result = presets_dir_under(dir.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_load_presets_from_xdg() {
        let dir = TempDir::new().unwrap();
        let presets = dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("readme.yaml"),
            "paths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
        )
        .unwrap();

        let schema = Schema::load(&presets).expect("should load presets");
        assert!(schema.get_type("readme").is_some());
    }
}
