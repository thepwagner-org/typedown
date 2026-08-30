//! AST types: Document, Frontmatter, Block, Inline.

use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::borrow::Cow;

/// A parsed markdown document.
#[derive(Debug, Clone)]
pub struct Document {
    pub frontmatter: Option<Frontmatter>,
    pub blocks: Vec<Block>,
}

impl Document {
    /// A document built in memory, ready to serialize.
    ///
    /// Inserts the blank lines [`crate::parse::serialize`] expects between
    /// blocks, so two consecutive paragraphs don't weld into one on the next
    /// parse. The struct literal stays for [`crate::parse::parse`], whose
    /// blocks already carry their blank lines.
    pub fn new(frontmatter: Option<Frontmatter>, mut blocks: Vec<Block>) -> Self {
        crate::parse::normalize_blank_lines(&mut blocks);
        Self {
            frontmatter,
            blocks,
        }
    }
}

/// YAML frontmatter.
///
/// `type` is the only special field: it maps to `doc_type`. Everything else
/// lands in `fields` as raw YAML values, preserving document order.
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    /// Value of the `type` key (schema type name).
    pub doc_type: Option<String>,
    /// All other frontmatter fields in document order.
    pub fields: IndexMap<String, Value>,
    /// 1-based source line of each top-level key (including `type`).
    ///
    /// Populated by [`crate::parse::parse`] from the raw source; empty when the
    /// frontmatter is deserialized standalone (e.g. in tests).
    pub lines: IndexMap<String, usize>,
    /// Raw source text of top-level keys written as block scalars (`key: >-`,
    /// `key: |`), header line included, one entry per key.
    ///
    /// Lets [`crate::parse::serialize`] re-emit an untouched block scalar
    /// verbatim instead of collapsing it onto one long quoted line. Populated
    /// by [`crate::parse::parse`]; empty when deserialized standalone.
    pub block_scalars: IndexMap<String, String>,
}

impl Frontmatter {
    /// 1-based source line for a top-level key, falling back to line 1 (the
    /// opening `---`) when the key is absent or line info wasn't recorded.
    pub fn line_of(&self, key: &str) -> usize {
        self.lines.get(key).copied().unwrap_or(1)
    }

    /// A top-level field's raw YAML value.
    ///
    /// `type` is not in `fields` — it is [`Self::doc_type`] — so it is answered
    /// here rather than left to read as absent, which is a trap worth the clone.
    pub fn get(&self, key: &str) -> Option<Cow<'_, Value>> {
        if key == "type" {
            return self
                .doc_type
                .as_ref()
                .map(|t| Cow::Owned(Value::String(t.clone())));
        }
        self.fields.get(key).map(Cow::Borrowed)
    }

    /// Whether the frontmatter carries `key` at all, a null value included.
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// A top-level field deserialized into `T`.
    ///
    /// `Ok(None)` covers both an absent key and an explicitly null one: a
    /// document that omits `files:` and one that writes `files:` with nothing
    /// under it say the same thing. An `Err` is a document that carries the key
    /// and spells it wrong — which a caller about to act on the value must not
    /// read as "absent".
    pub fn field<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, serde_yaml::Error> {
        match self.get(key).as_deref() {
            None | Some(Value::Null) => Ok(None),
            Some(value) => serde_yaml::from_value(value.clone()).map(Some),
        }
    }

    /// A top-level field as the string a reader sees: strings verbatim, numbers
    /// and booleans as YAML spells them, anything structured `None`.
    ///
    /// Going through the parsed value rather than the source line is what makes
    /// quoting, spacing and a trailing comment resolve before the caller sees it.
    pub fn scalar_str(&self, key: &str) -> Option<String> {
        scalar_to_string(self.get(key)?.as_ref())
    }

    /// Frontmatter from any `Serialize` type — the way to build one in memory.
    ///
    /// The value must serialize to a YAML mapping; a `type` field (spelled
    /// `r#type` or `#[serde(rename = "type")]` in a struct) lands in
    /// [`Self::doc_type`], everything else in [`Self::fields`] in declaration
    /// order. Pair with [`crate::parse::serialize`] or
    /// [`crate::parse::serialize_frontmatter`] and the emitted YAML is exactly
    /// what `td fmt` would write, quoting included.
    pub fn from_serialize<T: Serialize>(value: &T) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_value(serde_yaml::to_value(value)?)
    }

    /// Set a top-level field from any `Serialize` value — the write twin of
    /// [`Self::field`].
    ///
    /// `type` routes to [`Self::doc_type`] and must serialize to a string (or
    /// null, which clears it); any other value there is an error, mirroring
    /// what deserialization enforces.
    pub fn set<T: Serialize>(&mut self, key: &str, value: &T) -> Result<(), serde_yaml::Error> {
        let value = serde_yaml::to_value(value)?;
        if key == "type" {
            self.doc_type = match value {
                Value::String(s) => Some(s),
                Value::Null => None,
                other => {
                    return Err(serde::de::Error::custom(format!(
                        "type: expected string, got {other:?}"
                    )));
                }
            };
        } else {
            self.fields.insert(key.to_string(), value);
        }
        Ok(())
    }

    /// Every top-level field in serialization order: `type` first, then
    /// [`Self::fields`] in insertion order.
    ///
    /// The one loop that sees `type` alongside the rest — iterating
    /// `fields` directly silently skips it, since it lives in
    /// [`Self::doc_type`].
    pub fn iter(&self) -> impl Iterator<Item = (&str, Cow<'_, Value>)> {
        self.doc_type
            .as_ref()
            .map(|t| ("type", Cow::Owned(Value::String(t.clone()))))
            .into_iter()
            .chain(
                self.fields
                    .iter()
                    .map(|(k, v)| (k.as_str(), Cow::Borrowed(v))),
            )
    }
}

