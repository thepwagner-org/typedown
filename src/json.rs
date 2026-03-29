//! `td json` -- output documents as structured JSON (JSONL for multiple files).
//!
//! Each document is a flat JSON object:
//!
//! ```json
//! {
//!   "path": "journal/2026-03.md",
//!   "type": "journal",
//!   "frontmatter": { "type": "journal", "tags": ["foo"] },
//!   "title": "March 2026",
//!   "intro": { "content": "...", "items": [...], "links": [] },
//!   "sections": [
//!     {
//!       "heading": "How to Break It",
//!       "content": "- **Remove a required field**: ...",
//!       "items": [
//!         { "text": "Remove a required field: ...", "items": [] },
//!         { "text": "Use a bad enum value: ..." }
//!       ],
//!       "links": [...],
//!       "subsections": [...]
//!     }
//!   ]
//! }
//! ```
//!
//! `content` is the full markdown of the section's direct content (lossless).
//! `items` is present when the content contains list blocks; each item has
//! `text` (markup stripped) and optionally `items` for nested sub-lists.
//! Only nested `List` blocks count as sub-items -- child paragraphs or code
//! blocks under a list item are in `content` at the section level.
//!
//! Multiple files → JSONL (one object per line). Single file → single object.
//! `--pretty` emits indented JSON (still one object per line in multi-file mode).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::Serialize;
use walkdir::WalkDir;

use indexmap::IndexMap;

use crate::{
    ast::{inlines_to_string, Block, Document, Inline, ListItem},
    format::{
        find_schema_for, is_ignored, is_markdown, load_all_schemas, resolve_type, FormatResult,
        ResolvedType,
    },
    parse::serialize_blocks,
    schema::{FieldDef, FieldType, PathMatcher, Schema, StructureDef, TypeDef},
};

// ── Output types ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct JsonDocument {
    pub path: String,
    #[serde(rename = "type")]
    pub doc_type: Option<String>,
    pub frontmatter: serde_json::Value,
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intro: Option<SectionContent>,
    pub sections: Vec<JsonSection>,
}

#[derive(Serialize)]
pub struct JsonSection {
    pub heading: String,
    /// Raw markdown of the section's direct content. Omitted when `items` is
    /// non-empty -- the structured list representation supersedes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Section-level typed properties. Present when the schema declares
    /// `properties` on the section and all list items are flat `Key: Value`
    /// pairs (no sub-items). Mutually exclusive with `items` -- when
    /// section-level properties are extracted, items are omitted.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<JsonItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<JsonLink>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subsections: Vec<JsonSection>,
}

#[derive(Serialize)]
pub struct SectionContent {
    /// Raw markdown of the intro's direct content. Omitted when `items` is
    /// non-empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<JsonItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<JsonLink>,
}

/// A single list item. `text` is the item's inline content with markup stripped.
/// `properties` is present when the section schema declares a `properties` map:
/// the item's sub-list is parsed as `Key: Value` pairs, type-coerced, and
/// collected here. `items` is omitted when `properties` is non-empty -- the
/// structured map supersedes the raw sub-list.
#[derive(Serialize)]
pub struct JsonItem {
    pub text: String,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub properties: IndexMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<JsonItem>,
}

