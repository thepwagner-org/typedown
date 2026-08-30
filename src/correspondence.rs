//! Correspondence rules: schema-declared ties between frontmatter data and body
//! prose, checked in both directions.
//!
//! A rule reads as a quantifier plus an obligation — *each* of these things
//! *requires* that:
//!
//! ```yaml
//! correspondence:
//!   - each: frontmatter.files[]                 # data → prose
//!     requires: { subsection-under: "## Files", heading: "### {quality}" }
//!   - each: subsections-under "## Files"        # prose → data (no orphans)
//!     requires: { frontmatter-item: "files[].quality" }
//!   - each: frontmatter.species[]               # data → a link, not prose
//!     requires: { link-in: "## Species", target_type: species }
//!   - each: links-in-section "## Species"       # that link's inverse
//!     requires: { frontmatter-item: "species[]" }
//!   - each: links-in-section "## Cast"          # prose → prose
//!     requires: { target_type: personality, backlink-in: "## Movies" }
//!   - each: docs-of-type tvseason in-directory "."  # disk → prose
//!     requires: { link-in: "## Seasons" }
//! ```
//!
//! A frontmatter path names an array with `[]` or a single scalar without it;
//! the rules read either as a set, so a document with one location and a
//! document with several species differ only in their data.
//!
//! Pure, like the rest of validation: takes a `&Document` and the already
//! preloaded link data on [`ValidateCtx`], returns diagnostics. Nothing here is
//! fixable — a missing subsection needs prose only a human or an agent can
//! write, so violations stay diagnostics rather than becoming silent rewrites.
//! `docs-of-type` is the one rule that starts from files rather than from the
//! document, and it still does no I/O: the walk happened at preload, and what
//! it reads is the index on [`ValidateCtx`].

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_yaml::Value;

use crate::{
    ast::{inlines_to_string, scalar_to_string, Block, Document, Frontmatter},
    validate::{extract_links, normalize_path, resolve_link_path, Diagnostic, ValidateCtx},
};

// ── Rule types ────────────────────────────────────────────────────────────────

/// One correspondence rule: a set of things and what each of them must have.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrespondenceRule {
    /// The set the rule quantifies over.
    pub each: Selector,
    /// What each member of that set must have.
    pub requires: Requirement,
    /// Human-readable note. Ignored by the engine; useful as LLM guidance.
    #[serde(default)]
    #[allow(dead_code)]
    pub description: Option<String>,
}

/// What a rule quantifies over, parsed from the `each:` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// `frontmatter.files[]` — every item of a frontmatter array.
    FrontmatterItems(ItemPath),
    /// `subsections-under "## Files"` — every heading one level below the
    /// named section, within its span.
    SubsectionsUnder(SectionRef),
    /// `links-in-section "## Cast"` — every relative link in the named section.
    LinksInSection(SectionRef),
    /// `docs-of-type tvseason in-directory "."` — every document of a type that
    /// exists in a directory, whether or not this document mentions it.
    DocsOfType {
        /// The schema type the documents must resolve to.
        type_name: String,
        /// Where to look for them, relative to this document.
        scope: DirScope,
    },
}

/// The `in-directory "."` / `under-directory "."` half of a `docs-of-type`
/// selector: a directory relative to the document being validated, and whether
/// nested directories count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirScope {
    /// Directory relative to the document's own directory. `.` is its siblings.
    pub dir: PathBuf,
    /// `under-directory`: subdirectories count too.
    pub recursive: bool,
}

impl std::fmt::Display for DirScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keyword = if self.recursive {
            "under-directory"
        } else {
            "in-directory"
        };
        write!(f, "{keyword} \"{}\"", self.dir.display())
    }
}

/// The obligation half of a rule. Which keys are meaningful depends on the
/// selector; [`CorrespondenceRule::validate`] rejects mismatched pairings at
/// schema load time rather than ignoring them at validation time.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    /// The section the required subsection must live under.
    #[serde(default, rename = "subsection-under")]
    pub subsection_under: Option<SectionRef>,
    /// The heading that subsection must carry, with `{field}` placeholders
    /// interpolated from the item.
    #[serde(default)]
    pub heading: Option<HeadingTemplate>,
    /// The frontmatter path a subsection must correspond to.
    #[serde(default, rename = "frontmatter-item")]
    pub frontmatter_item: Option<ItemPath>,
    /// Links must point at documents of this schema type.
    #[serde(default)]
    pub target_type: Option<String>,
    /// The target document must link back from one of these sections.
    #[serde(default, rename = "backlink-in")]
    pub backlink_in: Option<SectionRefs>,
    /// The section that must carry a link to each selected document.
    #[serde(default, rename = "link-in")]
    pub link_in: Option<SectionRef>,
}

/// A reference to a section by heading level and text: `"## Files"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionRef {
    /// Heading level, from the number of leading `#`.
    pub level: u8,
    /// Heading text, markers and surrounding whitespace stripped.
    pub title: String,
}

impl std::fmt::Display for SectionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", "#".repeat(self.level as usize), self.title)
    }
}

impl SectionRef {
    /// Parse `"## Files"`. The marker is required — it carries the level.
    pub fn parse(s: &str) -> Result<Self> {
        let hashes = s.chars().take_while(|c| *c == '#').count();
        let title = s[hashes..].trim();
        if hashes == 0 || hashes > 6 || title.is_empty() {
            bail!("section reference '{s}' must be a heading with its marker, e.g. \"## Files\"");
        }
        Ok(Self {
            level: hashes as u8,
            title: title.to_string(),
        })
    }
}

/// One or more section references — `backlink-in` takes either spelling, since
/// a type can legitimately link back from any of several sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionRefs(pub Vec<SectionRef>);

impl SectionRefs {
    /// The section titles, joined the way [`Diagnostic::MissingBacklink`]
    /// renders them.
    fn joined_titles(&self) -> String {
        self.0
            .iter()
            .map(|s| s.title.as_str())
            .collect::<Vec<_>>()
            .join("' or '")
    }
}

