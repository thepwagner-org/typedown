//! Schema system: TypeDef, FieldDef, StructureDef, SectionDef.
//!
//! Schemas are loaded from `.typedown/` directories containing YAML type files.
//! Every schema feature is expressible in YAML -- no built-in-only knobs.

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::correspondence::CorrespondenceRule;

/// Schema directory name.
pub const SCHEMA_DIR: &str = ".typedown";

/// The json-schema that `.typedown/*.yaml` files conform to, baked into the
/// binary. `td schema` prints it, so nothing needs `schema.json` on disk.
pub const META_SCHEMA: &str = include_str!("schema.json");

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
pub const BUILTIN_PRESETS: &[(&str, &str)] = &[
    ("agent", include_str!("../presets/agent.yaml")),
    ("agents", include_str!("../presets/agents.yaml")),
    ("command", include_str!("../presets/command.yaml")),
    ("goals", include_str!("../presets/goals.yaml")),
    ("journal", include_str!("../presets/journal.yaml")),
    (
        "journal-entry",
        include_str!("../presets/journal-entry.yaml"),
    ),
    ("readme", include_str!("../presets/readme.yaml")),
    ("skill", include_str!("../presets/skill.yaml")),
    ("roadmap", include_str!("../presets/roadmap.yaml")),
    ("task", include_str!("../presets/task.yaml")),
];

/// Load built-in presets, then overlay the presets in `dir` on top.
///
/// Built-in presets ship with the binary. Overlay presets (in production,
/// `~/.config/typedown/presets/`) override built-ins by type name — a local
/// `readme.yaml` replaces the built-in one.
///
/// The overlay directory is a parameter rather than something this function
/// resolves from `XDG_CONFIG_HOME`: that variable is process-global, and a test
/// pointing it at its own temp dir would change which presets every other
/// test's `check_dir` loads on a neighbouring thread.
///
/// The second return value is the overlay directory's load error, if it had
/// one, so orchestration can report it.  A preset that fails to load is not a
/// preset that does nothing: it is one whose type falls back to the built-in of
/// the same name, or vanishes, and either way the project's documents are being
/// judged by a schema nobody wrote.  Swallowing that is how a stale schema
/// stays stale.
pub(crate) fn load_presets_from(dir: Option<PathBuf>) -> (Option<Schema>, Option<anyhow::Error>) {
    let mut schema = Schema::default();

    // 1. Load built-ins
    for (name, content) in BUILTIN_PRESETS {
        if let Ok(type_def) = serde_yaml::from_str::<TypeDef>(content) {
            schema.types.insert((*name).to_string(), type_def);
        }
    }

    // 2. Overlay the external presets (override by type name)
    let mut preset_error = None;
    if let Some(dir) = dir {
        match Schema::load(&dir) {
            Ok(xdg) => {
                for (name, type_def) in xdg.types {
                    schema.types.insert(name, type_def);
                }
            }
            Err(e) => preset_error = Some(e),
        }
    }

    let schema = if schema.types.is_empty() {
        None
    } else {
        Some(schema)
    };
    (schema, preset_error)
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

/// Schema format version.
///
/// Only `2` exists: frontmatter is described by a literal json-schema under
/// `frontmatter:`.  Version 1 — the bespoke `fields:` map — was removed;
/// see `MISSING_SCHEMA_VERSION` for what a schema that omits `version:` does.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// The `version` a schema file that declares none deserializes to.
///
/// Zero is not a real version, so it can't collide with one an author wrote.
/// [`TypeDef::validate`] turns it into a load error rather than guessing: an
/// unversioned schema is a v1 leftover, and silently reading one as v2 would
/// drop its whole `fields:` block along with every requirement in it.
const MISSING_SCHEMA_VERSION: u32 = 0;

/// Definition of a document type.
#[derive(Debug, Clone, Deserialize)]
pub struct TypeDef {
    /// Schema format version.  Must be `2`; absent is a load error.
    #[serde(default)]
    pub version: u32,
    /// Glob patterns for files this schema applies to (relative to project root).
    ///
    /// When a file matches a pattern and has no `type:` in frontmatter, this
    /// schema is used automatically.  Supports `*`, `**`, `?` via `globset`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Tombstone for the removed v1 `fields:` map.
    ///
    /// Kept only so a leftover v1 schema fails loudly.  Without it serde would
    /// ignore the unknown key and the type would validate no frontmatter at
    /// all — the same file, quietly enforcing nothing.
    #[serde(default, rename = "fields")]
    legacy_fields: Option<serde_yaml::Value>,
    /// Literal json-schema describing the document's frontmatter.
    ///
    /// The instance validated against it is the frontmatter object with `type:`
    /// included, so the schema can constrain `type` like any other property.
    #[serde(default)]
    pub frontmatter: Option<FrontmatterSchema>,
    /// Document structure rules.
    #[serde(default)]
    pub structure: StructureDef,
    /// Rules tying frontmatter data to body prose, in both directions.
    #[serde(default)]
    pub correspondence: Vec<CorrespondenceRule>,
    /// Lazily compiled validator for [`Self::frontmatter`].
    #[serde(skip)]
    compiled: std::sync::OnceLock<CompiledSchema>,
}

/// A v2 type's literal json-schema for frontmatter.
///
/// Carries the declared order of `properties` alongside the schema itself:
/// `serde_json::Value` sorts object keys, but `td fmt` writes frontmatter in
/// schema order, and the author's YAML order is the intended one.
#[derive(Debug, Clone)]
pub struct FrontmatterSchema {
    /// The json-schema, as written.
    pub schema: serde_json::Value,
    /// `properties` keys in declaration order.
    pub property_order: Vec<String>,
}

impl<'de> Deserialize<'de> for FrontmatterSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let yaml = serde_yaml::Value::deserialize(deserializer)?;
        let property_order = yaml
            .get("properties")
            .and_then(serde_yaml::Value::as_mapping)
            .map(|m| {
                m.keys()
                    .filter_map(|k| k.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let schema = serde_yaml::from_value(yaml).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema,
            property_order,
        })
    }
}