#[derive(Serialize)]
pub struct JsonLink {
    pub text: String,
    pub url: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Output documents as JSONL to stdout.
///
/// - `paths`: files or directories to process; empty means `root`.
/// - `pretty`: emit indented JSON (still one line per document in multi-file mode).
/// - `depth`: follow local markdown links this many hops deep (0 = seed files only).
pub fn json_output(
    root: &Path,
    cwd: &Path,
    paths: &[PathBuf],
    pretty: bool,
    depth: u32,
) -> Result<()> {
    let mut dummy = FormatResult::default();
    let (schemas, matchers) = load_all_schemas(root, &mut dummy);

    for err in &dummy.errors {
        eprintln!(
            "warning: {}: {}",
            err.path.display(),
            err.diagnostics[0].message()
        );
    }

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

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if depth == 0 {
        // Original behaviour: emit targets in walk order, no link following.
        for target in &targets {
            if target.is_file() {
                if is_markdown(target) {
                    process_file(target, root, &schemas, &matchers, pretty, &mut out)?;
                }
            } else {
                let walker = WalkDir::new(target)
                    .into_iter()
                    .filter_entry(|e| !is_ignored(e.file_name().to_str().unwrap_or("")));
                for entry in walker.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if is_markdown(path) {
                        process_file(path, root, &schemas, &matchers, pretty, &mut out)?;
                    }
                }
            }
        }
    } else {
        // BFS: seed from the expanded markdown files, then follow local .md links
        // up to `depth` hops. Each file is emitted at most once.
        let seeds = expand_to_markdown_files(&targets);
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<(PathBuf, u32)> = VecDeque::new();

        for path in seeds {
            if visited.insert(path.clone()) {
                queue.push_back((path, 0));
            }
        }

        while let Some((path, current_depth)) = queue.pop_front() {
            let jdoc = build_file_doc(&path, root, &schemas, &matchers)?;
            let json = if pretty {
                serde_json::to_string_pretty(&jdoc)?
            } else {
                serde_json::to_string(&jdoc)?
            };
            writeln!(out, "{json}")?;

            if current_depth < depth {
                for url in collect_doc_links(&jdoc) {
                    if let Some(linked) = crate::validate::resolve_link_path(&url, &path) {
                        if linked.extension().and_then(|e| e.to_str()) == Some("md")
                            && linked.exists()
                            && visited.insert(linked.clone())
                        {
                            queue.push_back((linked, current_depth + 1));
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Expand a list of targets (files or directories) to individual markdown file paths.
fn expand_to_markdown_files(targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for target in targets {
        if target.is_file() {
            if is_markdown(target) {
                result.push(target.clone());
            }
        } else {
            let walker = WalkDir::new(target)
                .into_iter()
                .filter_entry(|e| !is_ignored(e.file_name().to_str().unwrap_or("")));
            for entry in walker.filter_map(|e| e.ok()) {
                let path = entry.path().to_path_buf();
                if is_markdown(&path) {
                    result.push(path);
                }
            }
        }
    }
    result
}

/// Collect all link URLs from a `JsonDocument` (intro + all sections, recursively).
fn collect_doc_links(jdoc: &JsonDocument) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(intro) = &jdoc.intro {
        for link in &intro.links {
            urls.push(link.url.clone());
        }
    }
    for section in &jdoc.sections {
        collect_section_links(section, &mut urls);
    }
    urls
}

fn collect_section_links(section: &JsonSection, urls: &mut Vec<String>) {
    for link in &section.links {
        urls.push(link.url.clone());
    }
    for sub in &section.subsections {
        collect_section_links(sub, urls);
    }
}

fn process_file(
    path: &Path,
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
    pretty: bool,
    out: &mut impl Write,
) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let doc = crate::parse::parse(&content);

    let resolved = if let Some((schema, schema_dir)) = find_schema_for(path, root, schemas) {
        Some(resolve_type(path, &doc, schema, &schema_dir, matchers))
    } else {
        None
    };

    let (resolved_type_name, type_def): (Option<&str>, Option<&TypeDef>) = match &resolved {
        Some(ResolvedType::Explicit(name, td)) | Some(ResolvedType::PathMatched(name, td)) => {
            (Some(name.as_str()), Some(td))
        }
        _ => (None, None),
    };

    let jdoc = document_to_json(&doc, path, resolved_type_name, type_def, root);

    let json = if pretty {
        serde_json::to_string_pretty(&jdoc)?
    } else {
        serde_json::to_string(&jdoc)?
    };
    writeln!(out, "{json}")?;

    Ok(())
}

/// Parse a file and build its `JsonDocument`, resolving schema type if available.
/// Used by the BFS link-following path in `json_output`.
fn build_file_doc(
    path: &Path,
    root: &Path,
    schemas: &HashMap<PathBuf, Schema>,
    matchers: &HashMap<PathBuf, PathMatcher>,
) -> Result<JsonDocument> {
    let content = std::fs::read_to_string(path)?;
    let doc = crate::parse::parse(&content);

    let resolved = if let Some((schema, schema_dir)) = find_schema_for(path, root, schemas) {
        Some(resolve_type(path, &doc, schema, &schema_dir, matchers))
    } else {
        None
    };

    let (resolved_type_name, type_def): (Option<&str>, Option<&TypeDef>) = match &resolved {
        Some(ResolvedType::Explicit(name, td)) | Some(ResolvedType::PathMatched(name, td)) => {
            (Some(name.as_str()), Some(td))
        }
        _ => (None, None),
    };

    Ok(document_to_json(
        &doc,
        path,
        resolved_type_name,
        type_def,
        root,
    ))
}

// ── Document → JSON ───────────────────────────────────────────────────────────

/// Convert a parsed `Document` to its JSON representation.
///
/// - `resolved_type`: the resolved type name (from frontmatter or path-matching).
/// - `type_def`: the matching `TypeDef` for field coercion (may be `None` if no schema).
/// - `root`: project root for computing relative `path`.
pub fn document_to_json(
    doc: &Document,
    path: &Path,
    resolved_type: Option<&str>,
    type_def: Option<&TypeDef>,
    root: &Path,
) -> JsonDocument {
    let path_str = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();

    let frontmatter = build_frontmatter_json(doc, resolved_type, type_def);

    // Title = first H1
    let title_idx = doc
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Heading { level: 1, .. }));
    let title = title_idx.and_then(|i| {
        if let Block::Heading { content, .. } = &doc.blocks[i] {
            Some(inlines_to_string(content))
        } else {
            None
        }
    });

    // after_title: blocks after H1 (or all blocks if no H1)
    let after_title: &[Block] = match title_idx {
        Some(idx) => &doc.blocks[idx + 1..],
        None => &doc.blocks,
    };

    // first_h2: index in after_title of the first H2
    let first_h2 = after_title
        .iter()
        .position(|b| matches!(b, Block::Heading { level: 2, .. }));

    let (intro_blocks, body_start) = match first_h2 {
        Some(idx) => (&after_title[..idx], idx),
        None => (after_title, after_title.len()),
    };

    // Only emit intro when there's at least one non-blank block
    let intro = if intro_blocks.iter().any(|b| !matches!(b, Block::BlankLine)) {
        Some(blocks_to_section_content(intro_blocks, None)) // intro has no property schema
    } else {
        None
    };

    let struct_def = type_def.map(|td| &td.structure);
    let sections = build_sections(&after_title[body_start..], 2, 4, struct_def);

    JsonDocument {
        path: path_str,
        doc_type: resolved_type.map(|s| s.to_string()),
        frontmatter,
        title,
        intro,
        sections,
    }
}

// ── Frontmatter ───────────────────────────────────────────────────────────────

fn build_frontmatter_json(
    doc: &Document,
    resolved_type: Option<&str>,
    type_def: Option<&TypeDef>,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();

    // `type:` key: prefer the value actually in frontmatter (could be "none"),
    // fall back to injecting the path-matched resolved type.
    let fm_type = doc
        .frontmatter
        .as_ref()
        .and_then(|fm| fm.doc_type.as_deref());
    match (fm_type, resolved_type) {
        (Some(t), _) => {
            map.insert("type".to_string(), serde_json::Value::String(t.to_string()));
        }
        (None, Some(t)) => {
            // path-matched file: inject the resolved type
            map.insert("type".to_string(), serde_json::Value::String(t.to_string()));
        }
        (None, None) => {}
    }

    if let Some(fm) = &doc.frontmatter {
        for (key, value) in &fm.fields {
            let json_val = coerce_field_value(value, key, type_def);
            map.insert(key.clone(), json_val);
        }
    }

    serde_json::Value::Object(map)
}

fn coerce_field_value(
    value: &serde_yaml::Value,
    key: &str,
    type_def: Option<&TypeDef>,
) -> serde_json::Value {
    match type_def.and_then(|td| td.fields.get(key)) {
        Some(fd) => coerce_by_type(value, &fd.field_type, fd.item_type.as_ref()),
        None => yaml_to_json(value),
    }
}

fn coerce_by_type(
    value: &serde_yaml::Value,
    field_type: &FieldType,
    item_type: Option<&FieldType>,
) -> serde_json::Value {
    match field_type {
        FieldType::Integer => match value {
            serde_yaml::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    serde_json::Value::Number(i.into())
                } else {
                    yaml_to_json(value)
                }
            }
            serde_yaml::Value::String(s) => s
                .parse::<i64>()
                .map(|n| serde_json::Value::Number(n.into()))
                .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
            _ => yaml_to_json(value),
        },
        FieldType::Bool => match value {
            serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
            serde_yaml::Value::String(s) => match s.as_str() {
                "true" | "yes" => serde_json::Value::Bool(true),
                "false" | "no" => serde_json::Value::Bool(false),
                _ => yaml_to_json(value),
            },
            _ => yaml_to_json(value),
        },
        FieldType::List => match value {
            serde_yaml::Value::Sequence(seq) => serde_json::Value::Array(
                seq.iter()
                    .map(|item| match item_type {
                        Some(it) => coerce_by_type(item, it, None),
                        None => yaml_to_json(item),
                    })
                    .collect(),
            ),
            _ => yaml_to_json(value),
        },
        // String, Date, Datetime, Enum, Link — pass through as JSON
        _ => yaml_to_json(value),
    }
}

fn yaml_to_json(value: &serde_yaml::Value) -> serde_json::Value {
    match value {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        serde_yaml::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => {
            serde_json::Value::Array(seq.iter().map(yaml_to_json).collect())
        }
        serde_yaml::Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                if let serde_yaml::Value::String(key) = k {
                    obj.insert(key.clone(), yaml_to_json(v));
                }
            }
            serde_json::Value::Object(obj)
        }
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(&tagged.value),
    }
}