/// A path to a frontmatter value, and what the rule quantifies over once it
/// gets there: `files[]`, `files[].quality`, `meta.files[].quality`, or a plain
/// `location` naming a single scalar.
///
/// The `[]` is what makes the path a set of many rather than a set of one. Both
/// spellings work everywhere a path is asked for — a document with one location
/// and a document with several species differ in the data, not in the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPath {
    /// Path segments down to the value: the array, or the scalar itself.
    pub path: Vec<String>,
    /// The path ended in `[]`, so the set is that array's items.
    pub is_array: bool,
    /// Field read from each item, when the path continues past `[]`.
    pub field: Option<String>,
}

impl std::fmt::Display for ItemPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.join("."))?;
        if self.is_array {
            write!(f, "[]")?;
        }
        match &self.field {
            Some(field) => write!(f, ".{field}"),
            None => Ok(()),
        }
    }
}

impl ItemPath {
    /// Parse `files[].quality`, or a bare `location`. A leading `frontmatter.`
    /// is accepted and stripped, so both spellings in the docs mean the same
    /// path.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let s = s.strip_prefix("frontmatter.").unwrap_or(s);
        let Some((array_part, rest)) = s.split_once("[]") else {
            // No `[]`: the path names one value, which the rules treat as a set
            // of one. A stray bracket is a malformed array marker, not a name.
            if s.contains(['[', ']']) {
                bail!("frontmatter path '{s}' must spell its array marker '[]', e.g. \"files[].quality\"");
            }
            let path = split_segments(s);
            if path.is_empty() {
                bail!("frontmatter path '{s}' names no field");
            }
            return Ok(Self {
                path,
                is_array: false,
                field: None,
            });
        };
        let path = split_segments(array_part);
        if path.is_empty() {
            bail!("frontmatter path '{s}' has no array name before '[]'");
        }
        let field = match rest.trim() {
            "" => None,
            rest => {
                let field = rest.strip_prefix('.').unwrap_or(rest);
                if field.is_empty() || field.contains(['.', '[']) {
                    bail!("frontmatter path '{s}' must end in a single item field, e.g. \"files[].quality\"");
                }
                Some(field.to_string())
            }
        };
        Ok(Self {
            path,
            is_array: true,
            field,
        })
    }

    /// How a diagnostic names one member of the set: `files[0]` for an array
    /// item, and the path itself when the path names a single value.
    fn member_name(&self, idx: usize) -> String {
        match self.is_array {
            true => format!("{}[{idx}]", self.path.join(".")),
            false => self.path.join("."),
        }
    }
}

/// Split a dotted path into its non-empty segments.
fn split_segments(s: &str) -> Vec<String> {
    s.split('.')
        .filter(|seg| !seg.is_empty())
        .map(str::to_string)
        .collect()
}

/// A heading with `{field}` placeholders: `"### {quality}"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingTemplate {
    /// Heading level, from the number of leading `#`.
    pub level: u8,
    /// The template as written, for diagnostics.
    written: String,
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    /// `{name}`, or `{.}` for the item itself when it is a scalar.
    Placeholder(String),
}

impl HeadingTemplate {
    /// Parse `"### {quality}"`.
    pub fn parse(s: &str) -> Result<Self> {
        let hashes = s.chars().take_while(|c| *c == '#').count();
        let body = s[hashes..].trim();
        if hashes == 0 || hashes > 6 || body.is_empty() {
            bail!("heading template '{s}' must be a heading with its marker, e.g. \"### {{quality}}\"");
        }

        let mut parts = Vec::new();
        let mut rest = body;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}').map(|i| open + i) else {
                bail!("heading template '{s}' has an unclosed '{{'");
            };
            let name = rest[open + 1..close].trim();
            if name.is_empty() {
                bail!("heading template '{s}' has an empty '{{}}' placeholder");
            }
            if open > 0 {
                parts.push(Part::Literal(rest[..open].to_string()));
            }
            parts.push(Part::Placeholder(name.to_string()));
            rest = &rest[close + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.to_string()));
        }
        if !parts.iter().any(|p| matches!(p, Part::Placeholder(_))) {
            bail!(
                "heading template '{s}' has no '{{field}}' placeholder, so every item would \
                 require the same heading"
            );
        }

        Ok(Self {
            level: hashes as u8,
            written: s.trim().to_string(),
            parts,
        })
    }

    /// Render the template for one array item.
    ///
    /// `Err` carries the placeholder that had no scalar value on the item —
    /// the schema asked for a heading the data can't name.
    fn render(&self, item: &Value) -> std::result::Result<String, String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(lit) => out.push_str(lit),
                Part::Placeholder(name) => {
                    let value = if name == "." {
                        Some(item)
                    } else {
                        item.as_mapping().and_then(|m| m.get(Value::from(&**name)))
                    };
                    match value.and_then(scalar_to_string) {
                        Some(s) => out.push_str(&s),
                        None => return Err(name.clone()),
                    }
                }
            }
        }
        Ok(out)
    }

    /// The template as the schema author wrote it.
    fn as_written(&self) -> &str {
        &self.written
    }
}

// ── Deserialization ───────────────────────────────────────────────────────────

/// Deserialize a type by running its string parser and reporting failures as
/// serde errors, so a bad rule fails schema load with the parser's message.
macro_rules! deserialize_via_parse {
    ($ty:ty) => {
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let s = String::deserialize(deserializer)?;
                Self::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

deserialize_via_parse!(Selector);
deserialize_via_parse!(SectionRef);
deserialize_via_parse!(ItemPath);
deserialize_via_parse!(HeadingTemplate);

impl<'de> Deserialize<'de> for SectionRefs {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }

        let raw = match OneOrMany::deserialize(deserializer)? {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        };
        if raw.is_empty() {
            return Err(serde::de::Error::custom(
                "'backlink-in' needs at least one section",
            ));
        }
        raw.iter()
            .map(|s| SectionRef::parse(s))
            .collect::<Result<Vec<_>>>()
            .map(SectionRefs)
            .map_err(serde::de::Error::custom)
    }
}