/// A compiled json-schema validator, cheap to clone and `Debug`-opaque.
#[derive(Clone)]
pub struct CompiledSchema(std::sync::Arc<jsonschema::Validator>);

impl std::fmt::Debug for CompiledSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CompiledSchema(..)")
    }
}

impl std::ops::Deref for CompiledSchema {
    type Target = jsonschema::Validator;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Compile a json-schema for frontmatter validation.
///
/// v2 dates are ISO 8601 only: `format: date` is `YYYY-MM-DD`, `format:
/// date-time` is RFC 3339 (`2026-01-02T14:30:00Z`).  The loose spellings v1
/// took (`2026/01/02`, `January 2, 2026`, `2026-01-02 14:30`) are deliberately
/// not accepted — the crate's own format implementations are used as-is.
///
/// Format assertion is switched on: in draft 2020-12 `format` is annotation-only
/// by default, which would silently accept anything.
pub fn compile_frontmatter_schema(schema: &serde_json::Value) -> Result<CompiledSchema> {
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
        .map_err(|e| anyhow::anyhow!("invalid json-schema in 'frontmatter': {e}"))?;
    Ok(CompiledSchema(std::sync::Arc::new(validator)))
}

impl TypeDef {
    /// Validate that the type definition is internally consistent.
    pub fn validate(&self, type_name: &str) -> Result<()> {
        match self.version {
            CURRENT_SCHEMA_VERSION => {}
            MISSING_SCHEMA_VERSION => anyhow::bail!(
                "type '{type_name}': missing 'version: {CURRENT_SCHEMA_VERSION}' — every schema must declare its version"
            ),
            1 => anyhow::bail!(
                "type '{type_name}': schema version 1 ('fields:') is no longer supported — describe frontmatter with the json-schema under 'frontmatter:' and declare 'version: {CURRENT_SCHEMA_VERSION}'"
            ),
            other => anyhow::bail!(
                "type '{type_name}': unsupported schema version {other} (supported: {CURRENT_SCHEMA_VERSION})"
            ),
        }

        if self.legacy_fields.is_some() {
            anyhow::bail!(
                "type '{type_name}': 'fields:' is the removed version 1 spelling — describe frontmatter with the json-schema under 'frontmatter:'"
            );
        }

        self.frontmatter_validator()
            .with_context(|| format!("in type '{type_name}'"))?;

        for section in self.structure.intro.iter().chain(&self.structure.sections) {
            for (field_name, field_def) in section.properties.iter().flatten() {
                field_def.validate(field_name).with_context(|| {
                    format!("in type '{type_name}', section '{}'", section.title)
                })?;
            }
        }

        for (idx, rule) in self.correspondence.iter().enumerate() {
            rule.validate()
                .with_context(|| format!("in type '{type_name}', correspondence rule {idx}"))?;
        }
        Ok(())
    }

    /// The compiled frontmatter validator, or `None` without a `frontmatter:`.
    ///
    /// Compiled once and memoized; returns `Err` if the json-schema is invalid.
    pub fn frontmatter_validator(&self) -> Result<Option<&CompiledSchema>> {
        let Some(schema) = &self.frontmatter else {
            return Ok(None);
        };
        if self.compiled.get().is_none() {
            let _ = self
                .compiled
                .set(compile_frontmatter_schema(&schema.schema)?);
        }
        Ok(self.compiled.get())
    }

    /// The json-schema for one frontmatter property, if declared under
    /// `frontmatter.properties`. Used by `td json` for type coercion.
    pub fn frontmatter_property(&self, key: &str) -> Option<&serde_json::Value> {
        self.frontmatter
            .as_ref()?
            .schema
            .get("properties")?
            .get(key)
            .filter(|v| !v.is_null())
    }

    /// Frontmatter keys in schema-declared order, for `td fmt` serialization.
    pub fn frontmatter_field_order(&self) -> Vec<String> {
        self.frontmatter
            .as_ref()
            .map(|fm| fm.property_order.clone())
            .unwrap_or_default()
    }