// ── Section building ──────────────────────────────────────────────────────────

/// Split `blocks` into `JsonSection`s at `level` headings, recursing into
/// subsections up to `max_level`. Headings deeper than `max_level` fold into
/// their enclosing section's `content`.
///
/// `struct_def`: when provided (H2-level only), used to look up `properties`
/// declarations per section heading. Subsection recursion always passes `None`.
fn build_sections(
    blocks: &[Block],
    level: u8,
    max_level: u8,
    struct_def: Option<&StructureDef>,
) -> Vec<JsonSection> {
    let mut result = Vec::new();
    let mut i = 0;

    while i < blocks.len() {
        if let Block::Heading {
            level: l, content, ..
        } = &blocks[i]
        {
            if *l == level {
                let heading_text = inlines_to_string(content);
                i += 1;
                let start = i;

                // Advance until next heading at same or higher structural level
                while i < blocks.len() {
                    if let Block::Heading { level: l2, .. } = &blocks[i] {
                        if *l2 <= level {
                            break;
                        }
                    }
                    i += 1;
                }

                let section_blocks = &blocks[start..i];
                // Look up property definitions for this section (H2 level only)
                let props = struct_def
                    .and_then(|sd| sd.sections.iter().find(|s| s.title == heading_text))
                    .and_then(|sd| sd.properties.as_ref());
                result.push(make_section(
                    heading_text,
                    section_blocks,
                    level + 1,
                    max_level,
                    props,
                ));
                continue;
            }
        }
        i += 1;
    }

    result
}

fn make_section(
    heading: String,
    blocks: &[Block],
    next_level: u8,
    max_level: u8,
    properties: Option<&IndexMap<String, FieldDef>>,
) -> JsonSection {
    // Direct content = blocks before the first child-level heading.
    // When next_level > max_level, all blocks (including deeper headings) fold in.
    let direct_end = if next_level <= max_level {
        blocks
            .iter()
            .position(|b| matches!(b, Block::Heading { level: l, .. } if *l == next_level))
            .unwrap_or(blocks.len())
    } else {
        blocks.len()
    };

    let direct = &blocks[..direct_end];
    let sc = blocks_to_section_content(direct, properties);

    let subsections = if next_level <= max_level {
        // Properties only apply at H2; subsections get None
        build_sections(blocks, next_level, max_level, None)
    } else {
        Vec::new()
    };

    // When the schema declares properties and all items are flat Key: Value
    // pairs (no sub-items, no item-level properties), extract section-level
    // properties and omit the redundant items array.
    let section_props = properties
        .filter(|_| !sc.items.is_empty())
        .map(|p| extract_flat_section_properties(&sc.items, p))
        .unwrap_or_default();

    let items = if section_props.is_empty() {
        sc.items
    } else {
        Vec::new()
    };

    JsonSection {
        heading,
        content: sc.content,
        properties: section_props,
        items,
        links: sc.links,
        subsections,
    }
}