impl Selector {
    /// Parse an `each:` string.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.starts_with("frontmatter.") {
            return Ok(Self::FrontmatterItems(ItemPath::parse(s)?));
        }
        let (verb, arg) = s.split_once(char::is_whitespace).unwrap_or((s, ""));
        let arg = arg.trim();
        if verb == "docs-of-type" {
            return Self::parse_docs_of_type(arg);
        }
        let quoted = unquote(arg);
        match verb {
            "subsections-under" | "links-in-section" if quoted.is_empty() => {
                bail!("'{verb}' needs a section, e.g. {verb} \"## Files\"")
            }
            "subsections-under" => Ok(Self::SubsectionsUnder(SectionRef::parse(quoted)?)),
            "links-in-section" => Ok(Self::LinksInSection(SectionRef::parse(quoted)?)),
            _ => bail!(
                "unknown selector '{s}' (expected \"frontmatter.<array>[]\" or \
                 \"frontmatter.<field>\", \
                 \"subsections-under \\\"## Section\\\"\", \
                 \"links-in-section \\\"## Section\\\"\", or \
                 \"docs-of-type <type> in-directory \\\".\\\"\")"
            ),
        }
    }

    /// Parse the arguments of `docs-of-type <type> in-directory "<dir>"`.
    ///
    /// The scope keyword is required. An unscoped `docs-of-type` would have to
    /// mean the whole project, which is a different rule with a different cost —
    /// better spelled out than defaulted into.
    fn parse_docs_of_type(arg: &str) -> Result<Self> {
        let example = "docs-of-type tvseason in-directory \".\"";
        let (type_name, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
        if type_name.is_empty() {
            bail!("'docs-of-type' needs a type name, e.g. {example}");
        }
        if type_name.contains('"') {
            bail!("'docs-of-type' takes a bare type name, not a quoted one, e.g. {example}");
        }

        let rest = rest.trim();
        let (keyword, dir) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let recursive = match keyword {
            "in-directory" => false,
            "under-directory" => true,
            "" => bail!("'docs-of-type {type_name}' needs a scope, e.g. {example}"),
            other => bail!(
                "unknown scope '{other}' in 'docs-of-type {type_name}' (expected \
                 'in-directory' for one directory or 'under-directory' for it and \
                 everything below it)"
            ),
        };

        let dir = unquote(dir.trim());
        if dir.is_empty() {
            bail!("'{keyword}' needs a directory, e.g. {keyword} \".\" for this document's own");
        }
        let dir = Path::new(dir);
        if dir.is_absolute() {
            bail!(
                "'{keyword} \"{}\"' must be relative to the document, e.g. \".\"",
                dir.display()
            );
        }

        Ok(Self::DocsOfType {
            type_name: type_name.to_string(),
            scope: DirScope {
                dir: dir.to_path_buf(),
                recursive,
            },
        })
    }
}

/// Strip one layer of surrounding double quotes, if present.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(s)
}

// ── Load-time validation ──────────────────────────────────────────────────────

impl CorrespondenceRule {
    /// Check that the rule's `requires:` keys make sense for its selector.
    ///
    /// A requirement the selector can't act on would otherwise be silently
    /// ignored, which is the worst outcome for a schema knob.
    pub fn validate(&self) -> Result<()> {
        let req = &self.requires;
        let reject = |allowed: &str, offenders: &[(&str, bool)]| -> Result<()> {
            for (name, present) in offenders {
                if *present {
                    bail!("'{name}' is not meaningful here — {allowed}");
                }
            }
            Ok(())
        };

        match &self.each {
            Selector::FrontmatterItems(_) => {
                let takes = "'each: frontmatter.<path>' requires 'subsection-under' and \
                             'heading' for a subsection, or 'link-in' — optionally with \
                             'target_type' — for a link";
                reject(
                    takes,
                    &[
                        ("frontmatter-item", req.frontmatter_item.is_some()),
                        ("backlink-in", req.backlink_in.is_some()),
                        // `target_type` narrows what counts as a link, so it
                        // only means something alongside `link-in`.
                        (
                            "target_type",
                            req.target_type.is_some() && req.link_in.is_none(),
                        ),
                    ],
                )?;
                match &req.link_in {
                    // Two ways for data to reach prose, one rule each: a
                    // subsection is a place to write, a link is a place to point.
                    Some(_) => reject(
                        "a rule with 'link-in' requires a link, not a subsection — \
                         declare a second rule for the subsection",
                        &[
                            ("subsection-under", req.subsection_under.is_some()),
                            ("heading", req.heading.is_some()),
                        ],
                    )?,
                    None => {
                        let (Some(container), Some(heading)) =
                            (&req.subsection_under, &req.heading)
                        else {
                            bail!("{takes}");
                        };
                        if heading.level <= container.level {
                            bail!(
                                "heading '{}' must be deeper than its container '{container}'",
                                heading.as_written()
                            );
                        }
                    }
                }
            }
            Selector::SubsectionsUnder(container) => {
                reject(
                    "'each: subsections-under' requires 'frontmatter-item'",
                    &[
                        ("subsection-under", req.subsection_under.is_some()),
                        ("heading", req.heading.is_some()),
                        ("target_type", req.target_type.is_some()),
                        ("backlink-in", req.backlink_in.is_some()),
                        ("link-in", req.link_in.is_some()),
                    ],
                )?;
                if req.frontmatter_item.is_none() {
                    bail!("'each: subsections-under \"{container}\"' requires 'frontmatter-item'");
                }
            }
            Selector::LinksInSection(section) => {
                reject(
                    "'each: links-in-section' takes 'target_type', 'backlink-in', and \
                     'frontmatter-item'",
                    &[
                        ("subsection-under", req.subsection_under.is_some()),
                        ("heading", req.heading.is_some()),
                        ("link-in", req.link_in.is_some()),
                    ],
                )?;
                if req.target_type.is_none()
                    && req.backlink_in.is_none()
                    && req.frontmatter_item.is_none()
                {
                    bail!(
                        "'each: links-in-section \"{section}\"' requires 'target_type', \
                         'backlink-in', 'frontmatter-item', or any combination of them"
                    );
                }
            }
            Selector::DocsOfType { type_name, scope } => {
                reject(
                    "'each: docs-of-type' requires 'link-in'",
                    &[
                        ("subsection-under", req.subsection_under.is_some()),
                        ("heading", req.heading.is_some()),
                        ("frontmatter-item", req.frontmatter_item.is_some()),
                        ("target_type", req.target_type.is_some()),
                        ("backlink-in", req.backlink_in.is_some()),
                    ],
                )?;
                if req.link_in.is_none() {
                    bail!("'each: docs-of-type {type_name} {scope}' requires 'link-in'");
                }
            }
        }
        Ok(())
    }
}