    /// Whether a document of this type must carry frontmatter at all.
    ///
    /// `type` doesn't count: path-matched files get their type from their
    /// location, so requiring it never forces a frontmatter block.
    pub fn has_required_frontmatter(&self) -> bool {
        self.frontmatter.as_ref().is_some_and(|fm| {
            fm.schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|req| req.iter().any(|v| v.as_str() != Some("type")))
        })
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
/// - `"from_directory"` → H1 must match the immediate parent directory name,
///   compared ignoring ASCII case
/// - `"from_date"` → H1 is derived from the filename parsed as `YYYY-MM` or
///   `YYYY-MM-DD` (e.g. `"February 2026"`, `"April 14, 2026"`)
/// - `"required"` → H1 must exist (unfixable if missing)
/// - anything else → `Fixed("…")`: H1 auto-created with that text if missing
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TitleMode {
    #[default]
    None,
    FromFilename,
    /// H1 must match the name of the file's immediate parent directory,
    /// ignoring ASCII case.
    ///
    /// `typedown/README.md` → `# typedown`; `bridge/README.md` keeps
    /// `# Bridge`. Directory names are filesystem slugs, so the directory
    /// constrains *which* title the H1 states, not how it is capitalised.
    FromDirectory,
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
/// Deserialized from YAML: `ordered` or `unordered`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulletMode {
    /// Only ordered (numbered) lists.
    Ordered,
    /// Only unordered (dash/bullet) lists.
    Unordered,
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
    /// `ordered` — numbered lists only; `unordered` — dash lists only.
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

/// Typed field definition for a section's `properties:` map.
///
/// These are the one place typedown's own field types survive: a section's
/// `properties:` describes `Key: Value` pairs inside a list item, which is
/// prose rather than frontmatter, so json-schema has no instance to bind to.
/// Frontmatter itself is json-schema — see `TypeDef::frontmatter`.
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
        let no_values = self.values.as_ref().is_none_or(|v| v.is_empty());
        match self.field_type {
            FieldType::Enum if no_values => {
                anyhow::bail!("field '{field_name}': enum type requires non-empty 'values'")
            }
            FieldType::List if self.item_type == Some(FieldType::Enum) && no_values => {
                anyhow::bail!("field '{field_name}': list of enum requires non-empty 'values'")
            }
            _ => {}
        }
        Ok(())
    }
}

/// Field types for a section's `properties:` map.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Date,
    Datetime,
    Integer,
    Float,
    Bool,
    Enum,
    List,
}

/// Auto-managed section content.
///
/// When set, the validator checks the section against the template and
/// auto-fixes it on `td fmt`. How the template meets content the document
/// already has is decided by [`MergeMode`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ManagedContent {
    /// Markdown template the section must match.
    pub template: String,
    /// How the template combines with content the document already has
    /// (default: [`MergeMode::Upsert`]).
    #[serde(default)]
    pub merge: MergeMode,
    /// Which of the type's documents the managed section applies to
    /// (default: [`ManagedScope::Any`]).
    #[serde(default)]
    pub scope: ManagedScope,
    /// Legacy section titles to migrate away from.
    #[serde(default)]
    pub migrate_from: Vec<String>,
}

/// Which documents of a type a `managed_content` section applies to.
///
/// Lets a type keep a recursive `paths:` glob while confining a
/// root-shaped managed section to the project's own copy of the document.
/// `agents.yaml` is the motivating case: `**/CLAUDE.md` is deliberate
/// (per-directory instruction files are a real convention and want the
/// size warning), but its `Related Documents` template names `README.md`,
/// `GOALS.md`, `tasks/` and `journal/` — paths that only exist at the
/// project root.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ManagedScope {
    /// Every document the type matches (default).
    #[default]
    Any,
    /// Only a document sitting directly in the schema root (the
    /// `.typedown/` parent). Nested matches keep whatever they authored;
    /// the section is neither injected nor rewritten there.
    Root,
}

/// How a `managed_content` template combines with a section's existing content.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MergeMode {
    /// Insert and normalise the template's own entries, keep everything else
    /// (default).
    ///
    /// List items are matched by identity rather than position, so the
    /// template rewrites the entries it declares and leaves entries it has
    /// never heard of alone — they are appended after the templated ones.
    #[default]
    Upsert,
    /// Overwrite the section with the template, discarding anything else it
    /// contained.
    Replace,
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

/// A `template:` that constrains nothing, found at schema load time.
///
/// Once the list marker is stripped, the template compiles to free-text
/// wildcards only, so `matches_template` accepts every list item.  The author
/// almost certainly meant to describe a format, not to write a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VacuousTemplate {
    /// Type name — the `.typedown/*.yaml` file stem.
    pub type_name: String,
    /// Section title, or `"intro"` for the intro section.
    pub section: String,
    /// The template as written.
    pub template: String,
}

/// Whether `segments` match any list item at all.
///
/// The leading list marker (`- `) is expected on both the template and the item
/// text, so it carries no information; anything left must include at least one
/// segment that isn't a free-text wildcard.
fn is_vacuous_template(segments: &[TemplateSegment]) -> bool {
    let body = match segments.first() {
        Some(TemplateSegment::Literal(lit)) if matches!(lit.trim_end(), "" | "-" | "*" | "+") => {
            &segments[1..]
        }
        _ => segments,
    };
    !body.is_empty() && body.iter().all(|s| *s == TemplateSegment::Text)
}