impl Serialize for Frontmatter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let len = self.fields.len() + usize::from(self.doc_type.is_some());
        let mut map = serializer.serialize_map(Some(len))?;
        if let Some(doc_type) = &self.doc_type {
            map.serialize_entry("type", doc_type)?;
        }
        for (key, value) in &self.fields {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// A YAML scalar as its plain string spelling. Non-scalars have none.
pub fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl<'de> Deserialize<'de> for Frontmatter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut map: IndexMap<String, Value> = IndexMap::deserialize(deserializer)?;
        let doc_type = match map.shift_remove("type") {
            Some(Value::String(s)) => Some(s),
            Some(other) => {
                return Err(serde::de::Error::custom(format!(
                    "type: expected string, got {other:?}"
                )));
            }
            None => None,
        };
        Ok(Self {
            doc_type,
            fields: map,
            lines: IndexMap::new(),
            block_scalars: IndexMap::new(),
        })
    }
}

/// A block-level markdown element.
///
/// All variants except `BlankLine` carry a `line` field: the 1-based line
/// number where the block starts in the source file. `BlankLine` is synthetic
/// (inserted by normalization) and has no source position.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Block {
    Heading {
        level: u8,
        content: Vec<Inline>,
        line: usize,
    },
    Paragraph {
        content: Vec<Inline>,
        line: usize,
    },
    List {
        items: Vec<ListItem>,
        ordered: bool,
        /// First number of an ordered list (`5` for a list starting `5.`).
        ///
        /// Only meaningful when `ordered` is true; unordered lists carry 1.
        /// Items after the first are renumbered from here, so `5./5./5.`
        /// normalizes to `5./6./7.` — only the starting number survives.
        start: u64,
        line: usize,
    },
    CodeBlock {
        language: Option<String>,
        content: String,
        line: usize,
    },
    BlockQuote {
        blocks: Vec<Block>,
        line: usize,
    },
    Table {
        alignments: Vec<ColumnAlignment>,
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        line: usize,
    },
    ThematicBreak {
        line: usize,
    },
    /// A raw HTML block, kept verbatim so formatting round-trips losslessly.
    Html {
        content: String,
        line: usize,
    },
    /// Synthetic blank line inserted by normalization. Has no source position.
    BlankLine,
}

impl Block {
    /// A heading holding plain text, for a document built in memory.
    ///
    /// The text is content, not markup: `*` or `[` in it is escaped on
    /// serialization rather than read back as emphasis or a link. Built blocks
    /// carry line 0 — line numbers describe a source file, and there isn't one.
    pub fn heading(level: u8, text: impl Into<String>) -> Self {
        Self::Heading {
            level,
            content: vec![Inline::Text(text.into())],
            line: 0,
        }
    }