// ── Document validation ───────────────────────────────────────────────────────

/// Check a document against its type's correspondence rules.
pub fn validate_correspondence(
    doc: &Document,
    rules: &[CorrespondenceRule],
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    for rule in rules {
        match &rule.each {
            // Which direction a frontmatter rule runs in is settled at schema
            // load: exactly one of the two requirements is present.
            Selector::FrontmatterItems(path) if rule.requires.link_in.is_some() => {
                items_need_links(doc, path, &rule.requires, ctx, out)
            }
            Selector::FrontmatterItems(path) => {
                items_need_subsections(doc, path, &rule.requires, out)
            }
            Selector::SubsectionsUnder(container) => {
                subsections_need_items(doc, container, &rule.requires, out)
            }
            Selector::LinksInSection(section) => {
                links_need_targets(doc, section, &rule.requires, ctx, out)
            }
            Selector::DocsOfType { type_name, scope } => {
                docs_need_links(doc, type_name, scope, &rule.requires, ctx, out)
            }
        }
    }
}

/// data → prose: every item of a frontmatter array needs its subsection.
fn items_need_subsections(
    doc: &Document,
    path: &ItemPath,
    req: &Requirement,
    out: &mut Vec<Diagnostic>,
) {
    // Both are guaranteed by `CorrespondenceRule::validate` at schema load.
    let (Some(container), Some(heading)) = (&req.subsection_under, &req.heading) else {
        return;
    };
    let Some(fm) = &doc.frontmatter else { return };
    let Some(items) = members_at(fm, path) else {
        return;
    };
    if items.is_empty() {
        return;
    }

    // Only top-level frontmatter keys carry a line, so nested paths anchor to
    // the outermost one.
    let line = fm.line_of(path.path.first().map_or("", String::as_str));

    let Some(span) = section_span(doc, container) else {
        // One missing container, not one diagnostic per orphaned item.
        out.push(Diagnostic::MissingSection {
            section: container.title.clone(),
        });
        return;
    };

    let present: HashSet<String> = headings_at(span, heading.level)
        .into_iter()
        .map(|(text, _)| text)
        .collect();

    for (idx, item) in items.iter().enumerate() {
        match heading.render(item) {
            Ok(text) => {
                if !present.contains(&text) {
                    out.push(Diagnostic::MissingSubsection {
                        line,
                        heading: format!("{} {text}", "#".repeat(heading.level as usize)),
                        container: container.to_string(),
                    });
                }
            }
            Err(placeholder) => out.push(Diagnostic::InvalidFieldType {
                line,
                field: path.member_name(idx),
                message: format!(
                    "has no scalar '{placeholder}', so the '{}' heading it requires can't be \
                     derived",
                    heading.as_written()
                ),
            }),
        }
    }
}

/// data → prose: every frontmatter value needs a link answering it.
///
/// The subsection direction asks for prose *about* the value; this one asks for
/// a pointer *at* it, which is the shape a value that is itself a document
/// wants: `species: [Blue Jay]` answered by `[Blue Jay](../species/Blue%20Jay.md)`
/// under `## Species`.
fn items_need_links(
    doc: &Document,
    path: &ItemPath,
    req: &Requirement,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    // Guaranteed by `CorrespondenceRule::validate` at schema load.
    let Some(section) = &req.link_in else { return };
    let Some(fm) = &doc.frontmatter else { return };
    let Some(members) = members_at(fm, path) else {
        return;
    };
    if members.is_empty() {
        return;
    }

    let line = fm.line_of(path.path.first().map_or("", String::as_str));

    let Some(span) = section_span(doc, section) else {
        // One missing section, not one diagnostic per unanswered value.
        out.push(Diagnostic::MissingSection {
            section: section.title.clone(),
        });
        return;
    };

    // A link whose target is the wrong type doesn't answer the value: the rule
    // asked for the species document, not for something else of that name.
    let linked: HashSet<String> = extract_links(span)
        .into_iter()
        .filter_map(|(url, _)| resolve_link_path(&url, ctx.source_path))
        .filter(|target| match &req.target_type {
            Some(expected) => {
                ctx.linked_docs
                    .get(target)
                    .and_then(|d| d.doc_type.as_deref())
                    == Some(expected.as_str())
            }
            None => true,
        })
        .filter_map(|target| link_identity(&target))
        .collect();

    for (idx, member) in members.iter().enumerate() {
        let Some(value) = member_value(member, path.field.as_ref()) else {
            out.push(Diagnostic::InvalidFieldType {
                line,
                field: path.member_name(idx),
                message: match &path.field {
                    Some(field) => format!(
                        "has no scalar '{field}', so the link '{section}' requires can't be named"
                    ),
                    None => {
                        format!("is not a scalar, so the link '{section}' requires can't be named")
                    }
                },
            });
            continue;
        };
        if !linked.contains(&value) {
            out.push(Diagnostic::MissingItemLink {
                line,
                item: value,
                section: section.to_string(),
                target_type: req.target_type.clone(),
            });
        }
    }
}

/// prose → data: every subsection needs a frontmatter item behind it.
fn subsections_need_items(
    doc: &Document,
    container: &SectionRef,
    req: &Requirement,
    out: &mut Vec<Diagnostic>,
) {
    let Some(path) = &req.frontmatter_item else {
        return;
    };
    let Some(span) = section_span(doc, container) else {
        return; // no section, no orphans — absence is the structure check's job
    };

    let values: HashSet<String> = doc
        .frontmatter
        .as_ref()
        .map(|fm| item_values(fm, path))
        .unwrap_or_default();

    let marker = "#".repeat(container.level as usize + 1);
    for (text, line) in headings_at(span, container.level + 1) {
        if !values.contains(&text) {
            out.push(Diagnostic::OrphanSubsection {
                line,
                heading: format!("{marker} {text}"),
                expected: path.to_string(),
            });
        }
    }
}