impl Schema {
    /// Section templates across all types that validate anything at all.
    ///
    /// Pure: the caller turns these into diagnostics and decides where to
    /// report them.
    pub fn vacuous_templates(&self) -> Vec<VacuousTemplate> {
        let mut out = Vec::new();
        for (type_name, type_def) in &self.types {
            let intro = type_def.structure.intro.iter().map(|s| ("intro", s));
            let sections = type_def
                .structure
                .sections
                .iter()
                .map(|s| (s.title.as_str(), s));
            for (section, section_def) in intro.chain(sections) {
                let Some(template) = &section_def.template else {
                    continue;
                };
                if is_vacuous_template(&parse_template(template)) {
                    out.push(VacuousTemplate {
                        type_name: type_name.clone(),
                        section: section.to_string(),
                        template: template.clone(),
                    });
                }
            }
        }
        out
    }
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
version: 2
description: A recipe document
frontmatter:
  type: object
  properties:
    servings:
      type: integer
    cuisine:
      type: string
      enum: [italian, mexican, japanese]
    source:
      type: string
  required: [servings]
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        td.validate("recipe").unwrap();
        assert_eq!(
            td.frontmatter_field_order(),
            ["servings", "cuisine", "source"]
        );
        assert!(td.has_required_frontmatter());
    }

    // ── Section `properties:` field defs ──────────────────────────────────────

    #[test]
    fn test_section_property_order_preserved() {
        let yaml = r#"
version: 2
structure:
  sections:
    - title: Components
      properties:
        zebra:
          type: string
        alpha:
          type: string
        middle:
          type: string
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let props = td.structure.sections[0].properties.as_ref().unwrap();
        let keys: Vec<&str> = props.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, ["zebra", "alpha", "middle"]);
    }

    #[test]
    fn test_all_property_field_types() {
        let yaml = r#"
version: 2
structure:
  sections:
    - title: Parts
      properties:
        a: { type: string }
        b: { type: date }
        c: { type: datetime }
        d: { type: integer }
        e: { type: bool }
        f: { type: enum, values: [x] }
        g: { type: float }
        h: { type: list, item_type: string }
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let props = td.structure.sections[0].properties.as_ref().unwrap();
        assert_eq!(props["a"].field_type, FieldType::String);
        assert_eq!(props["b"].field_type, FieldType::Date);
        assert_eq!(props["c"].field_type, FieldType::Datetime);
        assert_eq!(props["d"].field_type, FieldType::Integer);
        assert_eq!(props["e"].field_type, FieldType::Bool);
        assert_eq!(props["f"].field_type, FieldType::Enum);
        assert_eq!(props["g"].field_type, FieldType::Float);
        assert_eq!(props["h"].field_type, FieldType::List);
        assert_eq!(props["h"].item_type, Some(FieldType::String));
    }

    /// A section's `properties:` are checked for internal consistency the same
    /// way frontmatter fields once were -- `enum` still needs `values`.
    #[test]
    fn test_section_property_enum_without_values_rejected() {
        let yaml = "version: 2
structure:
  sections:
    - title: Parts
      properties:
        category:
          type: enum
";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let err = td.validate("hardware").unwrap_err().to_string();
        assert!(err.contains("in type 'hardware', section 'Parts'"), "{err}");
    }

    #[test]
    fn test_intro_property_enum_without_values_rejected() {
        let yaml = "version: 2
structure:
  intro:
    properties:
      category:
        type: enum
";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.validate("hardware").is_err());
    }

    // ── v2 schemas ────────────────────────────────────────────────────────────

    /// An unversioned schema is a v1 leftover.  Reading it as v2 would drop
    /// its `fields:` block and quietly enforce nothing, so it is a load error.
    #[test]
    fn test_missing_version_is_an_error() {
        let td: TypeDef = serde_yaml::from_str("description: a note\n").unwrap();
        let err = td.validate("note").unwrap_err().to_string();
        assert!(err.contains("missing 'version: 2'"), "{err}");
    }

    #[test]
    fn test_version_1_is_an_error() {
        let td: TypeDef =
            serde_yaml::from_str("version: 1\nfields:\n  name:\n    type: string\n").unwrap();
        let err = td.validate("note").unwrap_err().to_string();
        assert!(
            err.contains("version 1 ('fields:') is no longer supported"),
            "{err}"
        );
    }

    #[test]
    fn test_v2_frontmatter_parses_as_json_schema() {
        let yaml = r#"
version: 2
frontmatter:
  type: object
  properties:
    ticker:
      type: string
    weight:
      type: integer
  required: [ticker]
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.version, 2);
        td.validate("security").unwrap();
        let fm = td.frontmatter.as_ref().unwrap();
        assert_eq!(fm.schema["properties"]["weight"]["type"], "integer");
        assert_eq!(fm.schema["required"][0], "ticker");
        assert!(td.frontmatter_validator().unwrap().is_some());
    }

    #[test]
    fn test_v2_property_order_is_the_authors_order() {
        // serde_json sorts object keys; `td fmt` must still write frontmatter
        // in the order the schema declares.
        let yaml = "version: 2\nfrontmatter:\n  type: object\n  properties:\n    zebra: {}\n    alpha: {}\n    middle: {}\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.frontmatter_field_order(), ["zebra", "alpha", "middle"]);
    }

    /// A v1 body under a v2 header: serde would ignore the unknown `fields:`
    /// key and the type would enforce nothing.  The tombstone catches it.
    #[test]
    fn test_v2_rejects_fields() {
        let yaml = "version: 2\nfields:\n  name:\n    type: string\nfrontmatter:\n  type: object\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let err = td.validate("note").unwrap_err().to_string();
        assert!(err.contains("removed version 1 spelling"), "{err}");
    }

    #[test]
    fn test_unsupported_version_rejected() {
        let td: TypeDef = serde_yaml::from_str("version: 99\n").unwrap();
        let err = td.validate("note").unwrap_err().to_string();
        assert!(err.contains("unsupported schema version 99"), "{err}");
    }

    #[test]
    fn test_v2_invalid_json_schema_rejected() {
        // `required` must be an array of strings, not a string.
        let yaml = "version: 2\nfrontmatter:\n  type: object\n  required: ticker\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert!(td.validate("security").is_err());
    }

    #[test]
    fn test_v2_without_frontmatter_is_valid() {
        let yaml = "version: 2\nstructure:\n  title: from_filename\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        td.validate("note").unwrap();
        assert!(td.frontmatter_validator().unwrap().is_none());
    }

    /// The stale-preset case: a schema dir holding one unversioned file fails
    /// to load rather than silently contributing a type that checks nothing.
    #[test]
    fn test_schema_load_rejects_unversioned_schema() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(".typedown");
        fs::create_dir(&schema_dir).unwrap();

        fs::write(
            schema_dir.join("note.yaml"),
            "fields:\n  priority:\n    type: integer\n",
        )
        .unwrap();

        let err = format!("{:#}", Schema::load(&schema_dir).unwrap_err());
        assert!(err.contains("missing 'version: 2'"), "{err}");
    }

    #[test]
    fn test_schema_load_rejects_v2_with_fields() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(".typedown");
        fs::create_dir(&schema_dir).unwrap();
        fs::write(
            schema_dir.join("broken.yaml"),
            "version: 2\nfields:\n  name:\n    type: string\n",
        )
        .unwrap();
        assert!(Schema::load(&schema_dir).is_err());
    }

    #[test]
    fn test_builtin_presets_match_the_published_meta_schema() {
        let meta: serde_json::Value =
            serde_json::from_str(META_SCHEMA).expect("schema.json is valid json");
        let validator = jsonschema::validator_for(&meta).expect("schema.json compiles");

        for (name, content) in BUILTIN_PRESETS {
            let yaml: serde_yaml::Value = serde_yaml::from_str(content).unwrap();
            let instance: serde_json::Value = serde_yaml::from_value(yaml).unwrap();
            let errors: Vec<String> = validator
                .iter_errors(&instance)
                .map(|e| format!("{}: {e}", e.instance_path()))
                .collect();
            assert!(errors.is_empty(), "preset '{name}': {errors:?}");
        }
    }

    /// Presets are the schemas every project inherits, so each must declare
    /// the one live version and compile its `frontmatter:`.
    #[test]
    fn test_builtin_presets_are_all_version_2() {
        for (name, content) in BUILTIN_PRESETS {
            let td: TypeDef =
                serde_yaml::from_str(content).unwrap_or_else(|e| panic!("preset {name}: {e}"));
            assert_eq!(td.version, 2, "preset '{name}' should be version 2");
            td.validate(name)
                .unwrap_or_else(|e| panic!("preset {name} should be a valid type def: {e:?}"));
        }
    }

    #[test]
    fn test_meta_schema_requires_version_2() {
        let meta: serde_json::Value = serde_json::from_str(META_SCHEMA).unwrap();
        let validator = jsonschema::validator_for(&meta).unwrap();

        let with_fields = serde_json::json!({"version": 2, "fields": {}});
        assert!(!validator.is_valid(&with_fields));

        let unversioned = serde_json::json!({"frontmatter": {"type": "object"}});
        assert!(!validator.is_valid(&unversioned));

        let v1 = serde_json::json!({"version": 1});
        assert!(!validator.is_valid(&v1));

        let good_v2 = serde_json::json!({"version": 2, "frontmatter": {"type": "object"}});
        assert!(validator.is_valid(&good_v2));
    }

    // ── Correspondence ────────────────────────────────────────────────────────

    /// Every rule shape, as `schema-authoring.md` documents them.
    const CORRESPONDENCE_YAML: &str = "\
version: 2
correspondence:
  - each: frontmatter.files[]
    requires:
      subsection-under: \"## Files\"
      heading: \"### {quality}\"
  - each: subsections-under \"## Files\"
    requires:
      frontmatter-item: files[].quality
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
  - each: links-in-section \"## Cast\"
    requires:
      target_type: personality
      backlink-in: \"## Movies\"
  - each: docs-of-type tvseason in-directory \".\"
    requires:
      link-in: \"## Seasons\"