    /// A paragraph holding plain text, for a document built in memory.
    ///
    /// The text is content, not markup — see [`Self::heading`]. A paragraph
    /// with real inline structure (links, emphasis) is a struct literal with
    /// the [`Inline`]s spelled out.
    pub fn paragraph(text: impl Into<String>) -> Self {
        Self::Paragraph {
            content: vec![Inline::Text(text.into())],
            line: 0,
        }
    }

    /// An unordered list, for a document built in memory.
    pub fn bullet_list(items: impl IntoIterator<Item = ListItem>) -> Self {
        Self::List {
            items: items.into_iter().collect(),
            ordered: false,
            start: 1,
            line: 0,
        }
    }

    /// An ordered list numbered from 1, for a document built in memory.
    pub fn numbered_list(items: impl IntoIterator<Item = ListItem>) -> Self {
        Self::List {
            items: items.into_iter().collect(),
            ordered: true,
            start: 1,
            line: 0,
        }
    }

    /// A fenced code block, for a document built in memory.
    ///
    /// The fence is chosen at serialization by [`code_fence`], wide enough for
    /// whatever `content` holds.
    pub fn code_block(language: Option<&str>, content: impl Into<String>) -> Self {
        Self::CodeBlock {
            language: language.map(str::to_string),
            content: content.into(),
            line: 0,
        }
    }

    /// Returns the 1-based source line number, or 0 for synthetic `BlankLine`.
    pub fn line(&self) -> usize {
        match self {
            Self::Heading { line, .. }
            | Self::Paragraph { line, .. }
            | Self::List { line, .. }
            | Self::CodeBlock { line, .. }
            | Self::BlockQuote { line, .. }
            | Self::Table { line, .. }
            | Self::Html { line, .. }
            | Self::ThematicBreak { line } => *line,
            Self::BlankLine => 0,
        }
    }
}

/// Column alignment in a GFM table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlignment {
    None,
    Left,
    Center,
    Right,
}

/// A list item with inline content and optional nested blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub content: Vec<Inline>,
    pub children: Vec<Block>,
}

impl ListItem {
    /// An item holding plain text and no nested blocks, for a list built in
    /// memory. The text is content, not markup — see [`Block::heading`].
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Inline::Text(text.into())],
            children: Vec::new(),
        }
    }

    /// An item with inline structure and no nested blocks.
    pub fn new(content: Vec<Inline>) -> Self {
        Self {
            content,
            children: Vec::new(),
        }
    }
}

/// An inline markdown element.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link {
        content: Vec<Inline>,
        url: String,
        /// Tooltip text from `[x](/url "title")`, unescaped. `None` when absent.
        title: Option<String>,
    },
    Image {
        content: Vec<Inline>,
        url: String,
        /// Tooltip text from `![x](/url "title")`, unescaped. `None` when absent.
        title: Option<String>,
    },
    /// An autolink (`<https://example.com>`, `<user@example.com>`). Held apart
    /// from `Link` so it serializes back to angle-bracket form instead of being
    /// expanded to `[url](url)`.
    Autolink(String),
    /// An inline code span.
    Code(CodeSpan),
    /// Raw inline HTML (`<Cat>`, `<br/>`, `<span>`), kept verbatim. Without this
    /// the tag would be dropped at parse time — silent data loss on `td fmt`.
    Html(String),
    SoftBreak,
    /// An authored line break (`\` or two spaces at end of line), which renders
    /// as a `<br>`. Held apart from `SoftBreak` because collapsing the two
    /// welds the surrounding words into one line and changes the rendering.
    HardBreak,
}

/// An inline code span: its content plus the delimiters the author wrote.
///
/// A newline in `text` is an authored line break inside the span (CommonMark
/// renders it as a space); it carries no block indentation, so serialization
/// re-adds whatever prefix the context needs, exactly like [`Inline::SoftBreak`].
/// The padding CommonMark strips is not part of `text` either — it lives in
/// `delim`.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeSpan {
    /// The span's content, as a reader sees it.
    pub text: String,
    /// How the author spelled the delimiters. `None` when the source wasn't
    /// available or didn't decompose; [`CodeSpan::render`] then picks the
    /// narrowest delimiter that fits.
    pub delim: Option<CodeDelim>,
}