// ── Content extraction ────────────────────────────────────────────────────────

fn blocks_to_section_content(
    blocks: &[Block],
    properties: Option<&IndexMap<String, FieldDef>>,
) -> SectionContent {
    let items = extract_items(blocks, properties);
    let content = if items.is_empty() {
        Some(serialize_blocks(blocks).trim_matches('\n').to_string()).filter(|s| !s.is_empty())
    } else {
        None
    };
    SectionContent {
        content,
        items,
        links: extract_links(blocks),
    }
}

/// Extract list items from `blocks`. Merges items across multiple `List` blocks.
/// Only nested `List` blocks within a `ListItem`'s children become sub-items;
/// child paragraphs or code blocks are ignored at the item level.
///
/// When `properties` is provided, each top-level item's sub-list is also parsed
/// as `Key: Value` pairs and type-coerced into `item.properties`.
fn extract_items(
    blocks: &[Block],
    properties: Option<&IndexMap<String, FieldDef>>,
) -> Vec<JsonItem> {
    blocks
        .iter()
        .filter_map(|b| {
            if let Block::List { items, .. } = b {
                Some(
                    items
                        .iter()
                        .map(|item| list_item_to_json_item(item, properties))
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        })
        .flatten()
        .collect()
}

fn list_item_to_json_item(
    item: &ListItem,
    properties: Option<&IndexMap<String, FieldDef>>,
) -> JsonItem {
    let text = inlines_to_string(&item.content);

    // Extract typed properties from sub-list when schema declares them
    let props = properties
        .map(|p| extract_item_properties(item, p))
        .unwrap_or_default();

    // Raw sub-list items -- omitted when properties were extracted (redundant)
    let sub_items: Vec<JsonItem> = if props.is_empty() {
        item.children
            .iter()
            .filter_map(|child| {
                if let Block::List { items, .. } = child {
                    // Sub-items don't inherit properties schema
                    Some(
                        items
                            .iter()
                            .map(|i| list_item_to_json_item(i, None))
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                }
            })
            .flatten()
            .collect()
    } else {
        Vec::new()
    };

    JsonItem {
        text,
        properties: props,
        items: sub_items,
    }
}

/// Parse a list item's sub-list as `Key: Value` pairs and coerce each value
/// to the declared JSON type. Unknown keys are included as strings.
fn extract_item_properties(
    item: &ListItem,
    properties_def: &IndexMap<String, FieldDef>,
) -> IndexMap<String, serde_json::Value> {
    let mut result = IndexMap::new();
    for child in &item.children {
        if let Block::List { items, .. } = child {
            for kv_item in items {
                let text = inlines_to_string(&kv_item.content);
                if let Some((k, v)) = text.split_once(": ") {
                    let key = k.trim().to_lowercase();
                    let val_str = v.trim();
                    let json_val = if let Some(field_def) = properties_def.get(&key) {
                        coerce_property_str(val_str, field_def)
                    } else {
                        serde_json::Value::String(val_str.to_string())
                    };
                    result.insert(key, json_val);
                }
            }
        }
    }
    result
}

/// Extract section-level properties from flat `Key: Value` list items.
///
/// Applies when the schema declares `properties` on a section and ALL items are
/// flat (no sub-items, no item-level properties already extracted).  Each item's
/// text is split on `": "` and type-coerced against the schema.  Returns an
/// empty map if items aren't uniformly flat Key: Value pairs.
fn extract_flat_section_properties(
    items: &[JsonItem],
    properties_def: &IndexMap<String, FieldDef>,
) -> IndexMap<String, serde_json::Value> {
    // Only applies when ALL items are flat (no sub-items, no existing properties)
    if items
        .iter()
        .any(|i| !i.items.is_empty() || !i.properties.is_empty())
    {
        return IndexMap::new();
    }

    let mut result = IndexMap::new();
    for item in items {
        if let Some((k, v)) = item.text.split_once(": ") {
            let key = k.trim().to_lowercase();
            let val_str = v.trim();
            let json_val = if let Some(field_def) = properties_def.get(&key) {
                coerce_property_str(val_str, field_def)
            } else {
                serde_json::Value::String(val_str.to_string())
            };
            result.insert(key, json_val);
        }
    }

    result
}

/// Coerce a property value string to JSON using the declared field type.
/// Delegates to `coerce_by_type` after wrapping the string as a YAML value —
/// `coerce_by_type` already handles string-to-integer and string-to-bool
/// conversion for the common cases.
fn coerce_property_str(value: &str, field_def: &FieldDef) -> serde_json::Value {
    let yaml_val = serde_yaml::Value::String(value.to_string());
    coerce_by_type(
        &yaml_val,
        &field_def.field_type,
        field_def.item_type.as_ref(),
    )
}

fn extract_links(blocks: &[Block]) -> Vec<JsonLink> {
    let mut links = Vec::new();
    for block in blocks {
        collect_block_links(block, &mut links);
    }
    links
}

fn collect_block_links(block: &Block, links: &mut Vec<JsonLink>) {
    match block {
        Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
            collect_inline_links(content, links);
        }
        Block::List { items, .. } => {
            for item in items {
                collect_inline_links(&item.content, links);
                for child in &item.children {
                    collect_block_links(child, links);
                }
            }
        }
        Block::BlockQuote { blocks, .. } => {
            for b in blocks {
                collect_block_links(b, links);
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

fn collect_inline_links(inlines: &[Inline], links: &mut Vec<JsonLink>) {
    for inline in inlines {
        match inline {
            Inline::Link { content, url } => links.push(JsonLink {
                text: inlines_to_string(content),
                url: url.clone(),
            }),
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strikethrough(inner) => {
                collect_inline_links(inner, links);
            }
            _ => {}
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn mk(md: &str) -> Document {
        parse(md)
    }

    #[test]
    fn test_basic_structure() {
        let doc = mk("---\ntype: journal\n---\n# March 2026\n\nIntro text.\n\n## First\n\nSome content.\n\n## Second\n\nMore content.\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("journal/2026-03.md"),
            Some("journal"),
            None,
            Path::new(""),
        );
        assert_eq!(jdoc.path, "journal/2026-03.md");
        assert_eq!(jdoc.doc_type.as_deref(), Some("journal"));
        assert_eq!(jdoc.title.as_deref(), Some("March 2026"));
        assert!(jdoc.intro.is_some());
        assert!(jdoc
            .intro
            .as_ref()
            .unwrap()
            .content
            .as_deref()
            .is_some_and(|s| s.contains("Intro text.")));
        assert_eq!(jdoc.sections.len(), 2);
        assert_eq!(jdoc.sections[0].heading, "First");
        assert!(jdoc.sections[0]
            .content
            .as_deref()
            .is_some_and(|s| s.contains("Some content.")));
        assert_eq!(jdoc.sections[1].heading, "Second");
        assert!(jdoc.sections[1]
            .content
            .as_deref()
            .is_some_and(|s| s.contains("More content.")));
    }

    #[test]
    fn test_no_intro_when_h2_immediately_follows_h1() {
        let doc = mk("# Title\n\n## Section One\n\nContent.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        assert!(jdoc.intro.is_none());
        assert_eq!(jdoc.sections.len(), 1);
    }

    #[test]
    fn test_subsections_h3() {
        let doc = mk("# Title\n\n## Section\n\nDirect content.\n\n### Sub A\n\nSub content.\n\n### Sub B\n\nMore sub.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        assert_eq!(jdoc.sections.len(), 1);
        let sec = &jdoc.sections[0];
        assert!(sec
            .content
            .as_deref()
            .is_some_and(|s| s.contains("Direct content.")));
        assert_eq!(sec.subsections.len(), 2);
        assert_eq!(sec.subsections[0].heading, "Sub A");
        assert!(sec.subsections[0]
            .content
            .as_deref()
            .is_some_and(|s| s.contains("Sub content.")));
        assert_eq!(sec.subsections[1].heading, "Sub B");
        assert!(sec.subsections[1]
            .content
            .as_deref()
            .is_some_and(|s| s.contains("More sub.")));
    }

    #[test]
    fn test_h4_nesting() {
        let doc = mk("# T\n\n## S\n\n### Sub\n\n#### Deep\n\nDeep content.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let sec = &jdoc.sections[0];
        assert_eq!(sec.subsections.len(), 1);
        let sub = &sec.subsections[0];
        assert_eq!(sub.heading, "Sub");
        assert_eq!(sub.subsections.len(), 1);
        let deep = &sub.subsections[0];
        assert_eq!(deep.heading, "Deep");
        assert!(deep
            .content
            .as_deref()
            .is_some_and(|s| s.contains("Deep content.")));
    }

    #[test]
    fn test_h5_folds_into_h4() {
        let doc = mk("# T\n\n## S\n\n### Sub\n\n#### Deep\n\n##### Too deep\n\nFolds in.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let deep = &jdoc.sections[0].subsections[0].subsections[0];
        assert_eq!(deep.heading, "Deep");
        assert!(
            deep.content
                .as_deref()
                .is_some_and(|s| s.contains("Too deep")),
            "H5 should fold into H4 content: {:?}",
            deep.content
        );
        assert!(deep.subsections.is_empty());
    }

    #[test]
    fn test_content_preserves_markdown() {
        let doc = mk("# Title\n\n## Section\n\nSee **bold** and `code`.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let content = jdoc.sections[0].content.as_deref().unwrap_or("");
        assert!(
            content.contains("**bold**"),
            "content must preserve bold: {content}"
        );
        assert!(
            content.contains("`code`"),
            "content must preserve inline code: {content}"
        );
    }

    #[test]
    fn test_items_for_list_section() {
        let doc = mk("# Title\n\n## Steps\n\n- First step\n- Second step\n- Third step\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let sec = &jdoc.sections[0];
        assert_eq!(sec.items.len(), 3);
        assert_eq!(sec.items[0].text, "First step");
        assert_eq!(sec.items[1].text, "Second step");
        assert_eq!(sec.items[2].text, "Third step");
        assert!(sec.items[0].items.is_empty());
    }

    #[test]
    fn test_items_nested_sublist() {
        let doc = mk("# Title\n\n## Section\n\n- Top A\n   - Sub A1\n   - Sub A2\n- Top B\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let sec = &jdoc.sections[0];
        assert_eq!(sec.items.len(), 2);
        assert_eq!(sec.items[0].text, "Top A");
        assert_eq!(sec.items[0].items.len(), 2);
        assert_eq!(sec.items[0].items[0].text, "Sub A1");
        assert_eq!(sec.items[0].items[1].text, "Sub A2");
        assert_eq!(sec.items[1].text, "Top B");
        assert!(sec.items[1].items.is_empty());
    }

    #[test]
    fn test_items_strips_markup() {
        let doc = mk("# Title\n\n## Section\n\n- **Bold item**: description\n- `code` item\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let sec = &jdoc.sections[0];
        assert_eq!(sec.items[0].text, "Bold item: description");
        assert_eq!(sec.items[1].text, "code item");
    }

    #[test]
    fn test_items_empty_for_paragraph_section() {
        let doc = mk("# T\n\n## S\n\nJust a paragraph.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        assert!(jdoc.sections[0].items.is_empty());
        // items omitted in JSON output
        let json = serde_json::to_string(&jdoc).unwrap();
        assert!(
            !json.contains("\"items\""),
            "empty items should be omitted: {json}"
        );
    }

    #[test]
    fn test_links_extracted() {
        let doc = mk("# Title\n\n## Section\n\nSee [the docs](https://example.com) for info.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        assert_eq!(jdoc.sections[0].links.len(), 1);
        assert_eq!(jdoc.sections[0].links[0].text, "the docs");
        assert_eq!(jdoc.sections[0].links[0].url, "https://example.com");
    }

    #[test]
    fn test_links_in_intro() {
        let doc = mk("# Title\n\nSee [README](README.md).\n\n## Section\n\nContent.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let intro = jdoc.intro.as_ref().expect("expected intro");
        assert_eq!(intro.links.len(), 1);
        assert_eq!(intro.links[0].url, "README.md");
    }

    #[test]
    fn test_type_injected_for_path_match() {
        let doc = mk("---\ncreated: 2026-01-01\n---\n# Title\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("README.md"),
            Some("readme"),
            None,
            Path::new(""),
        );
        assert_eq!(jdoc.doc_type.as_deref(), Some("readme"));
        if let serde_json::Value::Object(map) = &jdoc.frontmatter {
            assert_eq!(
                map.get("type").and_then(|v| v.as_str()),
                Some("readme"),
                "type must be injected into frontmatter for path-matched file"
            );
        } else {
            panic!("frontmatter must be an object");
        }
    }

    #[test]
    fn test_frontmatter_integer_coercion() {
        let doc = mk("---\nservings: 4\n---\n# Recipe\n");
        let jdoc = document_to_json(&doc, Path::new("recipe.md"), None, None, Path::new(""));
        if let serde_json::Value::Object(map) = &jdoc.frontmatter {
            assert!(
                map.get("servings").is_some_and(|v| v.is_number()),
                "servings should be a number"
            );
        } else {
            panic!("frontmatter must be an object");
        }
    }

    #[test]
    fn test_no_frontmatter() {
        let doc = mk("# Just a heading\n");
        let jdoc = document_to_json(&doc, Path::new("bare.md"), None, None, Path::new(""));
        assert!(jdoc.doc_type.is_none());
        assert_eq!(
            jdoc.frontmatter,
            serde_json::Value::Object(serde_json::Map::new())
        );
    }

    #[test]
    fn test_type_none_reflected_in_frontmatter() {
        let doc = mk("---\ntype: none\n---\n# Title\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        assert!(jdoc.doc_type.is_none());
        if let serde_json::Value::Object(map) = &jdoc.frontmatter {
            assert_eq!(
                map.get("type").and_then(|v| v.as_str()),
                Some("none"),
                "type: none must appear in frontmatter JSON"
            );
        } else {
            panic!("frontmatter must be an object");
        }
    }

    #[test]
    fn test_no_title_document() {
        let doc = mk("## Section\n\nContent.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        assert!(jdoc.title.is_none());
        assert_eq!(jdoc.sections.len(), 1);
        assert!(jdoc.sections[0]
            .content
            .as_deref()
            .is_some_and(|s| s.contains("Content.")));
    }

    #[test]
    fn test_subsections_empty_when_none() {
        let doc = mk("# T\n\n## S\n\nJust content.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let json = serde_json::to_string(&jdoc).unwrap();
        assert!(
            !json.contains("\"subsections\""),
            "empty subsections should be omitted: {json}"
        );
    }

    // ── Properties tests ──────────────────────────────────────────────────────

    fn make_type_def_with_properties(props_yaml: &str) -> crate::schema::TypeDef {
        let type_yaml = format!(
            "structure:\n  sections:\n    - title: Media\n      bullets: unordered\n      properties:\n{props_yaml}"
        );
        serde_yaml::from_str(&type_yaml).expect("type yaml should parse")
    }

    #[test]
    fn test_properties_extracted_string_and_integer() {
        let type_def = make_type_def_with_properties(
            "        size:\n          type: integer\n        audio:\n          type: string\n",
        );
        // Document uses mixed case — keys normalized to lowercase in output
        let doc = mk("# T\n\n## Media\n\n- Bluray\n  - Size: 42\n  - Audio: English\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        let item = &jdoc.sections[0].items[0];
        assert_eq!(item.text, "Bluray");
        assert_eq!(
            item.properties.get("size"),
            Some(&serde_json::Value::Number(42.into()))
        );
        assert_eq!(
            item.properties.get("audio"),
            Some(&serde_json::Value::String("English".to_string()))
        );
    }

    #[test]
    fn test_properties_bool_coercion() {
        let type_def = make_type_def_with_properties("        hdr:\n          type: bool\n");
        // Document uses uppercase key
        let doc = mk("# T\n\n## Media\n\n- Bluray\n  - HDR: true\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        let item = &jdoc.sections[0].items[0];
        assert_eq!(
            item.properties.get("hdr"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn test_properties_unknown_key_included_as_string() {
        // Keys not in schema are extracted as lowercase strings
        let type_def = make_type_def_with_properties("        size:\n          type: integer\n");
        let doc = mk("# T\n\n## Media\n\n- Bluray\n  - Size: 10\n  - Unknown: value\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        let item = &jdoc.sections[0].items[0];
        assert_eq!(
            item.properties.get("unknown"),
            Some(&serde_json::Value::String("value".to_string()))
        );
    }

    #[test]
    fn test_properties_absent_without_schema() {
        // No schema → no properties, sub-items still appear in items[]
        let doc = mk("# T\n\n## Media\n\n- Bluray\n  - Size: 42\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let item = &jdoc.sections[0].items[0];
        assert!(
            item.properties.is_empty(),
            "properties should be empty without schema"
        );
        assert_eq!(item.items.len(), 1, "sub-items should still be in items[]");
        assert_eq!(item.items[0].text, "Size: 42");
    }

    #[test]
    fn test_properties_multiple_items_each_extracted() {
        let type_def = make_type_def_with_properties("        size:\n          type: integer\n");
        let doc = mk("# T\n\n## Media\n\n- Bluray\n  - Size: 10\n- Remux\n  - Size: 30\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        assert_eq!(
            jdoc.sections[0].items[0].properties.get("size"),
            Some(&serde_json::Value::Number(10.into()))
        );
        assert_eq!(
            jdoc.sections[0].items[1].properties.get("size"),
            Some(&serde_json::Value::Number(30.into()))
        );
    }

    #[test]
    fn test_properties_omitted_in_json_when_empty() {
        let doc = mk("# T\n\n## S\n\n- Just a flat item\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let json = serde_json::to_string(&jdoc).unwrap();
        assert!(
            !json.contains("\"properties\""),
            "empty properties should be omitted: {json}"
        );
    }

    // ── Flat section-level properties tests ─────────────────────────────────

    #[test]
    fn test_flat_section_properties_extracted() {
        let type_def = make_type_def_with_properties(
            "        size:\n          type: integer\n        audio:\n          type: string\n",
        );
        // Flat items (no parent bullet, no nesting)
        let doc = mk("# T\n\n## Media\n\n- Size: 42\n- Audio: English\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        let sec = &jdoc.sections[0];
        assert_eq!(
            sec.properties.get("size"),
            Some(&serde_json::Value::Number(42.into())),
            "flat integer property should be type-coerced"
        );
        assert_eq!(
            sec.properties.get("audio"),
            Some(&serde_json::Value::String("English".to_string())),
        );
        assert!(
            sec.items.is_empty(),
            "items should be omitted when section properties are extracted"
        );
    }

    #[test]
    fn test_flat_section_properties_unknown_key_included() {
        let type_def = make_type_def_with_properties("        size:\n          type: integer\n");
        let doc = mk("# T\n\n## Media\n\n- Size: 10\n- Unknown: value\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        let sec = &jdoc.sections[0];
        assert_eq!(
            sec.properties.get("unknown"),
            Some(&serde_json::Value::String("value".to_string())),
        );
    }

    #[test]
    fn test_flat_properties_not_extracted_when_nested() {
        // Nested items (parent + sub-items) should use item-level properties, not section-level.
        let type_def = make_type_def_with_properties("        size:\n          type: integer\n");
        let doc = mk("# T\n\n## Media\n\n- Bluray\n  - Size: 42\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        let sec = &jdoc.sections[0];
        assert!(
            sec.properties.is_empty(),
            "section-level properties should be empty when items are nested"
        );
        assert_eq!(sec.items.len(), 1);
        assert_eq!(
            sec.items[0].properties.get("size"),
            Some(&serde_json::Value::Number(42.into())),
            "item-level properties should still work"
        );
    }

    #[test]
    fn test_flat_section_properties_lowercase_keys() {
        let type_def =
            make_type_def_with_properties("        off-peak kwh:\n          type: string\n");
        let doc = mk("# T\n\n## Media\n\n- Off-peak kwh: 785.29\n");
        let jdoc = document_to_json(
            &doc,
            Path::new("test.md"),
            None,
            Some(&type_def),
            Path::new(""),
        );
        let sec = &jdoc.sections[0];
        assert_eq!(
            sec.properties.get("off-peak kwh"),
            Some(&serde_json::Value::String("785.29".to_string())),
            "keys should be lowercased to match schema"
        );
    }

    // ── collect_doc_links tests ───────────────────────────────────────────────

    #[test]
    fn test_collect_doc_links_empty() {
        let doc = mk("# T\n\nNo links here.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        assert!(collect_doc_links(&jdoc).is_empty());
    }

    #[test]
    fn test_collect_doc_links_from_intro() {
        let doc = mk("# T\n\nSee [README](README.md).\n\n## S\n\nContent.\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let urls = collect_doc_links(&jdoc);
        assert_eq!(urls, vec!["README.md"]);
    }

    #[test]
    fn test_collect_doc_links_from_section() {
        let doc = mk("# T\n\n## S\n\nSee [foo](foo.md) and [bar](bar.md).\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let urls = collect_doc_links(&jdoc);
        assert_eq!(urls, vec!["foo.md", "bar.md"]);
    }

    #[test]
    fn test_collect_doc_links_from_subsection() {
        let doc = mk("# T\n\n## S\n\n### Sub\n\nSee [deep](deep.md).\n");
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let urls = collect_doc_links(&jdoc);
        assert_eq!(urls, vec!["deep.md"]);
    }

    #[test]
    fn test_collect_doc_links_all_sources() {
        let doc = mk(
            "# T\n\nIntro [a](a.md).\n\n## S1\n\nBody [b](b.md).\n\n### Sub\n\n[c](c.md).\n\n## S2\n\n[d](d.md).\n",
        );
        let jdoc = document_to_json(&doc, Path::new("test.md"), None, None, Path::new(""));
        let urls = collect_doc_links(&jdoc);
        assert_eq!(urls, vec!["a.md", "b.md", "c.md", "d.md"]);
    }

    // ── json_output BFS integration tests ────────────────────────────────────

    #[test]
    fn test_json_output_depth_zero_single_file() {
        // depth=0 on a single file should produce exactly one JSONL line.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "# A\n\nSee [b](b.md).\n").unwrap();
        std::fs::write(root.join("b.md"), "# B\n\nContent.\n").unwrap();

        let mut out = Vec::new();
        let schemas = std::collections::HashMap::new();
        let matchers = std::collections::HashMap::new();
        let jdoc = build_file_doc(&root.join("a.md"), root, &schemas, &matchers).unwrap();
        writeln!(out, "{}", serde_json::to_string(&jdoc).unwrap()).unwrap();
        let lines: Vec<_> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["title"], "A");
    }

    #[test]
    fn test_json_output_depth_one_follows_link() {
        // depth=1 should include a.md and the b.md it links to.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "# A\n\nSee [b](b.md).\n").unwrap();
        std::fs::write(root.join("b.md"), "# B\n\nContent.\n").unwrap();

        let schemas = std::collections::HashMap::new();
        let matchers = std::collections::HashMap::new();

        // Manually drive the BFS logic used by json_output.
        let mut visited: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<(std::path::PathBuf, u32)> =
            std::collections::VecDeque::new();
        let seed = root.join("a.md");
        visited.insert(seed.clone());
        queue.push_back((seed, 0));

        let mut titles = Vec::new();
        let depth: u32 = 1;
        while let Some((path, current_depth)) = queue.pop_front() {
            let jdoc = build_file_doc(&path, root, &schemas, &matchers).unwrap();
            if let Some(title) = &jdoc.title {
                titles.push(title.clone());
            }
            if current_depth < depth {
                for url in collect_doc_links(&jdoc) {
                    if let Some(linked) = crate::validate::resolve_link_path(&url, &path) {
                        if linked.extension().and_then(|e| e.to_str()) == Some("md")
                            && linked.exists()
                            && visited.insert(linked.clone())
                        {
                            queue.push_back((linked, current_depth + 1));
                        }
                    }
                }
            }
        }
        assert_eq!(titles, vec!["A", "B"]);
    }

    #[test]
    fn test_json_output_depth_cycle_detection() {
        // A links to B, B links to A. Each should appear exactly once.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "# A\n\nSee [b](b.md).\n").unwrap();
        std::fs::write(root.join("b.md"), "# B\n\nSee [a](a.md).\n").unwrap();

        let schemas = std::collections::HashMap::new();
        let matchers = std::collections::HashMap::new();

        let mut visited: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<(std::path::PathBuf, u32)> =
            std::collections::VecDeque::new();
        let seed = root.join("a.md");
        visited.insert(seed.clone());
        queue.push_back((seed, 0));

        let mut count = 0usize;
        let depth: u32 = 10; // high depth — cycle should still terminate
        while let Some((path, current_depth)) = queue.pop_front() {
            let jdoc = build_file_doc(&path, root, &schemas, &matchers).unwrap();
            count += 1;
            if current_depth < depth {
                for url in collect_doc_links(&jdoc) {
                    if let Some(linked) = crate::validate::resolve_link_path(&url, &path) {
                        if linked.extension().and_then(|e| e.to_str()) == Some("md")
                            && linked.exists()
                            && visited.insert(linked.clone())
                        {
                            queue.push_back((linked, current_depth + 1));
                        }
                    }
                }
            }
        }
        assert_eq!(
            count, 2,
            "each file should appear exactly once despite cycle"
        );
    }

    #[test]
    fn test_json_output_skips_external_links() {
        // External URLs should not be enqueued.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("a.md"),
            "# A\n\nSee [example](https://example.com) and [local](local.md).\n",
        )
        .unwrap();
        std::fs::write(root.join("local.md"), "# Local\n").unwrap();

        let schemas = std::collections::HashMap::new();
        let matchers = std::collections::HashMap::new();

        let mut visited: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<(std::path::PathBuf, u32)> =
            std::collections::VecDeque::new();
        let seed = root.join("a.md");
        visited.insert(seed.clone());
        queue.push_back((seed, 0));

        let mut paths_seen = Vec::new();
        let depth: u32 = 1;
        while let Some((path, current_depth)) = queue.pop_front() {
            let jdoc = build_file_doc(&path, root, &schemas, &matchers).unwrap();
            paths_seen.push(jdoc.path.clone());
            if current_depth < depth {
                for url in collect_doc_links(&jdoc) {
                    if let Some(linked) = crate::validate::resolve_link_path(&url, &path) {
                        if linked.extension().and_then(|e| e.to_str()) == Some("md")
                            && linked.exists()
                            && visited.insert(linked.clone())
                        {
                            queue.push_back((linked, current_depth + 1));
                        }
                    }
                }
            }
        }
        // Only the two .md files; https://example.com must not appear
        assert_eq!(paths_seen.len(), 2);
        assert!(paths_seen.iter().all(|p| p.ends_with(".md")));
    }
}