";

    #[test]
    fn test_correspondence_rules_load_on_a_v2_schema() {
        let td: TypeDef = serde_yaml::from_str(CORRESPONDENCE_YAML).unwrap();
        td.validate("movie").unwrap();
        assert_eq!(td.correspondence.len(), 7);
    }

    #[test]
    fn test_correspondence_defaults_to_empty() {
        let td: TypeDef = serde_yaml::from_str("version: 2\n").unwrap();
        assert!(td.correspondence.is_empty());
    }

    #[test]
    fn test_schema_load_rejects_a_malformed_correspondence_rule() {
        let dir = TempDir::new().unwrap();
        let schema_dir = dir.path().join(SCHEMA_DIR);
        fs::create_dir(&schema_dir).unwrap();
        fs::write(
            schema_dir.join("movie.yaml"),
            "version: 2\ncorrespondence:\n  - each: frontmatter.files[]\n    requires:\n      target_type: personality\n",
        )
        .unwrap();
        let err = Schema::load(&schema_dir).unwrap_err().to_string();
        assert!(err.contains("invalid schema"), "{err}");
    }

    #[test]
    fn test_meta_schema_accepts_the_documented_correspondence_rules() {
        let meta: serde_json::Value = serde_json::from_str(META_SCHEMA).unwrap();
        let validator = jsonschema::validator_for(&meta).unwrap();

        let yaml: serde_yaml::Value = serde_yaml::from_str(CORRESPONDENCE_YAML).unwrap();
        let instance: serde_json::Value = serde_yaml::from_value(yaml).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn test_meta_schema_rejects_correspondence_on_v1_and_unknown_selectors() {
        let meta: serde_json::Value = serde_json::from_str(META_SCHEMA).unwrap();
        let validator = jsonschema::validator_for(&meta).unwrap();

        let v1 = serde_json::json!({
            "correspondence": [{"each": "frontmatter.files[]", "requires": {"subsection-under": "## Files"}}],
        });
        assert!(!validator.is_valid(&v1), "'correspondence' needs version 2");

        let bogus_selector = serde_json::json!({
            "version": 2,
            "correspondence": [{"each": "wibbles-under \"## Files\"", "requires": {"target_type": "movie"}}],
        });
        assert!(!validator.is_valid(&bogus_selector));

        let bogus_key = serde_json::json!({
            "version": 2,
            "correspondence": [{"each": "frontmatter.files[]", "requires": {"subsection_under": "## Files"}}],
        });
        assert!(
            !validator.is_valid(&bogus_key),
            "underscore spelling is not a key"
        );

        let unscoped = serde_json::json!({
            "version": 2,
            "correspondence": [{"each": "docs-of-type tvseason", "requires": {"link-in": "## Seasons"}}],
        });
        assert!(
            !validator.is_valid(&unscoped),
            "'docs-of-type' has to say where to look"
        );

        let absolute_scope = serde_json::json!({
            "version": 2,
            "correspondence": [{"each": "docs-of-type tvseason in-directory \"/shows\"", "requires": {"link-in": "## Seasons"}}],
        });
        assert!(
            !validator.is_valid(&absolute_scope),
            "the scope is relative to the document"
        );
    }

    // ── FieldDef validation ───────────────────────────────────────────────────

    /// `FieldDef` survives only inside a section's `properties:`, so that is
    /// where its internal-consistency rules are exercised.
    fn props_type(body: &str) -> TypeDef {
        let yaml = format!(
            "version: 2\nstructure:\n  sections:\n    - title: Parts\n      properties:\n{body}"
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn test_enum_without_values_fails() {
        assert!(props_type("        status:\n          type: enum\n")
            .validate("test")
            .is_err());
    }

    #[test]
    fn test_enum_with_empty_values_fails() {
        assert!(
            props_type("        status:\n          type: enum\n          values: []\n")
                .validate("test")
                .is_err()
        );
    }

    #[test]
    fn test_enum_with_values_ok() {
        assert!(
            props_type("        status:\n          type: enum\n          values: [a, b]\n")
                .validate("test")
                .is_ok()
        );
    }

    #[test]
    fn test_list_of_enum_without_values_fails() {
        assert!(
            props_type("        tags:\n          type: list\n          item_type: enum\n")
                .validate("test")
                .is_err()
        );
    }

    #[test]
    fn test_list_of_string_ok() {
        assert!(
            props_type("        names:\n          type: list\n          item_type: string\n")
                .validate("test")
                .is_ok()
        );
    }

    // ── StructureDef ─────────────────────────────────────────────────────────

    #[test]
    fn test_structure_defaults() {
        let yaml = "version: 2\ndescription: foo\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::None);
        assert!(td.structure.strict_sections);
        assert!(td.structure.size_warning.is_none());
        assert!(td.structure.sections.is_empty());
    }

    #[test]
    fn test_structure_all_fields_from_yaml() {
        let yaml = r#"
version: 2
structure:
  title: from_filename
  strict_sections: false
  size_warning: 4000
  sections:
    - title: Notes
      required: false
      bullets: unordered
"#;
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.title, TitleMode::FromFilename);
        assert!(!td.structure.strict_sections);
        assert_eq!(td.structure.size_warning, Some(4000));
        assert_eq!(td.structure.sections.len(), 1);
        assert_eq!(
            td.structure.sections[0].bullets,
            Some(BulletMode::Unordered)
        );
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
        // Preserving what the template doesn't declare is the default; the
        // clobbering behaviour has to be asked for.
        assert_eq!(mc.merge, MergeMode::Upsert);
    }

    #[test]
    fn test_managed_content_merge_mode_from_yaml() {
        let yaml = "structure:\n  sections:\n    - title: Related\n      managed_content:\n        template: \"## Related\\n\"\n        merge: replace\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        let mc = td.structure.sections[0].managed_content.as_ref().unwrap();
        assert_eq!(mc.merge, MergeMode::Replace);
    }

    #[test]
    fn test_meta_schema_allows_managed_content_merge() {
        // `managed_content` is `additionalProperties: false`, so a knob the
        // engine understands but schema.json doesn't is unusable in YAML.
        let meta: serde_json::Value = serde_json::from_str(META_SCHEMA).unwrap();
        let validator = jsonschema::validator_for(&meta).unwrap();

        for mode in ["upsert", "replace"] {
            let instance = serde_json::json!({
                "version": 2,
                "structure": {
                    "sections": [{
                        "title": "Related",
                        "managed_content": {"template": "## Related\n", "merge": mode},
                    }],
                }
            });
            let errors: Vec<String> = validator
                .iter_errors(&instance)
                .map(|e| format!("{}: {e}", e.instance_path()))
                .collect();
            assert!(errors.is_empty(), "merge: {mode}: {errors:?}");
        }

        let bogus = serde_json::json!({
            "version": 2,
            "structure": {
                "sections": [{
                    "title": "Related",
                    "managed_content": {"template": "## Related\n", "merge": "clobber"},
                }],
            }
        });
        assert!(
            !validator.is_valid(&bogus),
            "an unknown merge mode must be rejected"
        );
    }

    // ── BulletMode deserialization ────────────────────────────────────────────

    #[test]
    fn test_bullet_mode_string_values() {
        let yaml = "structure:\n  sections:\n    - title: B\n      bullets: ordered\n    - title: C\n      bullets: unordered\n";
        let td: TypeDef = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(td.structure.sections[0].bullets, Some(BulletMode::Ordered));
        assert_eq!(
            td.structure.sections[1].bullets,
            Some(BulletMode::Unordered)
        );
    }

    /// `any` and its `true` shorthand were removed: a section that accepts both
    /// list types is what omitting `bullets` already means.
    #[test]
    fn test_bullet_mode_any_rejected() {
        for value in ["any", "true", "false"] {
            let yaml = format!("structure:\n  sections:\n    - title: A\n      bullets: {value}\n");
            let result: Result<TypeDef, _> = serde_yaml::from_str(&yaml);
            assert!(result.is_err(), "bullets: {value} should be rejected");
        }
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
            "version: 2\ndescription: A recipe\nfrontmatter:\n  type: object\n  properties:\n    servings:\n      type: integer\n",
        )
        .unwrap();
        fs::write(
            schema_dir.join("note.yaml"),
            "version: 2\ndescription: A note\nfrontmatter:\n  type: object\n  properties:\n    tags:\n      type: array\n      items:\n        type: string\n",
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

        fs::write(
            schema_dir.join("recipe.yaml"),
            "version: 2\ndescription: A recipe\n",
        )
        .unwrap();
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

        fs::write(schema_dir.join("bad.yaml"), "paths: [\ninvalid yaml").unwrap();

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
            "version: 2\nstructure:\n  sections:\n    - title: Parts\n      properties:\n        status:\n          type: enum\n",
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

    // ── vacuous template lint ─────────────────────────────────────────────────

    /// The real case that motivated the lint: a journal schema whose template
    /// read like guidance but compiled to `- ` + wildcard, matching every item.
    #[test]
    fn test_vacuous_template_prose_only() {
        let yaml = "structure:\n  sections:\n    - title: Notes\n      template: '- Fact or impression about the movie'\n";
        let mut schema = Schema::default();
        schema
            .types
            .insert("movie".to_string(), serde_yaml::from_str(yaml).unwrap());

        let lints = schema.vacuous_templates();
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].type_name, "movie");
        assert_eq!(lints[0].section, "Notes");
        assert_eq!(lints[0].template, "- Fact or impression about the movie");
    }

    #[test]
    fn test_vacuous_template_intro_section() {
        let yaml = "structure:\n  intro:\n    template: '- Text'\n";
        let mut schema = Schema::default();
        schema
            .types
            .insert("note".to_string(), serde_yaml::from_str(yaml).unwrap());

        let lints = schema.vacuous_templates();
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].section, "intro");
    }

    /// Anything that pins down part of the item — bold, a link, a date, or a
    /// separator literal — makes the template do real work.
    #[test]
    fn test_constraining_templates_not_vacuous() {
        for template in [
            "- **Text**: Text",
            "- [text](url) - Text",
            "- YYYY-MM-DD - Text",
            "- Text, Text",
        ] {
            let yaml =
                format!("structure:\n  sections:\n    - title: S\n      template: '{template}'\n");
            let mut schema = Schema::default();
            schema
                .types
                .insert("t".to_string(), serde_yaml::from_str(&yaml).unwrap());
            assert!(
                schema.vacuous_templates().is_empty(),
                "template {template:?} should not be flagged"
            );
        }
    }

    #[test]
    fn test_no_template_is_not_vacuous() {
        let yaml = "structure:\n  sections:\n    - title: S\n      bullets: unordered\n";
        let mut schema = Schema::default();
        schema
            .types
            .insert("t".to_string(), serde_yaml::from_str(yaml).unwrap());
        assert!(schema.vacuous_templates().is_empty());
    }

    /// Built-in presets ship as examples, so they must pass their own lint.
    #[test]
    fn test_builtin_presets_have_no_vacuous_templates() {
        let mut schema = Schema::default();
        for (name, content) in BUILTIN_PRESETS {
            schema.types.insert(
                (*name).to_string(),
                serde_yaml::from_str(content).expect("preset parses"),
            );
        }
        assert_eq!(schema.vacuous_templates(), vec![]);
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
            "version: 2\npaths:\n  - \".claude/commands/*.md\"\nstructure:\n  title: required\n",
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

    // ── journal / journal-entry path discrimination ─────────────────────────

    #[test]
    fn test_journal_patterns_no_conflict() {
        let mut schema = Schema::default();
        let journal: TypeDef =
            serde_yaml::from_str("paths:\n  - \"**/journal/????-??.md\"\n").unwrap();
        let entry: TypeDef =
            serde_yaml::from_str("paths:\n  - \"**/journal/????-??-??T??-??.md\"\n").unwrap();
        schema.types.insert("journal".to_string(), journal);
        schema.types.insert("journal-entry".to_string(), entry);

        let matcher = schema.build_path_matcher().unwrap();

        // Monthly file matches journal only
        let m = matcher.match_path("journal/2026-04.md");
        assert_eq!(m, vec!["journal"], "monthly: {m:?}");

        // Entry file matches journal-entry only
        let m = matcher.match_path("journal/2026-04-03T14-32.md");
        assert_eq!(m, vec!["journal-entry"], "entry: {m:?}");

        // Nested paths work too
        let m = matcher.match_path("projects/foo/journal/2026-04-03T14-32.md");
        assert_eq!(m, vec!["journal-entry"], "nested entry: {m:?}");

        // Unrelated file matches neither
        assert!(matcher.match_path("journal/notes.md").is_empty());
    }

    // ── Built-in preset path patterns ────────────────────────────────────────

    fn builtin_matcher() -> PathMatcher {
        let mut schema = Schema::default();
        for (name, content) in BUILTIN_PRESETS {
            let td: TypeDef = serde_yaml::from_str(content)
                .unwrap_or_else(|e| panic!("preset {name} should parse: {e}"));
            schema.types.insert((*name).to_string(), td);
        }
        schema.build_path_matcher().unwrap()
    }

    #[test]
    fn test_root_document_presets_do_not_claim_nested_files() {
        // Regression: `paths: ["**/README.md"]` plus `title: from_directory`
        // claimed every nested README and renamed its H1 after the containing
        // directory — a nested README titled `# TSM mats groups` lost its
        // heading and became `# tsm`. Singleton project documents anchor at
        // the schema root.
        let matcher = builtin_matcher();

        assert_eq!(matcher.match_path("README.md"), vec!["readme"]);
        assert_eq!(matcher.match_path("GOALS.md"), vec!["goals"]);
        assert_eq!(matcher.match_path("ROADMAP.md"), vec!["roadmap"]);

        for nested in [
            "tsm/README.md",
            "docs/deep/README.md",
            "tsm/GOALS.md",
            "docs/ROADMAP.md",
        ] {
            assert!(
                matcher.match_path(nested).is_empty(),
                "{nested} should not be claimed by a root-document preset"
            );
        }
    }

    #[test]
    fn test_agents_preset_still_claims_nested_instruction_files() {
        // AGENTS.md / CLAUDE.md are a per-directory convention and the preset
        // has `title: none`, so there is no H1 for it to overwrite.
        let matcher = builtin_matcher();
        assert_eq!(matcher.match_path("CLAUDE.md"), vec!["agents"]);
        assert_eq!(matcher.match_path("src/AGENTS.md"), vec!["agents"]);
    }

    #[test]
    fn test_agents_preset_confines_related_documents_to_the_root() {
        // The glob stays recursive so nested instruction files keep the size
        // warning, but the Related Documents template names project-root paths
        // (README.md, GOALS.md, tasks/, journal/) — wrong for a nested file.
        let (_, content) = BUILTIN_PRESETS
            .iter()
            .find(|(name, _)| *name == "agents")
            .expect("agents preset should exist");
        let td: TypeDef = serde_yaml::from_str(content).expect("agents preset should parse");

        let managed = td
            .structure
            .sections
            .iter()
            .find(|s| s.title == "Related Documents")
            .and_then(|s| s.managed_content.as_ref())
            .expect("Related Documents should be managed");

        assert_eq!(managed.scope, ManagedScope::Root);
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

    /// A stale v1 schema in `~/.config/typedown/presets/` is the file the
    /// version requirement exists for, and the one nothing else would mention:
    /// it sits outside the project, so no document names it.  Preset loading
    /// used to swallow the failure and quietly hand back the built-in of the
    /// same name, which is a project judged by a schema nobody wrote.
    #[test]
    fn test_broken_xdg_preset_is_reported_not_swallowed() {
        let dir = TempDir::new().unwrap();
        let presets = dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("readme.yaml"),
            "paths:\n  - \"**/README.md\"\nfields:\n  owner:\n    type: string\n    required: true\n",
        )
        .unwrap();

        let (schema, err) = load_presets_from(Some(presets));
        let err = format!("{:#}", err.expect("a broken preset must surface its error"));
        assert!(err.contains("missing 'version: 2'"), "{err}");
        assert!(
            schema.unwrap().get_type("readme").is_some(),
            "the built-in readme still stands in — which is exactly why the \
             error has to be reported rather than inferred from a missing type"
        );
    }

    #[test]
    fn test_load_presets_from_xdg() {
        let dir = TempDir::new().unwrap();
        let presets = dir.path().join("typedown/presets");
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("readme.yaml"),
            "version: 2\npaths:\n  - \"**/README.md\"\nstructure:\n  title: from_directory\n",
        )
        .unwrap();

        let schema = Schema::load(&presets).expect("should load presets");
        assert!(schema.get_type("readme").is_some());
    }
}