/// The backtick delimiters around a code span, as authored.
///
/// Deriving these from the content alone is lossy twice over: narrowing a
/// delimiter changes how *later* literal backtick runs on the same line pair
/// up, so the rendering changes; and the padding a reader strips is never put
/// back, so the span ends up glued to its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeDelim {
    /// Backticks on each side.
    pub width: usize,
    /// Whether the content was padded with one space on each side.
    pub padded: bool,
}

impl CodeSpan {
    /// The span as markdown, keeping the authored delimiters when they still
    /// spell `text` faithfully and falling back to [`format_code_span`].
    pub fn render(&self) -> String {
        match self.delim {
            Some(delim) if delim.spells(&self.text) => {
                let ticks = "`".repeat(delim.width);
                let pad = if delim.padded { " " } else { "" };
                format!("{ticks}{pad}{}{pad}{ticks}", self.text)
            }
            _ => format_code_span(&self.text),
        }
    }

    /// The span on one line, with an authored break collapsed to the space
    /// CommonMark renders it as.
    pub fn flattened(&self) -> Self {
        Self {
            text: self.text.replace('\n', " "),
            delim: self.delim,
        }
    }
}

impl CodeDelim {
    /// Whether wrapping `text` in these delimiters parses back to `text`.
    ///
    /// True by construction for a span read off the source, but content can
    /// change after parsing — flattening for one-line output, a fix rewriting
    /// a cell — and a stale delimiter would silently change what the span says.
    fn spells(self, text: &str) -> bool {
        // A span closes at the first run of *exactly* the opening width, so a
        // run of that width inside the content would end it early.
        if self.width == 0 || has_backtick_run(text, self.width) {
            return false;
        }
        let is_pad = |c: char| c == ' ' || c == '\n';
        // Vacuously true for empty content, which is not a code span at all.
        let all_pad = text.chars().all(is_pad);
        if self.padded {
            // A reader only strips the padding back off when what it wraps is
            // not itself all spaces.
            return !all_pad;
        }
        let (Some(first), Some(last)) = (text.chars().next(), text.chars().next_back()) else {
            return false;
        };
        // Unpadded content must neither merge with the delimiters nor look
        // padded to a reader, who would strip a space off each end.
        first != '`' && last != '`' && !(is_pad(first) && is_pad(last) && !all_pad)
    }
}

/// Whether `content` holds a maximal run of exactly `width` backticks.
fn has_backtick_run(content: &str, width: usize) -> bool {
    let mut run = 0;
    for ch in content.chars() {
        if ch == '`' {
            run += 1;
        } else {
            if run == width {
                return true;
            }
            run = 0;
        }
    }
    run == width
}