/// prose → prose (and prose → data): links in a section point at the right
/// type, are answered by a backlink, and are named by frontmatter.
fn links_need_targets(
    doc: &Document,
    section: &SectionRef,
    req: &Requirement,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(span) = section_span(doc, section) else {
        return;
    };
    let source_abs = normalize_path(ctx.source_path);
    let named: HashSet<String> = req
        .frontmatter_item
        .as_ref()
        .map(|path| {
            doc.frontmatter
                .as_ref()
                .map(|fm| item_values(fm, path))
                .unwrap_or_default()
        })
        .unwrap_or_default();

    for (url, line) in extract_links(span) {
        if url.starts_with("http://") || url.starts_with("https://") || url.starts_with('#') {
            continue;
        }
        let Some(target_path) = resolve_link_path(&url, ctx.source_path) else {
            continue;
        };
        let linked = ctx.linked_docs.get(&target_path);

        if let Some(expected) = &req.target_type {
            let actual = linked.and_then(|d| d.doc_type.as_deref());
            if actual != Some(expected.as_str()) {
                out.push(Diagnostic::LinkTargetTypeMismatch {
                    line,
                    url: url.clone(),
                    expected: expected.clone(),
                    actual: actual.map(str::to_string),
                });
                continue;
            }
        }

        if let Some(backlinks) = &req.backlink_in {
            // A link the walk never preloaded has no sections to search; broken
            // links are reported once by the link checker, not again per rule.
            let Some(linked) = linked else { continue };
            let answered = backlinks.0.iter().any(|sec| {
                linked.section_links.get(&sec.title).is_some_and(|urls| {
                    urls.iter().any(|back| {
                        resolve_link_path(back, &linked.path).is_some_and(|p| p == source_abs)
                    })
                })
            });
            if !answered {
                out.push(Diagnostic::MissingBacklink {
                    line,
                    url: url.clone(),
                    inverse_section: backlinks.joined_titles(),
                });
            }
        }

        if let Some(path) = &req.frontmatter_item {
            if link_identity(&target_path).is_some_and(|name| !named.contains(&name)) {
                out.push(Diagnostic::OrphanLink {
                    line,
                    url: url.clone(),
                    expected: path.to_string(),
                });
            }
        }
    }
}

/// How a link and a frontmatter value are matched: the target file's stem.
///
/// [`resolve_link_path`] has already decoded the URL, so `../species/Blue%20Jay.md`
/// answers a `Blue Jay` in frontmatter without either side knowing about the
/// other's spelling.
fn link_identity(target: &Path) -> Option<String> {
    Some(target.file_stem()?.to_string_lossy().into_owned())
}

/// disk → prose: every document of a type that exists in scope needs a link.
///
/// The other rules walk outward from the document; this one starts from the set
/// of files that exist and asks which of them nobody linked. The set comes from
/// the orchestrator's preloaded index, so validation still touches no disk.
fn docs_need_links(
    doc: &Document,
    type_name: &str,
    scope: &DirScope,
    req: &Requirement,
    ctx: &ValidateCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    // Guaranteed by `CorrespondenceRule::validate` at schema load.
    let Some(section) = &req.link_in else { return };
    // Everything compares against normalized paths: the preloaded keys and the
    // document's own path both go through the same spelling as resolved links.
    let source = normalize_path(ctx.source_path);
    let Some(base) = source.parent() else { return };
    let scope_dir = normalize_path(&base.join(&scope.dir));

    // A missing section is still a violation: the point is to name the file
    // nobody linked, and an absent heading links even less than an empty one.
    let span = section_span(doc, section);
    let linked: HashSet<PathBuf> = span
        .map(|blocks| {
            extract_links(blocks)
                .into_iter()
                .filter_map(|(url, _)| resolve_link_path(&url, ctx.source_path))
                .collect()
        })
        .unwrap_or_default();
    let line = section_heading_line(doc, section);

    // `linked_docs` is a HashMap, so collect and sort — diagnostics for one
    // document have to come out in the same order every run.
    let mut unlinked: Vec<PathBuf> = ctx
        .linked_docs
        .iter()
        .filter(|(_, info)| info.doc_type.as_deref() == Some(type_name))
        // Keys come from the walk, so a `.` or `..` in the root would spell
        // them differently from the links that point at them.
        .map(|(path, _)| normalize_path(path))
        .filter(|path| {
            *path != source && in_scope(path, &scope_dir, scope.recursive) && !linked.contains(path)
        })
        .collect();
    unlinked.sort();

    for path in unlinked {
        out.push(Diagnostic::UnlinkedDocument {
            line,
            file: path
                .strip_prefix(base)
                .unwrap_or(&path)
                .display()
                .to_string(),
            doc_type: type_name.to_string(),
            section: section.to_string(),
        });
    }
}

/// Whether a document sits in the scope's directory — or anywhere below it,
/// when the rule said `under-directory`.
fn in_scope(path: &Path, scope_dir: &Path, recursive: bool) -> bool {
    match path.parent() {
        Some(parent) if recursive => parent.starts_with(scope_dir),
        Some(parent) => parent == scope_dir,
        None => false,
    }
}

// ── Document helpers ──────────────────────────────────────────────────────────

/// The blocks belonging to a section: everything after its heading up to the
/// next heading at the same or a shallower level.
fn section_span<'a>(doc: &'a Document, sec: &SectionRef) -> Option<&'a [Block]> {
    let start = doc.blocks.iter().position(|b| match b {
        Block::Heading { level, content, .. } => {
            *level == sec.level && inlines_to_string(content) == sec.title
        }
        _ => false,
    })?;
    let end = doc.blocks[start + 1..]
        .iter()
        .position(|b| matches!(b, Block::Heading { level, .. } if *level <= sec.level))
        .map_or(doc.blocks.len(), |offset| start + 1 + offset);
    Some(&doc.blocks[start + 1..end])
}

/// The source line of a section's own heading, when the document has it.
fn section_heading_line(doc: &Document, sec: &SectionRef) -> Option<usize> {
    doc.blocks.iter().find_map(|b| match b {
        Block::Heading {
            level,
            content,
            line,
        } if *level == sec.level && inlines_to_string(content) == sec.title => Some(*line),
        _ => None,
    })
}

/// Headings at exactly `level` within a span, as `(text, line)`.
fn headings_at(blocks: &[Block], level: u8) -> Vec<(String, usize)> {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Heading {
                level: l,
                content,
                line,
            } if *l == level => Some((inlines_to_string(content), *line)),
            _ => None,
        })
        .collect()
}

// ── Frontmatter helpers ───────────────────────────────────────────────────────

/// The set a path quantifies over: every item of its array, or the single value
/// a scalar path names.
///
/// `None` when the key is absent, holds nothing, or isn't the shape the path
/// claimed — an absent optional field makes the rule vacuous, not violated.
///
/// Members come back owned: `type` isn't in `fields`, so the root value
/// `Frontmatter::get` hands back can be synthesized rather than borrowed.
fn members_at(fm: &Frontmatter, path: &ItemPath) -> Option<Vec<Value>> {
    let (first, rest) = path.path.split_first()?;
    let root = fm.get(first)?;
    let mut current = &*root;
    for segment in rest {
        current = current
            .as_mapping()
            .and_then(|m| m.get(Value::from(&**segment)))?;
    }
    match path.is_array {
        true => Some(current.as_sequence()?.to_vec()),
        false if current.is_null() => None,
        false => Some(vec![current.clone()]),
    }
}

/// The string one member of the set contributes: the field the path named on
/// it, or the member itself. `None` when there is no scalar there to read.
fn member_value(member: &Value, field: Option<&String>) -> Option<String> {
    match field {
        Some(field) => member
            .as_mapping()
            .and_then(|m| m.get(Value::from(&**field)))
            .and_then(scalar_to_string),
        None => scalar_to_string(member),
    }
}