/// Longest run of consecutive backticks in `content` (0 if there are none).
fn longest_backtick_run(content: &str) -> usize {
    let mut max_run = 0;
    let mut current_run = 0;
    for ch in content.chars() {
        if ch == '`' {
            current_run += 1;
            max_run = max_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    max_run
}

/// Format a code span with delimiters wide enough for its content.
///
/// Per CommonMark spec §6.1 a code span ends at the first backtick run of
/// *exactly* the opening width, so the delimiter must be one wider than the
/// longest run in `content`. Any wider run would also work — the spec only
/// requires a width the content doesn't contain — but N+1 is what authors
/// write by hand (`` ` `` for a backtick, ``` `` ``` for a pair), so picking
/// it keeps `td fmt` from churning idiomatic markup.
///
/// Content is padded with one space on each side when it starts or ends with
/// a backtick (which would otherwise merge with the delimiter) or with a
/// space on *both* sides (which a reader strips back off). Padding is not
/// needed for all-space content: the stripping rule exempts it.
pub fn format_code_span(content: &str) -> String {
    // A code span can't be empty — `` is literal text, not a span — so the
    // nearest faithful spelling is a single space.
    if content.is_empty() {
        return "` `".to_string();
    }

    let delim = "`".repeat(longest_backtick_run(content) + 1);

    let edge_backtick = content.starts_with('`') || content.ends_with('`');
    let edge_spaces =
        content.starts_with(' ') && content.ends_with(' ') && !content.trim().is_empty();
    if edge_backtick || edge_spaces {
        format!("{delim} {content} {delim}")
    } else {
        format!("{delim}{content}{delim}")
    }
}

/// Backtick fence for a code block whose body is `content`.
///
/// A closing fence is any line-initial run *at least* as wide as the opening
/// one, so the fence has to be wider than every backtick run in the body —
/// otherwise a nested example's own fence terminates the block early and the
/// rest of the body spills out as prose. Minimum three, per the spec.
pub fn code_fence(content: &str) -> String {
    "`".repeat((longest_backtick_run(content) + 1).max(3))
}

/// Render the `(destination "title")` half of a link or image.
///
/// The destination is re-escaped by [`crate::escape::link_destination`], since
/// parsing hands it back decoded and bare. The title is re-quoted with double
/// quotes, so backslashes and double quotes inside it are escaped. An empty
/// destination is written `<>`: without it a following title would be read as
/// the destination.
pub fn format_link_target(url: &str, title: Option<&str>) -> String {
    let url = crate::escape::link_destination(url);
    match title {
        Some(t) => {
            let dest = if url.is_empty() { "<>" } else { &url };
            let escaped = t.replace('\\', "\\\\").replace('"', "\\\"");
            format!("({dest} \"{escaped}\")")
        }
        None => format!("({url})"),
    }
}

/// Convert inlines to plain text (strip all markup).
pub fn inlines_to_string(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(s) => out.push_str(s),
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strikethrough(inner) => {
                out.push_str(&inlines_to_string(inner));
            }
            Inline::Link { content, .. } => out.push_str(&inlines_to_string(content)),
            Inline::Image { content, .. } => out.push_str(&inlines_to_string(content)),
            Inline::Autolink(url) => out.push_str(url),
            // Line breaks inside a code span render as spaces, same as a SoftBreak.
            Inline::Code(span) => out.push_str(&span.text.replace('\n', " ")),
            // Raw HTML has no plain-text rendering; keep the markup verbatim so
            // text-derived values (headings, slugs) stay stable.
            Inline::Html(s) => out.push_str(s),
            // Both breaks flatten to a space: this is a single-line rendering,
            // used for headings, keys and field values.
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
    out
}

/// Convert inlines to markdown syntax (preserving links, emphasis, code, etc.).
pub fn inlines_to_markdown(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(s) => out.push_str(s),
            Inline::Strong(inner) => {
                out.push_str("**");
                out.push_str(&inlines_to_markdown(inner));
                out.push_str("**");
            }
            Inline::Emphasis(inner) => {
                out.push('*');
                out.push_str(&inlines_to_markdown(inner));
                out.push('*');
            }
            Inline::Strikethrough(inner) => {
                out.push_str("~~");
                out.push_str(&inlines_to_markdown(inner));
                out.push_str("~~");
            }
            Inline::Link {
                content,
                url,
                title,
            } => {
                out.push('[');
                out.push_str(&inlines_to_markdown(content));
                out.push(']');
                out.push_str(&format_link_target(url, title.as_deref()));
            }
            Inline::Image {
                content,
                url,
                title,
            } => {
                out.push_str("![");
                out.push_str(&inlines_to_markdown(content));
                out.push(']');
                out.push_str(&format_link_target(url, title.as_deref()));
            }
            Inline::Autolink(url) => {
                out.push('<');
                out.push_str(url);
                out.push('>');
            }
            Inline::Code(span) => {
                // Single-line rendering, so an authored break becomes a space.
                out.push_str(&span.flattened().render());
            }
            Inline::Html(s) => out.push_str(s),
            // Single-line rendering, as in `inlines_to_string`.
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
    out
}

/// One link found by [`links`]: where it points, what it says, where it sits.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRef<'a> {
    pub url: &'a str,
    /// Tooltip title, when the link carries one. Autolinks never do.
    pub title: Option<&'a str>,
    /// The link's content as plain text; the URL itself for an autolink.
    pub text: String,
    /// Source line of the enclosing block (0 for built blocks).
    pub line: usize,
    pub kind: LinkKind,
}

/// Which spelling of link a [`LinkRef`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Link,
    Image,
    Autolink,
}