/// The set of scalar values a path selects across every member.
fn item_values(fm: &Frontmatter, path: &ItemPath) -> HashSet<String> {
    members_at(fm, path)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|member| member_value(&member, path.field.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Selector parsing ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_frontmatter_selector() {
        let sel = Selector::parse("frontmatter.files[]").unwrap();
        assert_eq!(
            sel,
            Selector::FrontmatterItems(ItemPath {
                path: vec!["files".into()],
                is_array: true,
                field: None,
            })
        );
    }

    #[test]
    fn test_parse_subsections_selector() {
        let sel = Selector::parse("subsections-under \"## Files\"").unwrap();
        assert_eq!(
            sel,
            Selector::SubsectionsUnder(SectionRef {
                level: 2,
                title: "Files".into()
            })
        );
    }

    #[test]
    fn test_parse_links_selector() {
        let sel = Selector::parse("links-in-section \"## Cast\"").unwrap();
        assert_eq!(
            sel,
            Selector::LinksInSection(SectionRef {
                level: 2,
                title: "Cast".into()
            })
        );
    }

    #[test]
    fn test_parse_docs_of_type_selector() {
        let sel = Selector::parse("docs-of-type tvseason in-directory \".\"").unwrap();
        assert_eq!(
            sel,
            Selector::DocsOfType {
                type_name: "tvseason".into(),
                scope: DirScope {
                    dir: PathBuf::from("."),
                    recursive: false,
                },
            }
        );
    }

    #[test]
    fn test_parse_docs_of_type_under_directory_is_recursive() {
        let sel = Selector::parse("docs-of-type tvseason under-directory \"seasons\"").unwrap();
        assert_eq!(
            sel,
            Selector::DocsOfType {
                type_name: "tvseason".into(),
                scope: DirScope {
                    dir: PathBuf::from("seasons"),
                    recursive: true,
                },
            }
        );
    }

    #[test]
    fn test_docs_of_type_without_a_scope_rejected() {
        let err = Selector::parse("docs-of-type tvseason").unwrap_err();
        assert!(err.to_string().contains("needs a scope"), "{err}");
    }

    #[test]
    fn test_docs_of_type_with_an_unknown_scope_rejected() {
        let err = Selector::parse("docs-of-type tvseason in-project \".\"").unwrap_err();
        assert!(err.to_string().contains("unknown scope"), "{err}");
    }

    #[test]
    fn test_docs_of_type_with_an_absolute_directory_rejected() {
        let err = Selector::parse("docs-of-type tvseason in-directory \"/shows\"").unwrap_err();
        assert!(err.to_string().contains("must be relative"), "{err}");
    }

    #[test]
    fn test_docs_of_type_without_a_type_rejected() {
        let err = Selector::parse("docs-of-type").unwrap_err();
        assert!(err.to_string().contains("needs a type name"), "{err}");
    }

    #[test]
    fn test_unknown_selector_rejected() {
        let err = Selector::parse("wibbles-under \"## Files\"").unwrap_err();
        assert!(err.to_string().contains("unknown selector"), "{err}");
    }

    #[test]
    fn test_parse_scalar_frontmatter_selector() {
        // No `[]`: one value, which the rules read as a set of one.
        let sel = Selector::parse("frontmatter.location").unwrap();
        assert_eq!(
            sel,
            Selector::FrontmatterItems(ItemPath {
                path: vec!["location".into()],
                is_array: false,
                field: None,
            })
        );
    }

    #[test]
    fn test_scalar_frontmatter_path_may_be_nested() {
        assert_eq!(
            ItemPath::parse("meta.location").unwrap().path,
            ["meta", "location"]
        );
    }

    #[test]
    fn test_half_written_array_marker_rejected() {
        let err = ItemPath::parse("files[").unwrap_err();
        assert!(err.to_string().contains("must spell its array"), "{err}");
    }

    #[test]
    fn test_section_ref_needs_a_marker() {
        let err = SectionRef::parse("Files").unwrap_err();
        assert!(err.to_string().contains("heading with its marker"), "{err}");
    }

    #[test]
    fn test_item_path_roundtrips_through_display() {
        for spelling in [
            "files[]",
            "files[].quality",
            "meta.files[].quality",
            "location",
            "meta.location",
        ] {
            assert_eq!(ItemPath::parse(spelling).unwrap().to_string(), spelling);
        }
    }

    #[test]
    fn test_item_path_strips_frontmatter_prefix() {
        assert_eq!(
            ItemPath::parse("frontmatter.files[].quality").unwrap(),
            ItemPath::parse("files[].quality").unwrap()
        );
    }

    // ── Heading templates ─────────────────────────────────────────────────────

    #[test]
    fn test_heading_template_interpolates_a_field() {
        let tpl = HeadingTemplate::parse("### {quality}").unwrap();
        assert_eq!(tpl.level, 3);
        let item: Value = serde_yaml::from_str("quality: WEBRip-1080p").unwrap();
        assert_eq!(tpl.render(&item).unwrap(), "WEBRip-1080p");
    }

    #[test]
    fn test_heading_template_keeps_literal_text() {
        let tpl = HeadingTemplate::parse("### Copy {n}: {quality}").unwrap();
        let item: Value = serde_yaml::from_str("n: 2\nquality: 4k").unwrap();
        assert_eq!(tpl.render(&item).unwrap(), "Copy 2: 4k");
    }

    #[test]
    fn test_heading_template_dot_renders_a_scalar_item() {
        let tpl = HeadingTemplate::parse("### {.}").unwrap();
        assert_eq!(tpl.render(&Value::from("1080p")).unwrap(), "1080p");
    }

    #[test]
    fn test_heading_template_reports_the_missing_placeholder() {
        let tpl = HeadingTemplate::parse("### {quality}").unwrap();
        let item: Value = serde_yaml::from_str("size: 12").unwrap();
        assert_eq!(tpl.render(&item).unwrap_err(), "quality");
    }

    #[test]
    fn test_placeholderless_heading_template_rejected() {
        let err = HeadingTemplate::parse("### Files").unwrap_err();
        assert!(
            err.to_string().contains("no '{field}' placeholder"),
            "{err}"
        );
    }

    #[test]
    fn test_unclosed_placeholder_rejected() {
        let err = HeadingTemplate::parse("### {quality").unwrap_err();
        assert!(err.to_string().contains("unclosed"), "{err}");
    }

    // ── Rule shape validation ─────────────────────────────────────────────────

    fn rule(yaml: &str) -> Result<CorrespondenceRule> {
        let rule: CorrespondenceRule = serde_yaml::from_str(yaml)?;
        rule.validate()?;
        Ok(rule)
    }

    #[test]
    fn test_valid_rules_load() {
        rule("each: frontmatter.files[]\nrequires:\n  subsection-under: \"## Files\"\n  heading: \"### {quality}\"\n").unwrap();
        rule("each: subsections-under \"## Files\"\nrequires:\n  frontmatter-item: files[].quality\n").unwrap();
        rule("each: links-in-section \"## Cast\"\nrequires:\n  target_type: personality\n  backlink-in: \"## Movies\"\n").unwrap();
        rule("each: docs-of-type tvseason in-directory \".\"\nrequires:\n  link-in: \"## Seasons\"\n").unwrap();
        rule("each: frontmatter.species[]\nrequires:\n  link-in: \"## Species\"\n  target_type: species\n").unwrap();
        rule("each: frontmatter.location\nrequires:\n  link-in: \"## Location\"\n").unwrap();
        rule("each: links-in-section \"## Species\"\nrequires:\n  frontmatter-item: species[]\n")
            .unwrap();
    }

    #[test]
    fn test_frontmatter_link_rule_rejects_subsection_keys() {
        // Two obligations, two rules — a link isn't a place to write prose.
        let err = rule("each: frontmatter.species[]\nrequires:\n  link-in: \"## Species\"\n  subsection-under: \"## Species\"\n  heading: \"### {.}\"\n").unwrap_err();
        assert!(err.to_string().contains("not meaningful here"), "{err}");
    }

    #[test]
    fn test_target_type_needs_link_in_on_a_frontmatter_rule() {
        let err = rule("each: frontmatter.species[]\nrequires:\n  subsection-under: \"## Species\"\n  heading: \"### {.}\"\n  target_type: species\n").unwrap_err();
        assert!(err.to_string().contains("not meaningful here"), "{err}");
    }

    #[test]
    fn test_frontmatter_rule_error_names_both_forms() {
        let err = rule("each: frontmatter.species[]\nrequires: {}\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("subsection-under"), "{msg}");
        assert!(msg.contains("link-in"), "{msg}");
    }

    #[test]
    fn test_links_rule_error_names_frontmatter_item() {
        let err = rule("each: links-in-section \"## Species\"\nrequires: {}\n").unwrap_err();
        assert!(err.to_string().contains("'frontmatter-item'"), "{err}");
    }

    #[test]
    fn test_docs_of_type_rule_needs_link_in() {
        let err = rule(
            "each: docs-of-type tvseason in-directory \".\"\nrequires:\n  target_type: tvseason\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not meaningful here"), "{err}");
    }

    #[test]
    fn test_docs_of_type_rule_with_no_requirement_rejected() {
        let err =
            rule("each: docs-of-type tvseason in-directory \".\"\nrequires: {}\n").unwrap_err();
        assert!(err.to_string().contains("requires 'link-in'"), "{err}");
    }

    #[test]
    fn test_link_in_needs_a_docs_of_type_selector() {
        let err =
            rule("each: links-in-section \"## Seasons\"\nrequires:\n  link-in: \"## Seasons\"\n")
                .unwrap_err();
        assert!(err.to_string().contains("not meaningful here"), "{err}");
    }

    #[test]
    fn test_frontmatter_rule_needs_subsection_and_heading() {
        let err = rule("each: frontmatter.files[]\nrequires:\n  subsection-under: \"## Files\"\n")
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("requires 'subsection-under' and 'heading'"),
            "{err}"
        );
    }

    #[test]
    fn test_mismatched_requirement_rejected() {
        let err = rule("each: frontmatter.files[]\nrequires:\n  target_type: movie\n").unwrap_err();
        assert!(err.to_string().contains("not meaningful here"), "{err}");
    }

    #[test]
    fn test_heading_must_be_deeper_than_container() {
        let err = rule("each: frontmatter.files[]\nrequires:\n  subsection-under: \"## Files\"\n  heading: \"## {quality}\"\n").unwrap_err();
        assert!(err.to_string().contains("must be deeper"), "{err}");
    }

    #[test]
    fn test_link_rule_needs_a_constraint() {
        let err = rule("each: links-in-section \"## Cast\"\nrequires: {}\n").unwrap_err();
        assert!(err.to_string().contains("requires 'target_type'"), "{err}");
    }

    #[test]
    fn test_unknown_requirement_key_rejected() {
        let err = rule("each: frontmatter.files[]\nrequires:\n  subsection_under: \"## Files\"\n")
            .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn test_backlink_in_accepts_one_or_many() {
        let one = rule(
            "each: links-in-section \"## Leads To\"\nrequires:\n  backlink-in: \"## Enabled By\"\n",
        )
        .unwrap();
        assert_eq!(one.requires.backlink_in.unwrap().0.len(), 1);
        let many = rule("each: links-in-section \"## Leads To\"\nrequires:\n  backlink-in: [\"## Enabled By\", \"## Leads To\"]\n").unwrap();
        assert_eq!(many.requires.backlink_in.unwrap().0.len(), 2);
    }

    // ── Document helpers ──────────────────────────────────────────────────────

    fn doc(md: &str) -> Document {
        crate::parse::parse(md)
    }

    #[test]
    fn test_section_span_stops_at_the_next_same_level_heading() {
        let d = doc("# T\n\n## Files\n\n### A\n\n### B\n\n## Details\n\n### C\n");
        let span = section_span(
            &d,
            &SectionRef {
                level: 2,
                title: "Files".into(),
            },
        )
        .unwrap();
        let headings: Vec<String> = headings_at(span, 3).into_iter().map(|(t, _)| t).collect();
        assert_eq!(headings, ["A", "B"]);
    }

    #[test]
    fn test_section_span_absent_section() {
        let d = doc("# T\n\n## Details\n");
        assert!(section_span(
            &d,
            &SectionRef {
                level: 2,
                title: "Files".into()
            }
        )
        .is_none());
    }

    #[test]
    fn test_section_heading_line_finds_the_heading_itself() {
        let d = doc("# T\n\n## Seasons\n\n- [Season 1](Season%201.md)\n");
        let line = section_heading_line(
            &d,
            &SectionRef {
                level: 2,
                title: "Seasons".into(),
            },
        );
        assert_eq!(line, Some(3));
    }

    // ── Scope matching ────────────────────────────────────────────────────────

    #[test]
    fn test_in_directory_is_siblings_only() {
        let scope = Path::new("shows/3rd Rock");
        assert!(in_scope(
            Path::new("shows/3rd Rock/Season 1.md"),
            scope,
            false
        ));
        assert!(!in_scope(
            Path::new("shows/3rd Rock/Season 1/Episodes.md"),
            scope,
            false
        ));
        assert!(!in_scope(
            Path::new("shows/Other/Season 1.md"),
            scope,
            false
        ));
    }

    #[test]
    fn test_under_directory_descends() {
        let scope = Path::new("shows/3rd Rock");
        assert!(in_scope(
            Path::new("shows/3rd Rock/Season 1.md"),
            scope,
            true
        ));
        assert!(in_scope(
            Path::new("shows/3rd Rock/Season 1/Episodes.md"),
            scope,
            true
        ));
        assert!(!in_scope(Path::new("shows/Other/Season 1.md"), scope, true));
    }

    #[test]
    fn test_scope_of_a_document_at_the_root() {
        // A document with no directory of its own: `.` is the empty path, and
        // its siblings are the paths with no directory either.
        let scope = Path::new("");
        assert!(in_scope(Path::new("Season 1.md"), scope, false));
        assert!(!in_scope(Path::new("shows/Season 1.md"), scope, false));
        assert!(in_scope(Path::new("shows/Season 1.md"), scope, true));
    }

    #[test]
    fn test_item_values_collects_the_selected_field() {
        let d = doc("---\ntype: movie\nfiles:\n  - quality: 1080p\n  - quality: 4k\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        let values = item_values(fm, &ItemPath::parse("files[].quality").unwrap());
        assert_eq!(values, HashSet::from(["1080p".into(), "4k".into()]));
    }

    #[test]
    fn test_item_values_of_a_scalar_array() {
        let d = doc("---\ntype: movie\nqualities:\n  - 1080p\n  - 4k\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        let values = item_values(fm, &ItemPath::parse("qualities[]").unwrap());
        assert_eq!(values, HashSet::from(["1080p".into(), "4k".into()]));
    }

    #[test]
    fn test_item_values_of_a_scalar_field() {
        let d = doc("---\ntype: bird\nlocation: Sunnyvale\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        let values = item_values(fm, &ItemPath::parse("location").unwrap());
        assert_eq!(values, HashSet::from(["Sunnyvale".into()]));
    }

    #[test]
    fn test_members_at_walks_nested_maps() {
        let d = doc("---\ntype: movie\nmeta:\n  files:\n    - quality: 4k\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        assert_eq!(
            members_at(fm, &ItemPath::parse("meta.files[].quality").unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_members_at_missing_key_is_none() {
        let d = doc("---\ntype: movie\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        assert!(members_at(fm, &ItemPath::parse("files[]").unwrap()).is_none());
    }

    #[test]
    fn test_members_at_a_scalar_is_a_set_of_one() {
        let d = doc("---\ntype: bird\nlocation: Sunnyvale\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        let members = members_at(fm, &ItemPath::parse("location").unwrap()).unwrap();
        assert_eq!(members.len(), 1);
    }

    #[test]
    fn test_members_at_a_null_scalar_is_none() {
        // `location:` with nothing after it is an absent value, not a violation.
        let d = doc("---\ntype: bird\nlocation:\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        assert!(members_at(fm, &ItemPath::parse("location").unwrap()).is_none());
    }

    #[test]
    fn test_members_at_wrong_shape_is_none() {
        // `[]` on a scalar, and no `[]` is still fine on an array's own value.
        let d = doc("---\ntype: bird\nlocation: Sunnyvale\n---\n\n# T\n");
        let fm = d.frontmatter.as_ref().unwrap();
        assert!(members_at(fm, &ItemPath::parse("location[]").unwrap()).is_none());
    }

    // ── Link identity ─────────────────────────────────────────────────────────

    #[test]
    fn test_link_identity_is_the_decoded_file_stem() {
        let target = resolve_link_path("../species/Blue%20Jay.md", Path::new("/b/photos/x.md"));
        assert_eq!(
            link_identity(&target.unwrap()).as_deref(),
            Some("Blue Jay"),
            "a %20-encoded link should answer an unencoded frontmatter value"
        );
    }
}