/// Every link in `blocks`, in document order.
///
/// Walks headings, paragraphs, lists (items and their nested blocks),
/// blockquotes, and table cells, descending through emphasis and into link
/// content — an image inside a link's text is reported after the link itself.
/// One walk shared by every consumer: a caller that recurses by hand and skips
/// a variant reports different links than the tool next door.
pub fn links(blocks: &[Block]) -> Vec<LinkRef<'_>> {
    let mut out = Vec::new();
    block_links(blocks, &mut out);
    out
}

fn block_links<'a>(blocks: &'a [Block], out: &mut Vec<LinkRef<'a>>) {
    for block in blocks {
        let line = block.line();
        match block {
            Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
                inline_links(content, line, out);
            }
            Block::List { items, .. } => {
                for item in items {
                    inline_links(&item.content, line, out);
                    block_links(&item.children, out);
                }
            }
            Block::BlockQuote { blocks, .. } => block_links(blocks, out),
            Block::Table { header, rows, .. } => {
                for cell in header {
                    inline_links(cell, line, out);
                }
                for row in rows {
                    for cell in row {
                        inline_links(cell, line, out);
                    }
                }
            }
            Block::CodeBlock { .. }
            | Block::ThematicBreak { .. }
            | Block::Html { .. }
            | Block::BlankLine => {}
        }
    }
}

fn inline_links<'a>(inlines: &'a [Inline], line: usize, out: &mut Vec<LinkRef<'a>>) {
    for inline in inlines {
        match inline {
            Inline::Link {
                content,
                url,
                title,
            } => {
                out.push(LinkRef {
                    url,
                    title: title.as_deref(),
                    text: inlines_to_string(content),
                    line,
                    kind: LinkKind::Link,
                });
                inline_links(content, line, out);
            }
            Inline::Image {
                content,
                url,
                title,
            } => {
                out.push(LinkRef {
                    url,
                    title: title.as_deref(),
                    text: inlines_to_string(content),
                    line,
                    kind: LinkKind::Image,
                });
                inline_links(content, line, out);
            }
            Inline::Autolink(url) => {
                out.push(LinkRef {
                    url,
                    title: None,
                    text: url.clone(),
                    line,
                    kind: LinkKind::Autolink,
                });
            }
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strikethrough(inner) => {
                inline_links(inner, line, out);
            }
            Inline::Text(_)
            | Inline::Code(_)
            | Inline::Html(_)
            | Inline::SoftBreak
            | Inline::HardBreak => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inlines_to_string_strips_markup() {
        let inlines = vec![
            Inline::Text("Hello ".to_string()),
            Inline::Strong(vec![Inline::Text("world".to_string())]),
            Inline::Text("!".to_string()),
        ];
        assert_eq!(inlines_to_string(&inlines), "Hello world!");
    }

    #[test]
    fn test_inlines_to_string_link_uses_text() {
        let inlines = vec![Inline::Link {
            content: vec![Inline::Text("click here".to_string())],
            url: "https://example.com".to_string(),
            title: None,
        }];
        assert_eq!(inlines_to_string(&inlines), "click here");
    }

    #[test]
    fn test_inlines_to_markdown_preserves_syntax() {
        let inlines = vec![
            Inline::Strong(vec![Inline::Text("bold".to_string())]),
            Inline::Text(" and ".to_string()),
            Inline::Emphasis(vec![Inline::Text("italic".to_string())]),
        ];
        assert_eq!(inlines_to_markdown(&inlines), "**bold** and *italic*");
    }

    #[test]
    fn test_inlines_to_markdown_link() {
        let inlines = vec![Inline::Link {
            content: vec![Inline::Text("docs".to_string())],
            url: "https://example.com".to_string(),
            title: None,
        }];
        assert_eq!(inlines_to_markdown(&inlines), "[docs](https://example.com)");
    }

    #[test]
    fn test_block_line_accessor() {
        let b = Block::Heading {
            level: 1,
            content: vec![],
            line: 42,
        };
        assert_eq!(b.line(), 42);
        assert_eq!(Block::BlankLine.line(), 0);
    }

    #[test]
    fn test_heading_text() {
        let b = Block::Heading {
            level: 2,
            content: vec![Inline::Text("My Heading".to_string())],
            line: 1,
        };
        let text = match &b {
            Block::Heading { content, .. } => Some(inlines_to_string(content)),
            _ => None,
        };
        assert_eq!(text, Some("My Heading".to_string()));
    }

    #[test]
    fn test_frontmatter_deserialize_extracts_type() {
        let yaml = "type: recipe\nservings: 4\ncuisine: italian\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.doc_type.as_deref(), Some("recipe"));
        assert!(!fm.fields.contains_key("type"));
        assert!(fm.fields.contains_key("servings"));
        assert!(fm.fields.contains_key("cuisine"));
    }

    #[test]
    fn test_frontmatter_deserialize_no_type() {
        let yaml = "created: 2024-01-01\ndescription: A doc\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        assert!(fm.doc_type.is_none());
        assert_eq!(fm.fields.len(), 2);
    }

    #[test]
    fn test_frontmatter_deserialize_type_not_string_errors() {
        let yaml = "type:\n  - list\n  - value\n";
        let result: Result<Frontmatter, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn test_frontmatter_field_order_preserved() {
        let yaml = "type: foo\nzebra: z\nalpha: a\nmiddle: m\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        let keys: Vec<_> = fm.fields.keys().collect();
        assert_eq!(keys, &["zebra", "alpha", "middle"]);
    }

    #[test]
    fn test_frontmatter_serialize_round_trips_with_type_first() {
        let yaml = "type: recipe\nservings: 4\ncuisine: italian\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        let back = Frontmatter::from_serialize(&fm).unwrap();
        assert_eq!(back.doc_type.as_deref(), Some("recipe"));
        assert_eq!(back.fields, fm.fields);
        let keys: Vec<_> = fm.iter().map(|(k, _)| k.to_string()).collect();
        assert_eq!(keys, ["type", "servings", "cuisine"]);
    }

    #[test]
    fn test_frontmatter_set_routes_type_and_fields() {
        let mut fm = Frontmatter::default();
        fm.set("type", &"journal").unwrap();
        assert_eq!(fm.doc_type.as_deref(), Some("journal"));
        fm.set("tags", &vec!["a", "b"]).unwrap();
        assert_eq!(
            fm.field::<Vec<String>>("tags").unwrap().unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        // type must stay a string; a list there is the same error deserialization reports.
        assert!(fm.set("type", &vec!["x"]).is_err());
        fm.set("type", &Value::Null).unwrap();
        assert!(fm.doc_type.is_none());
    }

    #[test]
    fn test_links_walks_every_container() {
        let blocks = vec![
            Block::Heading {
                level: 1,
                content: vec![Inline::Link {
                    content: vec![Inline::Text("docs".to_string())],
                    url: "https://docs.example".to_string(),
                    title: None,
                }],
                line: 1,
            },
            Block::List {
                items: vec![ListItem {
                    content: vec![Inline::Emphasis(vec![Inline::Image {
                        content: vec![Inline::Text("logo".to_string())],
                        url: "logo.png".to_string(),
                        title: Some("The logo".to_string()),
                    }])],
                    children: vec![Block::Paragraph {
                        content: vec![Inline::Autolink("https://auto.example".to_string())],
                        line: 4,
                    }],
                }],
                ordered: false,
                start: 1,
                line: 3,
            },
            Block::Table {
                alignments: vec![ColumnAlignment::None],
                header: vec![vec![Inline::Text("h".to_string())]],
                rows: vec![vec![vec![Inline::Link {
                    content: vec![Inline::Text("cell".to_string())],
                    url: "cell.md".to_string(),
                    title: None,
                }]]],
                line: 6,
            },
        ];
        let found = links(&blocks);
        let summary: Vec<_> = found
            .iter()
            .map(|l| (l.url, l.text.as_str(), l.line, l.kind))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("https://docs.example", "docs", 1, LinkKind::Link),
                ("logo.png", "logo", 3, LinkKind::Image),
                (
                    "https://auto.example",
                    "https://auto.example",
                    4,
                    LinkKind::Autolink
                ),
                ("cell.md", "cell", 6, LinkKind::Link),
            ]
        );
        assert_eq!(found[1].title, Some("The logo"));
    }

    #[test]
    fn test_document_new_inserts_blank_lines() {
        let doc = Document::new(None, vec![Block::paragraph("one"), Block::paragraph("two")]);
        assert!(matches!(doc.blocks[1], Block::BlankLine));
        assert_eq!(doc.blocks.len(), 3);
    }

    #[test]
    fn test_format_code_span_simple() {
        assert_eq!(format_code_span("cargo build"), "`cargo build`");
    }

    #[test]
    fn test_format_code_span_content_with_backtick() {
        // Content contains single backticks → needs double-backtick delimiters + padding.
        assert_eq!(format_code_span("`!`command`"), "`` `!`command` ``");
    }

    #[test]
    fn test_format_code_span_content_with_double_backticks() {
        // Content contains `` but doesn't start/end with ` → triple delimiters, no padding.
        assert_eq!(format_code_span("a``b"), "```a``b```");
    }

    #[test]
    fn test_format_code_span_content_is_single_backtick() {
        assert_eq!(format_code_span("`"), "`` ` ``");
    }

    #[test]
    fn test_format_code_span_no_padding_when_no_edge_backticks() {
        // Backtick in the middle but not at start/end → needs double delimiters, no padding.
        assert_eq!(format_code_span("a`b"), "``a`b``");
    }

    #[test]
    fn test_format_code_span_pads_content_with_spaces_on_both_ends() {
        // A reader strips one space from each end of a padded span, so the
        // padding has to be doubled up for the content's own spaces to survive.
        assert_eq!(format_code_span(" x "), "`  x  `");
        // All-space content is exempt from the stripping rule — no padding.
        assert_eq!(format_code_span("  "), "`  `");
        // One-sided spaces aren't stripped either.
        assert_eq!(format_code_span(" x"), "` x`");
        assert_eq!(format_code_span("x "), "`x `");
    }

    #[test]
    fn test_format_code_span_widens_with_longest_run() {
        assert_eq!(format_code_span("``"), "``` `` ```");
        assert_eq!(format_code_span("a ``` b"), "````a ``` b````");
    }

    #[test]
    fn test_code_span_render_keeps_the_authored_spelling() {
        // Neither of these is what `format_code_span` would pick on its own:
        // it narrows the delimiter to the width the content needs and drops
        // padding the content doesn't require.
        let span = CodeSpan {
            text: " ".to_string(),
            delim: Some(CodeDelim {
                width: 2,
                padded: false,
            }),
        };
        assert_eq!(span.render(), "`` ``");
        let span = CodeSpan {
            text: "foo".to_string(),
            delim: Some(CodeDelim {
                width: 1,
                padded: true,
            }),
        };
        assert_eq!(span.render(), "` foo `");
    }

    #[test]
    fn test_code_span_render_falls_back_when_the_delimiter_stops_fitting() {
        // An authored delimiter is only reusable while it still spells the
        // content. A run of the same width inside would close the span early…
        let span = CodeSpan {
            text: "a `` b".to_string(),
            delim: Some(CodeDelim {
                width: 2,
                padded: false,
            }),
        };
        assert_eq!(span.render(), "```a `` b```");
        // …and padding is only stripped back off when it wraps something else.
        let span = CodeSpan {
            text: " ".to_string(),
            delim: Some(CodeDelim {
                width: 1,
                padded: true,
            }),
        };
        assert_eq!(span.render(), "` `");
    }

    #[test]
    fn test_code_span_render_without_an_authored_spelling() {
        let span = CodeSpan {
            text: "a`b".to_string(),
            delim: None,
        };
        assert_eq!(span.render(), "``a`b``");
    }

    #[test]
    fn test_code_fence_minimum_is_three() {
        assert_eq!(code_fence("plain code\n"), "```");
        assert_eq!(code_fence("a ` b\n"), "```");
        assert_eq!(code_fence("a `` b\n"), "```");
    }

    #[test]
    fn test_code_fence_widens_past_nested_fences() {
        // A three-backtick run anywhere in the body would close a ``` block.
        assert_eq!(code_fence("```bash\nls\n```\n"), "````");
        assert_eq!(code_fence("````\nnested\n````\n"), "`````");
    }
}
