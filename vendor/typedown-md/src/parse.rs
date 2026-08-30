//! Markdown parsing and serialization using pulldown-cmark.

use crate::ast::{
    Block, CodeDelim, CodeSpan, ColumnAlignment, Document, Frontmatter, Inline, ListItem,
};
use crate::escape::{EscapeCtx, InlineBuf};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::ops::Range;

/// The event stream, paired with each event's byte range in the source body
/// and, for a code span, the delimiters its source spells it with.
///
/// Ranges are what let list-item parsing tell a tight item from a loose one:
/// looseness is a blank line in the source, which no event reports on its own.
/// The delimiter is read off the source for the same reason: `Event::Code`
/// carries the content and nothing about how it was written.
type Events<'a> = VecDeque<(Event<'a>, Range<usize>, Option<CodeDelim>)>;

/// Byte-offset to 1-based line number lookup over the markdown body.
struct LineIndex<'a> {
    /// The body the event ranges index into. Kept so a construct pulldown-cmark
    /// drops on the floor — a table row's cells past the header's column count —
    /// can still be read back off the source it came from.
    body: &'a str,
    starts: Vec<usize>,
    /// `blank[n]` is true when 1-based line `n` holds only whitespace.
    blank: Vec<bool>,
    /// Lines the frontmatter occupies. The parser only ever sees the body, so
    /// its offsets are body-relative; this is what turns them back into lines
    /// of the original file.
    fm_lines: usize,
}

impl<'a> LineIndex<'a> {
    fn new(body: &'a str, fm_lines: usize) -> Self {
        let starts = std::iter::once(0)
            .chain(body.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        // Inside a blockquote the gap between two blocks is spelled `>`, not an
        // empty line, and the quote markers are on the line the same way its
        // text is. Read literally, no line in a blockquote is ever blank, and
        // every gap inside one is lost: a list item's second paragraph came
        // back welded onto its first.
        let blank = std::iter::once(false)
            .chain(
                body.lines()
                    .map(|l| l.chars().all(|c| c == '>' || c.is_whitespace())),
            )
            .collect();
        Self {
            body,
            starts,
            blank,
            fm_lines,
        }
    }

    /// 1-based line *within the body*, which is also the index into `blank`.
    fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset)
    }

    /// 1-based line in the original file, frontmatter included.
    fn source_line(&self, offset: usize) -> usize {
        self.line_of(offset) + self.fm_lines
    }

    /// Whether the line above the one holding `offset` is blank.
    ///
    /// This asks the *following* block whether it was preceded by a gap, rather
    /// than asking the preceding block whether it was followed by one. A
    /// container's range runs past its own last line — a nested list's ends on
    /// the indent of whatever comes after it, blank line included — so measured
    /// from the front the gap disappears and the two blocks come back welded
    /// together. Measured from behind it is always visible.
    fn preceded_by_blank_line(&self, offset: usize) -> bool {
        let line = self.line_of(offset);
        line > 1 && self.blank.get(line - 1).copied().unwrap_or(false)
    }

    /// Whether the last line before `end` is blank.
    ///
    /// The blank line that makes a list loose falls inside the *preceding*
    /// item's range, so this is how an item learns it is followed by a gap.
    fn ends_with_blank_line(&self, end: usize) -> bool {
        end > 0
            && self
                .blank
                .get(self.line_of(end - 1))
                .copied()
                .unwrap_or(false)
    }
}

/// Parser options: CommonMark + GFM tables only.
///
/// Strikethrough is intentionally excluded: pulldown-cmark treats `~` as a
/// strikethrough delimiter, corrupting prose like `~$5` (approximately $5).
fn parser_options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts
}

/// Normalize every line ending to a bare newline.
///
/// CommonMark's preprocessing step treats CRLF, and a lone CR, as line endings,
/// but pulldown-cmark passes some of them through as text — a `\r` that reaches
/// `Event::Text` inside a code block lands in the AST verbatim, and serializing
/// gives a file with CRLF inside its fences and LF everywhere else. Doing the
/// preprocessing ourselves means a CRLF document comes out uniformly LF, which
/// is what every other line the serializer writes uses.
///
/// Borrows when there is nothing to normalize, which is the common case.
fn normalize_line_endings(content: &str) -> Cow<'_, str> {
    if content.contains('\r') {
        Cow::Owned(content.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(content)
    }
}

/// Parse markdown content into a Document AST.
pub fn parse(content: &str) -> Document {
    let content = normalize_line_endings(content);
    let content = content.as_ref();
    let split = split_frontmatter(content);
    // A document whose frontmatter doesn't deserialize still has a body, and
    // formatting it has to stay lossless — so the error is dropped here and
    // reported separately by `get_frontmatter_error`.
    let frontmatter = read_frontmatter(content, split.frontmatter).ok().flatten();

    let lines = LineIndex::new(split.body, split.frontmatter_lines);
    let blocks = parse_blocks(markdown_events(split.body), &lines);

    Document {
        frontmatter,
        blocks,
    }
}

/// Parse a document's frontmatter without reading its body.
///
/// The [`Frontmatter`] is the one [`parse`] would produce — `lines` and
/// `block_scalars` populated — for a fraction of the work, since the markdown
/// never reaches pulldown-cmark. A tool that walks a corpus to read one
/// frontmatter key is the case this exists for.
///
/// `Ok(None)` is a document with no frontmatter block. `Err` is a document that
/// has one which isn't a YAML mapping — a caller deciding anything on a field's
/// value must not read that as "the key is absent".
pub fn parse_frontmatter(content: &str) -> Result<Option<Frontmatter>, serde_yaml::Error> {
    let content = normalize_line_endings(content);
    let content = content.as_ref();
    read_frontmatter(content, split_frontmatter(content).frontmatter)
}

/// Deserialize `yaml` and attach the source positions read off `content`.
///
/// `content` must already be line-ending normalized, and `yaml` must be the
/// frontmatter [`split_frontmatter`] found in it.
fn read_frontmatter(
    content: &str,
    yaml: Option<&str>,
) -> Result<Option<Frontmatter>, serde_yaml::Error> {
    let Some(yaml) = yaml else {
        return Ok(None);
    };
    let mut fm: Frontmatter = serde_yaml::from_str(yaml)?;
    fm.lines = frontmatter_key_lines(content);
    fm.block_scalars = frontmatter_block_scalars(content);
    Ok(Some(fm))
}

/// pulldown-cmark events for markdown body text (frontmatter already stripped).
///
/// CommonMark normalizes line endings inside a code span to spaces, so
/// `Event::Code` alone can't tell an authored wrap from a literal space. Put
/// the break back from the source before the events reach the AST.
fn markdown_events(src: &str) -> Events<'_> {
    Parser::new_ext(src, parser_options())
        .into_offset_iter()
        .map(|(event, range)| match event {
            Event::Code(code) => {
                let raw = &src[range.clone()];
                let event = match restore_code_span_breaks(&code, raw) {
                    Some(wrapped) => Event::Code(wrapped.into()),
                    None => Event::Code(code),
                };
                (event, range, authored_code_delim(raw))
            }
            other => (other, range, None),
        })
        .collect()
}

/// Get the frontmatter parse error from raw content, if any.
///
/// Returns `Some(message)` if frontmatter exists but failed to deserialize,
/// `None` if there is no frontmatter or it parsed successfully.
pub fn get_frontmatter_error(content: &str) -> Option<String> {
    parse_frontmatter(content).err().map(|e| e.to_string())
}

/// One top-level frontmatter field from raw document text, in one call.
///
/// `Ok(None)` is a document with no frontmatter, no such key, or an explicitly
/// null one — the same reading as [`Frontmatter::field`]. `Err` is frontmatter
/// that doesn't parse or a value that isn't a `T`, which a caller acting on
/// the value must not fold into "absent".
pub fn frontmatter_field<T: serde::de::DeserializeOwned>(
    content: &str,
    key: &str,
) -> Result<Option<T>, serde_yaml::Error> {
    match parse_frontmatter(content)? {
        Some(fm) => fm.field(key),
        None => Ok(None),
    }
}

/// A document split at its frontmatter delimiters, with nothing parsed.
///
/// Both halves borrow from the source, so a tool that rewrites one of them
/// hands the other back byte for byte — which is how blob edits its own
/// frontmatter block without reformatting keys it doesn't own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split<'a> {
    /// The raw YAML between the delimiters, without the delimiter lines
    /// themselves. `None` is a document with no frontmatter, which is not the
    /// same as one whose frontmatter is empty.
    pub frontmatter: Option<&'a str>,
    /// The body, starting on the line after the closing delimiter. The whole
    /// source when there is no frontmatter.
    pub body: &'a str,
    /// Source lines the frontmatter block spans, both delimiters included.
    /// Zero when there is no frontmatter — what turns a body offset back into a
    /// line of the original file.
    pub frontmatter_lines: usize,
    /// Byte offset of the closing `---` in the source: where a tool splices a
    /// frontmatter line of its own, rather than searching for a delimiter and
    /// finding one inside a value. `None` when there is no frontmatter.
    pub close: Option<usize>,
}

/// Split `content` into its frontmatter YAML and its markdown body.
///
/// The single authority on where frontmatter ends. Splitting the body with one
/// rule and reading the YAML with another is how a leading `---` gets eaten: a
/// lenient splitter treats an unterminated `---` as an opener and swallows the
/// line, while the strict reader reports no frontmatter at all — so the
/// thematic break the author wrote vanishes from both halves. Public for the
/// same reason [`yaml_scalar`] is: tools that write these documents have to
/// agree with `td fmt` about what is frontmatter and what is prose.
///
/// Reads the source as written — a CRLF document keeps its `\r`s in both
/// halves, since the halves are borrowed. [`parse`] normalizes line endings
/// before splitting.
pub fn split_frontmatter(content: &str) -> Split<'_> {
    let no_frontmatter = Split {
        frontmatter: None,
        body: content,
        frontmatter_lines: 0,
        close: None,
    };
    let Some(rest) = content.strip_prefix("---") else {
        return no_frontmatter;
    };
    // The opening delimiter owns its whole line; `---foo` is not frontmatter.
    if !rest.starts_with(['\n', '\r']) {
        return no_frontmatter;
    }
    // No closing delimiter means no frontmatter: the `---` is a thematic break.
    let Some(end) = rest.find("\n---") else {
        return no_frontmatter;
    };
    // The YAML runs from the line after the opening delimiter through the
    // newline that closes its last line — a literal scalar's trailing line
    // break is part of its value, and a leading blank line is not.
    let first = if rest.starts_with("\r\n") { 2 } else { 1 };
    let yaml = &rest[first.min(end + 1)..end + 1];
    // `end` indexes the newline before the closing delimiter, so the delimiter
    // itself starts one past it — measured from `content`, past the opener.
    let close = "---".len() + end + 1;
    // The body starts on the line after the closing delimiter.
    let body_start = match content[close..].find('\n') {
        Some(nl) => close + nl + 1,
        None => content.len(),
    };
    Split {
        frontmatter: Some(yaml),
        body: &content[body_start..],
        frontmatter_lines: content[..body_start].lines().count(),
        close: Some(close),
    }
}

/// Serialize a Document AST back to a markdown string.
///
/// `type` is written first, then all other fields in their original order.
pub fn serialize(doc: &Document) -> String {
    serialize_with_field_order(doc, &[])
}

/// Serialize a Document AST with optional frontmatter field ordering.
///
/// If `field_order` is non-empty, those fields are written first in the given
/// order; any remaining fields follow in insertion order. If empty, all fields
/// are written in insertion order (`type` always comes first).
pub fn serialize_with_field_order(doc: &Document, field_order: &[&str]) -> String {
    serialize_doc(doc, field_order, true)
}

/// `document_start` says these blocks begin a file, so a leading thematic break
/// has to avoid the `---` spelling. Nested serializations (a blockquote's body)
/// pass false: their `---` is indented behind a marker and can't be mistaken for
/// a frontmatter delimiter.
fn serialize_doc(doc: &Document, field_order: &[&str], document_start: bool) -> String {
    let mut out = String::new();

    if let Some(fm) = &doc.frontmatter {
        out.push_str(&frontmatter_block(fm, field_order));
    }

    let mut styles = list_styles(&doc.blocks, false).into_iter();
    for (i, block) in doc.blocks.iter().enumerate() {
        // Ensure a blank line before headings, thematic breaks, and blockquotes
        // (except the first block) to avoid CommonMark setext-heading ambiguity.
        if matches!(
            block,
            Block::Heading { .. } | Block::ThematicBreak { .. } | Block::BlockQuote { .. }
        ) && i > 0
            && !matches!(doc.blocks.get(i - 1), Some(Block::BlankLine))
        {
            out.push('\n');
        }

        match block {
            Block::Heading { level, content, .. } => {
                out.push_str(&heading_line(*level, content));
                out.push('\n');
            }
            Block::Paragraph { content, .. } => {
                out.push_str(&serialize_inlines(content, true));
                out.push('\n');
            }
            Block::List {
                items,
                ordered,
                start,
                ..
            } => {
                let style = styles.next().unwrap_or(ListStyle::House);
                serialize_list(&mut out, items, *ordered, *start, "", style);
            }
            Block::CodeBlock {
                language, content, ..
            } => {
                write_code_block(&mut out, language.as_deref(), content, "");
            }
            Block::BlockQuote { blocks, .. } => {
                serialize_blockquote(&mut out, blocks);
            }
            Block::Table {
                alignments,
                header,
                rows,
                ..
            } => {
                serialize_table(&mut out, alignments, header, rows, "");
            }
            Block::ThematicBreak { .. } => {
                // A `---` opening a file is a frontmatter delimiter to the next
                // parse, which eats the break — and, if the file holds another
                // `---` further down, everything in between with it. The same
                // goes for the line right after a real frontmatter block, which
                // reads as a second delimiter. `***` is the same thematic break
                // with no spelling a frontmatter reader can claim.
                out.push_str(if document_start && i == 0 {
                    "***\n"
                } else {
                    "---\n"
                });
            }
            Block::Html { content, .. } => {
                write_verbatim(&mut out, content, "");
            }
            Block::BlankLine => {
                out.push('\n');
            }
        }
    }

    // Ensure single trailing newline
    while out.ends_with("\n\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }

    out
}

// ── parsing internals ─────────────────────────────────────────────────────────

/// Parse a run of block-level events into blocks.
///
/// Each block's source line comes from the byte offset of the event that opened
/// it. Deriving it here, where the block is built, is what keeps line numbers in
/// step with the AST: a second pass that re-walked the events would have to
/// re-derive which events become blocks — and any disagreement (an inline tag
/// miscounted as a block, an empty paragraph collapsed to a `BlankLine`) would
/// slide every later block's line number.
fn parse_blocks(mut events: Events, lines: &LineIndex) -> Vec<Block> {
    let mut blocks = Vec::new();

    while let Some((event, range, _)) = events.pop_front() {
        let line = lines.source_line(range.start);
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let content = collect_inlines(&mut events, TagEnd::Heading(level));
                blocks.push(Block::Heading {
                    level: heading_level_to_u8(level),
                    content: flatten_breaks(&content),
                    line,
                });
            }
            Event::Start(Tag::Paragraph) => {
                let content = collect_inlines(&mut events, TagEnd::Paragraph);
                if content.is_empty() {
                    blocks.push(Block::BlankLine);
                } else {
                    blocks.push(Block::Paragraph { content, line });
                }
            }
            Event::Start(Tag::List(start_num)) => {
                let items = collect_list_items(&mut events, lines);
                blocks.push(Block::List {
                    items,
                    ordered: start_num.is_some(),
                    start: start_num.unwrap_or(1),
                    line,
                });
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
                let content = collect_code_block(&mut events);
                blocks.push(Block::CodeBlock {
                    language,
                    content,
                    line,
                });
            }
            Event::Start(Tag::BlockQuote(_kind)) => {
                let inner_events = collect_container_events(&mut events, |e| {
                    matches!(e, Event::End(TagEnd::BlockQuote(_)))
                });
                let inner_blocks = parse_blocks(inner_events, lines);
                blocks.push(Block::BlockQuote {
                    blocks: inner_blocks,
                    line,
                });
            }
            Event::Start(Tag::Table(alignments)) => {
                let aligns = alignments.into_iter().map(convert_alignment).collect();
                let (header, rows) = collect_table(&mut events, lines);
                blocks.push(Block::Table {
                    alignments: aligns,
                    header,
                    rows,
                    line,
                });
            }
            Event::Start(Tag::HtmlBlock) => {
                blocks.push(Block::Html {
                    content: collect_html_block(&mut events),
                    line,
                });
            }
            Event::Rule => {
                blocks.push(Block::ThematicBreak { line });
            }
            _ => {}
        }
    }

    normalize_blank_lines(&mut blocks);
    blocks
}

/// Re-derive the line breaks CommonMark normalized away inside a code span.
///
/// The parser turns a line ending inside a code span into a space, so the
/// event carries no trace of the author's wrap. Serializing that content
/// verbatim joins the two source lines — a paragraph with a straddling code
/// span reflows while its hard-wrapped neighbours don't. Align the normalized
/// content against the raw source to put the breaks back.
///
/// Returns `None` when the two can't be aligned (a continuation prefix we
/// don't recognize, CRLF endings, odd delimiters) — the caller then keeps the
/// normalized content, which is what it used before.
fn restore_code_span_breaks(normalized: &str, raw: &str) -> Option<String> {
    let inner = strip_code_span_padding(strip_code_span_delimiters(raw)?);
    if !inner.contains('\n') {
        return None;
    }

    let mut out = String::new();
    let mut rest = normalized;
    for (i, line) in inner.split('\n').enumerate() {
        if i > 0 {
            // The line ending is the space the parser put in its place.
            rest = rest.strip_prefix(' ')?;
            out.push('\n');
        }
        // Continuation lines still carry the block prefix (list indentation,
        // blockquote markers) that the block parser stripped before inline
        // parsing. It is whatever leading run the normalized content lacks.
        let prefix_max = line.len() - line.trim_start_matches([' ', '\t', '>']).len();
        let text = (0..=prefix_max)
            .find(|&k| rest.starts_with(&line[k..]))
            .map(|k| &line[k..])?;
        out.push_str(text);
        rest = &rest[text.len()..];
    }

    // A leftover tail means the alignment drifted; don't trust the result.
    rest.is_empty().then_some(out)
}

/// Read a code span's delimiters back off its source.
///
/// The event stream reports the content only, so a serializer left to itself
/// re-derives the narrowest delimiter that fits — which changes how a *later*
/// literal backtick run on the same line pairs up — and drops the padding
/// CommonMark strips, gluing the span to its neighbours. Both facts are still
/// in the source; take them from there.
///
/// Returns `None` when the source doesn't decompose into matching delimiters,
/// leaving the serializer to derive its own.
fn authored_code_delim(raw: &str) -> Option<CodeDelim> {
    let inner = strip_code_span_delimiters(raw)?;
    Some(CodeDelim {
        width: (raw.len() - inner.len()) / 2,
        padded: strip_code_span_padding(inner).len() < inner.len(),
    })
}

/// Split a code span's source into the text between its backtick delimiters.
fn strip_code_span_delimiters(raw: &str) -> Option<&str> {
    let open = raw.len() - raw.trim_start_matches('`').len();
    let close = raw.len() - raw.trim_end_matches('`').len();
    if open == 0 || open != close || raw.len() <= open + close {
        return None;
    }
    Some(&raw[open..raw.len() - close])
}

/// Drop the one space of padding CommonMark strips when a code span's content
/// both begins and ends with a space (a line ending counts as one) without
/// being all spaces — so the source lines up with the parsed content.
fn strip_code_span_padding(inner: &str) -> &str {
    let is_pad = |c: char| c == ' ' || c == '\n';
    let (Some(first), Some(last)) = (inner.chars().next(), inner.chars().next_back()) else {
        return inner;
    };
    if inner.len() >= 2 && is_pad(first) && is_pad(last) && !inner.chars().all(is_pad) {
        &inner[1..inner.len() - 1]
    } else {
        inner
    }
}

fn collect_inlines(events: &mut Events, end_tag: TagEnd) -> Vec<Inline> {
    let mut inlines = Vec::new();

    while let Some((event, _, delim)) = events.pop_front() {
        match event {
            Event::End(tag) if tag == end_tag => break,
            Event::Text(text) => inlines.push(Inline::Text(text.to_string())),
            Event::Code(code) => inlines.push(Inline::Code(CodeSpan {
                text: code.to_string(),
                delim,
            })),
            Event::Start(Tag::Strong) => {
                let inner = collect_inlines(events, TagEnd::Strong);
                inlines.push(Inline::Strong(inner));
            }
            Event::Start(Tag::Emphasis) => {
                let inner = collect_inlines(events, TagEnd::Emphasis);
                inlines.push(Inline::Emphasis(inner));
            }
            Event::Start(Tag::Strikethrough) => {
                let inner = collect_inlines(events, TagEnd::Strikethrough);
                inlines.push(Inline::Strikethrough(inner));
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            }) => {
                let inner = collect_inlines(events, TagEnd::Link);
                inlines.push(make_link(link_type, &dest_url, &title, inner));
            }
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                let inner = collect_inlines(events, TagEnd::Image);
                inlines.push(Inline::Image {
                    content: inner,
                    url: dest_url.to_string(),
                    title: opt_title(&title),
                });
            }
            Event::InlineHtml(html) => inlines.push(Inline::Html(html.to_string())),
            Event::SoftBreak => inlines.push(Inline::SoftBreak),
            Event::HardBreak => inlines.push(Inline::HardBreak),
            _ => {}
        }
    }

    inlines
}

/// A link/image title, or `None` when pulldown-cmark reports the empty string
/// it uses for "no title".
fn opt_title(title: &str) -> Option<String> {
    (!title.is_empty()).then(|| title.to_string())
}

/// Build a link inline, preserving angle-bracket autolinks as [`Inline::Autolink`].
///
/// Only collapses to an autolink when the link text is exactly the destination —
/// otherwise the round-trip would rewrite the visible text.
fn make_link(link_type: LinkType, dest_url: &str, title: &str, content: Vec<Inline>) -> Inline {
    let is_auto = matches!(link_type, LinkType::Autolink | LinkType::Email);
    if is_auto && matches!(content.as_slice(), [Inline::Text(t)] if t == dest_url) {
        return Inline::Autolink(dest_url.to_string());
    }
    Inline::Link {
        content,
        url: dest_url.to_string(),
        title: opt_title(title),
    }
}

fn collect_list_items(events: &mut Events, lines: &LineIndex) -> Vec<ListItem> {
    let mut items: Vec<ListItem> = Vec::new();
    let mut gap_after: Vec<bool> = Vec::new();

    while let Some((event, range, _)) = events.pop_front() {
        match event {
            Event::End(TagEnd::List(_)) => break,
            Event::Start(Tag::Item) => {
                items.push(collect_item_content(events, lines));
                gap_after.push(lines.ends_with_blank_line(range.end));
            }
            _ => {}
        }
    }

    // The blank line that separates two items of a loose list sits inside the
    // earlier item's range. Carry it as a trailing `BlankLine` child so the
    // list does not come back tight. The final item has no following gap.
    for idx in 0..items.len().saturating_sub(1) {
        if gap_after[idx] {
            items[idx].children.push(Block::BlankLine);
        }
    }

    items
}

/// Accumulates one list item's blocks in source order.
///
/// pulldown-cmark reports a tight item's prose as bare inline events and a
/// loose item's as `Paragraph` runs. Either way only the *first* run becomes
/// the item's inline `content`; every later run is a `Paragraph` child. Without
/// that split, prose following a code fence was appended to `content` and so
/// re-emitted above the fence, welded to the intro text.
struct ItemBuilder {
    content: Vec<Inline>,
    children: Vec<Block>,
    /// Inline run collected after the item's first child block.
    pending: Vec<Inline>,
    /// Whether a blank line preceded `pending`.
    pending_blank: bool,
    /// False once the `content` slot is closed and later runs become children.
    content_open: bool,
}

impl ItemBuilder {
    fn new() -> Self {
        Self {
            content: Vec::new(),
            children: Vec::new(),
            pending: Vec::new(),
            pending_blank: false,
            content_open: true,
        }
    }

    fn has_content(&self) -> bool {
        !self.content.is_empty() || !self.children.is_empty()
    }

    fn push_inline(&mut self, inline: Inline, range: &Range<usize>, lines: &LineIndex) {
        if self.content_open {
            self.content.push(inline);
        } else {
            if self.pending.is_empty() {
                self.pending_blank = lines.preceded_by_blank_line(range.start);
            }
            self.pending.push(inline);
        }
    }

    /// Turn the inline run collected since the last child block into a paragraph.
    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if self.pending_blank && self.has_content() {
            self.children.push(Block::BlankLine);
        }
        let content = std::mem::take(&mut self.pending);
        self.children.push(Block::Paragraph { content, line: 0 });
    }

    /// The first paragraph fills the item's inline content; later ones are children.
    fn push_paragraph(&mut self, content: Vec<Inline>, range: &Range<usize>, lines: &LineIndex) {
        if self.content_open && !self.has_content() {
            self.content = content;
            self.content_open = false;
            return;
        }
        self.push_block(Block::Paragraph { content, line: 0 }, range, lines);
    }

    fn push_block(&mut self, block: Block, range: &Range<usize>, lines: &LineIndex) {
        self.flush_pending();
        self.content_open = false;
        if self.has_content() && lines.preceded_by_blank_line(range.start) {
            self.children.push(Block::BlankLine);
        }
        self.children.push(block);
    }

    fn finish(mut self) -> ListItem {
        self.flush_pending();
        ListItem {
            content: self.content,
            children: self.children,
        }
    }
}

fn collect_item_content(events: &mut Events, lines: &LineIndex) -> ListItem {
    let mut item = ItemBuilder::new();

    while let Some((event, range, delim)) = events.pop_front() {
        match event {
            Event::End(TagEnd::Item) => break,
            Event::Start(Tag::Paragraph) => {
                let content = collect_inlines(events, TagEnd::Paragraph);
                item.push_paragraph(content, &range, lines);
            }
            // Without this arm the heading's `#`s are dropped and its text
            // falls through to the inline arms below: `- item\n\n  ## sub`
            // came back demoted to a paragraph, and the tight spelling
            // `- item\n  ## sub` welded the two into `- itemsub`.
            Event::Start(Tag::Heading { level, .. }) => {
                let content = collect_inlines(events, TagEnd::Heading(level));
                item.push_block(
                    Block::Heading {
                        level: heading_level_to_u8(level),
                        content: flatten_breaks(&content),
                        line: 0,
                    },
                    &range,
                    lines,
                );
            }
            Event::Start(Tag::List(start_num)) => {
                let items = collect_list_items(events, lines);
                item.push_block(
                    Block::List {
                        items,
                        ordered: start_num.is_some(),
                        start: start_num.unwrap_or(1),
                        line: 0,
                    },
                    &range,
                    lines,
                );
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
                let content = collect_code_block(events);
                item.push_block(
                    Block::CodeBlock {
                        language,
                        content,
                        line: 0,
                    },
                    &range,
                    lines,
                );
            }
            Event::Start(Tag::BlockQuote(_kind)) => {
                let inner_events = collect_container_events(events, |e| {
                    matches!(e, Event::End(TagEnd::BlockQuote(_)))
                });
                let blocks = parse_blocks(inner_events, lines);
                item.push_block(Block::BlockQuote { blocks, line: 0 }, &range, lines);
            }
            // Without this arm the table's own events fall through to the
            // inline arms below and the whole thing comes back as its cell
            // texts run together — `- item` followed by `ab12`.
            Event::Start(Tag::Table(alignments)) => {
                let aligns = alignments.into_iter().map(convert_alignment).collect();
                let (header, rows) = collect_table(events, lines);
                item.push_block(
                    Block::Table {
                        alignments: aligns,
                        header,
                        rows,
                        line: 0,
                    },
                    &range,
                    lines,
                );
            }
            Event::Start(Tag::HtmlBlock) => {
                let content = collect_html_block(events);
                item.push_block(Block::Html { content, line: 0 }, &range, lines);
            }
            Event::Rule => {
                item.push_block(Block::ThematicBreak { line: 0 }, &range, lines);
            }
            Event::Text(text) => item.push_inline(Inline::Text(text.to_string()), &range, lines),
            Event::Code(code) => item.push_inline(
                Inline::Code(CodeSpan {
                    text: code.to_string(),
                    delim,
                }),
                &range,
                lines,
            ),
            Event::Start(Tag::Strong) => {
                let inner = collect_inlines(events, TagEnd::Strong);
                item.push_inline(Inline::Strong(inner), &range, lines);
            }
            Event::Start(Tag::Emphasis) => {
                let inner = collect_inlines(events, TagEnd::Emphasis);
                item.push_inline(Inline::Emphasis(inner), &range, lines);
            }
            Event::Start(Tag::Strikethrough) => {
                let inner = collect_inlines(events, TagEnd::Strikethrough);
                item.push_inline(Inline::Strikethrough(inner), &range, lines);
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            }) => {
                let inner = collect_inlines(events, TagEnd::Link);
                item.push_inline(
                    make_link(link_type, &dest_url, &title, inner),
                    &range,
                    lines,
                );
            }
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                let inner = collect_inlines(events, TagEnd::Image);
                item.push_inline(
                    Inline::Image {
                        content: inner,
                        url: dest_url.to_string(),
                        title: opt_title(&title),
                    },
                    &range,
                    lines,
                );
            }
            Event::InlineHtml(html) => {
                item.push_inline(Inline::Html(html.to_string()), &range, lines);
            }
            Event::SoftBreak => item.push_inline(Inline::SoftBreak, &range, lines),
            Event::HardBreak => item.push_inline(Inline::HardBreak, &range, lines),
            _ => {}
        }
    }

    item.finish()
}

fn collect_code_block(events: &mut Events) -> String {
    let mut content = String::new();
    while let Some((event, _, _)) = events.pop_front() {
        match event {
            Event::End(TagEnd::CodeBlock) => break,
            Event::Text(text) => content.push_str(&text),
            _ => {}
        }
    }
    terminated(content)
}

/// Collect the raw text of an HTML block, verbatim but for its own indent.
///
/// A block inside a container reaches us with the container's prefix already
/// stripped; one at top level keeps whatever indent it was written with, up to
/// the three spaces CommonMark allows. Left in, that indent is re-emitted at
/// column 0 plus itself, and a block written under a list lands *inside* the
/// last item on the next parse. It is inert in the rendered HTML either way.
fn collect_html_block(events: &mut Events) -> String {
    let mut content = String::new();
    while let Some((event, _, _)) = events.pop_front() {
        match event {
            Event::End(TagEnd::HtmlBlock) => break,
            Event::Html(html) | Event::InlineHtml(html) | Event::Text(html) => {
                content.push_str(&html);
            }
            _ => {}
        }
    }
    terminated(dedent(content))
}

/// Remove the longest run of spaces that every non-blank line starts with.
fn dedent(content: String) -> String {
    let indent = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    if indent == 0 {
        return content;
    }
    content
        .lines()
        .map(|l| l.get(indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
        + if content.ends_with('\n') { "\n" } else { "" }
}

/// Give verbatim block content the trailing newline its serialization will have.
///
/// A code or HTML block that runs to the end of a file with no final newline
/// comes back without one, but the serializer always writes the closing fence
/// (or the next block) on its own line — so the second parse sees content that
/// ends in `\n` and the two ASTs differ over a character the text never shows.
/// Normalizing here makes `parse(serialize(d)) == parse(d)` hold.
///
/// Empty content stays empty: an empty fence holds no lines at all, and giving
/// it a newline would make it hold one blank one.
fn terminated(mut content: String) -> String {
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content
}

fn collect_container_events<'a, F>(events: &mut Events<'a>, is_end: F) -> Events<'a>
where
    F: Fn(&Event) -> bool,
{
    let mut inner = VecDeque::new();
    while let Some((event, range, delim)) = events.pop_front() {
        if is_end(&event) {
            break;
        }
        inner.push_back((event, range, delim));
    }
    inner
}

fn collect_table(
    events: &mut Events,
    lines: &LineIndex,
) -> (Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>) {
    let mut header = Vec::new();
    let mut rows = Vec::new();

    while let Some((event, range, _)) = events.pop_front() {
        match event {
            Event::End(TagEnd::Table) => break,
            Event::Start(Tag::TableHead) => {
                header = collect_table_row(events, TagEnd::TableHead, range, lines);
            }
            Event::Start(Tag::TableRow) => {
                rows.push(collect_table_row(events, TagEnd::TableRow, range, lines));
            }
            _ => {}
        }
    }

    (header, rows)
}

fn collect_table_row(
    events: &mut Events,
    end_tag: TagEnd,
    row: Range<usize>,
    lines: &LineIndex,
) -> Vec<Vec<Inline>> {
    let mut cells = Vec::new();
    let mut consumed = row.start;

    while let Some((event, range, _)) = events.pop_front() {
        match event {
            Event::End(tag) if tag == end_tag => break,
            Event::Start(Tag::TableCell) => {
                consumed = range.end.max(consumed);
                cells.push(collect_inlines(events, TagEnd::TableCell));
            }
            _ => {}
        }
    }

    // pulldown-cmark stops at the header's column count, so anything the author
    // wrote past it never reaches an event. Read it back off the row's source.
    if consumed < row.end {
        for text in overflow_cells(&lines.body[consumed..row.end]) {
            cells.push(parse_table_cell(text));
        }
    }

    cells
}

/// The cells left in a table row's source after the last one pulldown-cmark
/// reported, given the row text from that cell's end to the row's end.
///
/// GFM ends a row at the header's column count and drops the rest. They render
/// as nothing either way, but dropping them from the AST means `td fmt` deletes
/// them from the file — so they are read back here and written out again.
///
/// `rest` is the tail of one source line: leading `|` closing the last reported
/// cell, then the overflow, then an optional closing `|`. A `|` inside a cell
/// has to be written `\|` even inside a code span, so an unescaped one always
/// means a cell boundary.
fn overflow_cells(rest: &str) -> Vec<&str> {
    // The `|` that closed the last reported cell opens the overflow.
    let Some(mut inner) = rest.trim().strip_prefix('|') else {
        return Vec::new();
    };
    // A row that ends in `|` spends it closing its last cell rather than
    // opening another; one written without a closing pipe has nothing to drop.
    let bars = unescaped_pipes(inner);
    if let Some(&last) = bars.last() {
        if inner[last + 1..].trim().is_empty() {
            inner = &inner[..last];
        }
    }
    if inner.is_empty() {
        return Vec::new();
    }
    let mut cells = Vec::new();
    let mut start = 0;
    for i in unescaped_pipes(inner) {
        cells.push(&inner[start..i]);
        start = i + 1;
    }
    cells.push(&inner[start..]);
    cells
}

/// Byte offsets of every `|` in `s` that is not backslash-escaped.
///
/// A pipe inside a cell must be written `\|` — GFM applies that rule even
/// inside a code span — so an unescaped one is always a cell boundary.
fn unescaped_pipes(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '|' => out.push(i),
            _ => {}
        }
    }
    out
}

/// Parse one table cell's source into inlines.
fn parse_table_cell(text: &str) -> Vec<Inline> {
    match parse_fragment(text.trim()).into_iter().next() {
        Some(Block::Paragraph { content, .. }) => content,
        _ => Vec::new(),
    }
}

fn convert_alignment(align: pulldown_cmark::Alignment) -> ColumnAlignment {
    match align {
        pulldown_cmark::Alignment::None => ColumnAlignment::None,
        pulldown_cmark::Alignment::Left => ColumnAlignment::Left,
        pulldown_cmark::Alignment::Center => ColumnAlignment::Center,
        pulldown_cmark::Alignment::Right => ColumnAlignment::Right,
    }
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Ensure there is a `BlankLine` between every pair of consecutive non-blank
/// blocks. Prevents CommonMark from merging consecutive paragraphs into one
/// paragraph with soft breaks, and avoids lazy-continuation issues when a
/// paragraph follows a list.
pub fn normalize_blank_lines(blocks: &mut Vec<Block>) {
    let mut i = 0;
    while i < blocks.len() {
        if !matches!(blocks[i], Block::BlankLine) {
            let next_is_blank = blocks
                .get(i + 1)
                .is_some_and(|b| matches!(b, Block::BlankLine));
            if !next_is_blank && i + 1 < blocks.len() {
                blocks.insert(i + 1, Block::BlankLine);
            }
        }
        i += 1;
    }
}

/// Map each top-level frontmatter key to its 1-based source line.
///
/// Scans the raw frontmatter block for unindented `key:` lines — enough to
/// anchor diagnostics, without reimplementing a YAML position-tracking parser.
/// Nested keys and list items are indented and therefore skipped.
fn frontmatter_key_lines(content: &str) -> indexmap::IndexMap<String, usize> {
    let mut out = indexmap::IndexMap::new();
    if !content.starts_with("---") {
        return out;
    }
    // Line 1 is the opening `---`; keys start on line 2.
    for (idx, line) in content.lines().enumerate().skip(1) {
        if line.trim_end() == "---" {
            break;
        }
        if line.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim();
        if key.is_empty() {
            continue;
        }
        let key = key.trim_matches(['"', '\'']);
        out.entry(key.to_string()).or_insert(idx + 1);
    }
    out
}

/// Capture the raw source text of top-level block-scalar frontmatter values.
///
/// A folded (`>`) or literal (`|`) scalar carries line breaks the author chose;
/// the parsed value is just a string, so re-serializing loses them. Keeping the
/// source text lets [`serialize`] write the block back verbatim when the value
/// hasn't changed. Each entry spans the `key:` header line through the last
/// indented body line, newline-terminated.
fn frontmatter_block_scalars(content: &str) -> indexmap::IndexMap<String, String> {
    let mut out = indexmap::IndexMap::new();
    if !content.starts_with("---") {
        return out;
    }
    let lines: Vec<&str> = content.lines().collect();
    // Line index 0 is the opening `---`; keys start on the next line.
    let mut i = 1;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_end() == "---" {
            break;
        }
        let Some(key) = top_level_block_scalar_key(line) else {
            i += 1;
            continue;
        };
        // Body runs to the last indented line; blank lines inside it are kept,
        // trailing ones are not (they belong to the document, not the scalar).
        let mut last = i;
        let mut j = i + 1;
        while j < lines.len() {
            let body = lines[j];
            if body.trim().is_empty() {
                j += 1;
                continue;
            }
            if !body.starts_with([' ', '\t']) {
                break;
            }
            last = j;
            j += 1;
        }
        if last > i {
            let raw: String = lines[i..=last].iter().map(|l| format!("{l}\n")).collect();
            out.entry(key).or_insert(raw);
        }
        i = last + 1;
    }
    out
}

/// The key of an unindented `key: >-` / `key: |` header line, if it is one.
fn top_level_block_scalar_key(line: &str) -> Option<String> {
    if line.starts_with([' ', '\t', '#', '-']) {
        return None;
    }
    let (key, rest) = line.split_once(':')?;
    let key = key.trim().trim_matches(['"', '\'']);
    if key.is_empty() {
        return None;
    }
    let mut tokens = rest.split_whitespace();
    let indicator = tokens.next()?;
    // `>`/`|` optionally followed by chomping (`+`/`-`) and indentation digits.
    if !indicator.starts_with(['>', '|'])
        || !indicator[1..]
            .chars()
            .all(|c| c.is_ascii_digit() || c == '+' || c == '-')
    {
        return None;
    }
    // Only a comment may follow the indicator.
    if tokens.next().is_some_and(|t| !t.starts_with('#')) {
        return None;
    }
    Some(key.to_string())
}

// ── serialization internals ───────────────────────────────────────────────────

/// Serialize inline content as paragraph-position text.
///
/// `real_lines` says a wrapped line lands as a line of the document rather
/// than as a lazy continuation, which changes what it can turn into on the
/// next parse. See [`EscapeCtx::real_lines`].
fn serialize_inlines(inlines: &[Inline], real_lines: bool) -> String {
    serialize_inlines_ctx(inlines, EscapeCtx::paragraph(real_lines))
}

/// Serialize inline content, re-escaping author text for the position it lands
/// in. Parsing resolves `\*` and `&#42;` alike into plain `Inline::Text`, so
/// without this pass every escape is lost on the first `td fmt` and the text
/// comes back as live markup on the next parse.
///
/// Escapes are decided character by character, which cannot know whether a
/// delimiter would actually have found a partner — `M*A*S*H` needs none,
/// because its last `*` never matches. So the unescaped spelling gets the first
/// try and escapes are added only when it fails to re-parse. Documents that
/// were already a fixed point stay byte-identical, and the re-parse only runs
/// when there was something to escape at all.
fn serialize_inlines_ctx(inlines: &[Inline], ctx: EscapeCtx) -> String {
    let mut buf = InlineBuf::new();
    write_inlines(&mut buf, inlines, ctx, false);
    let escaped = buf.finish(ctx);
    let plain = buf.finish_plain();
    if plain != escaped && reparses_intact(&plain, ctx, inlines) {
        return plain;
    }
    escaped
}

/// Lay an inline run into `buf`, tagging author text so the escaper can tell
/// it from the delimiters and destinations the serializer emits itself.
fn write_inlines(buf: &mut InlineBuf, inlines: &[Inline], ctx: EscapeCtx, link_text: bool) {
    for inline in inlines {
        match inline {
            Inline::Text(s) => buf.push_text(s, link_text),
            Inline::Strong(inner) => {
                buf.push_markup("**");
                write_inlines(buf, inner, ctx, link_text);
                buf.push_markup("**");
            }
            Inline::Emphasis(inner) => {
                buf.push_markup("*");
                write_inlines(buf, inner, ctx, link_text);
                buf.push_markup("*");
            }
            Inline::Strikethrough(inner) => {
                buf.push_markup("~~");
                write_inlines(buf, inner, ctx, link_text);
                buf.push_markup("~~");
            }
            Inline::Link {
                content,
                url,
                title,
            } => {
                buf.push_markup("[");
                write_inlines(buf, content, ctx, true);
                buf.push_markup("]");
                buf.push_markup(&crate::ast::format_link_target(url, title.as_deref()));
            }
            Inline::Image {
                content,
                url,
                title,
            } => {
                buf.push_markup("![");
                write_inlines(buf, content, ctx, true);
                buf.push_markup("]");
                buf.push_markup(&crate::ast::format_link_target(url, title.as_deref()));
            }
            Inline::Autolink(url) => {
                buf.push_markup("<");
                buf.push_markup(&crate::escape::escape_entities(url));
                buf.push_markup(">");
            }
            Inline::Code(span) => buf.push_markup(&code_span_markup(span, ctx)),
            Inline::Html(s) => buf.push_markup(s),
            Inline::SoftBreak => buf.push_markup("\n"),
            // The backslash form, not the two trailing spaces: invisible
            // whitespace is what every editor and formatter strips first, and
            // the break would be gone again by the next pass.
            Inline::HardBreak => buf.push_markup("\\\n"),
        }
    }
}

/// A code span as markdown, with the backslashes a table cell needs.
///
/// GFM splits a row into cells on unescaped `|` *before* any inline parsing, so
/// a pipe inside a code span needs its backslash exactly as much as one in
/// prose — CommonMark's rule that escapes are inert inside a span has not come
/// into play yet. Without it the row silently grows a column and everything
/// past the pipe is dropped from the rendered table.
///
/// The rendered span is escaped whole: parsing already resolved the source's
/// escapes, so any pipe left in the output is content, never markup.
fn code_span_markup(span: &CodeSpan, ctx: EscapeCtx) -> String {
    let rendered = span.render();
    if ctx.table_cell {
        rendered.replace('|', "\\|")
    } else {
        rendered
    }
}

/// Whether `text`, placed back in the position `ctx` describes, parses to
/// exactly `inlines` again.
fn reparses_intact(text: &str, ctx: EscapeCtx, inlines: &[Inline]) -> bool {
    let (wrapped, want_cell) = if ctx.heading {
        (format!("# {text}\n"), false)
    } else if ctx.table_cell {
        (format!("| {text} |\n| --- |\n"), true)
    } else {
        (format!("{text}\n"), false)
    };
    let blocks = parse_fragment(&wrapped);
    let [block] = blocks
        .iter()
        .filter(|b| !matches!(b, Block::BlankLine))
        .collect::<Vec<_>>()[..]
    else {
        return false;
    };
    let want = merge_text(inlines);
    match (block, want_cell) {
        (Block::Paragraph { content, .. }, false) if !ctx.heading => merge_text(content) == want,
        (
            Block::Heading {
                level: 1, content, ..
            },
            false,
        ) if ctx.heading => merge_text(content) == want,
        (Block::Table { header, rows, .. }, true) => {
            rows.is_empty() && header.len() == 1 && merge_text(&header[0]) == want
        }
        _ => false,
    }
}

/// Collapse adjacent `Text` nodes. An escape or an entity ends one text event
/// and starts another, so `*x*` and `&#42;x&#42;` parse to the same characters
/// split across a different number of nodes — a difference no comparison of
/// meaning should see.
fn merge_text(inlines: &[Inline]) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::with_capacity(inlines.len());
    for inline in inlines {
        let inline = match inline {
            Inline::Strong(inner) => Inline::Strong(merge_text(inner)),
            Inline::Emphasis(inner) => Inline::Emphasis(merge_text(inner)),
            Inline::Strikethrough(inner) => Inline::Strikethrough(merge_text(inner)),
            Inline::Link {
                content,
                url,
                title,
            } => Inline::Link {
                content: merge_text(content),
                url: url.clone(),
                title: title.clone(),
            },
            Inline::Image {
                content,
                url,
                title,
            } => Inline::Image {
                content: merge_text(content),
                url: url.clone(),
                title: title.clone(),
            },
            other => other.clone(),
        };
        match (out.last_mut(), &inline) {
            (Some(Inline::Text(prev)), Inline::Text(next)) => prev.push_str(next),
            _ => out.push(inline),
        }
    }
    out
}

/// Parse a bare markdown fragment into blocks: no frontmatter, no line
/// numbers, just the block structure used to check a serialization.
fn parse_fragment(src: &str) -> Vec<Block> {
    parse_blocks(markdown_events(src), &LineIndex::new(src, 0))
}

/// Write inline content followed by a newline, re-indenting wrapped lines.
///
/// SoftBreaks serialize as a bare `\n`. Inside a list item every line after the
/// first needs the item's content indent, or a hard-wrapped sentence lands at
/// column 0 and walks out of the item.
///
/// A run holding a wrapped line that reads as a table row is the exception: it
/// stays at column 0 whole. Such a line only reached the AST as prose because
/// it was a *lazy* continuation — pulldown-cmark doesn't run the table
/// extension over lazy lines — so indenting it to the item's content column
/// promotes it to a real table, swallowing it and its neighbours into the
/// bullet and changing the rendering. And once one line is pinned there the
/// run is anchored at column 0 anyway: it parsed as one paragraph from that
/// column, so indenting the rest of it would be churn for nothing.
fn push_inlines_indented(out: &mut String, content: &[Inline], indent: &str) {
    // Anchoring is decided on the unescaped spelling, because the two answers
    // depend on each other: a run stays at column 0 because a wrapped line
    // reads as a table row, and it needs no escaping *because* it stayed
    // there. Escaping first would hide the row from this test.
    let plain = serialize_inlines(content, false);
    let anchored = !indent.is_empty() && plain.split('\n').skip(1).any(is_table_row);
    if anchored {
        out.push_str(&plain);
    } else {
        let text = serialize_inlines(content, true);
        out.push_str(&text.replace('\n', &format!("\n{indent}")));
    }
    out.push('\n');
}

/// Whether a line would be read as a row of a GFM table.
///
/// Only the leading-pipe spelling is recognized. A table written without outer
/// pipes is indistinguishable from prose one line at a time, and guessing wrong
/// costs more than the rare miss.
fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// The text an item's opening block can carry on the marker line itself.
///
/// An item that begins with a block rather than prose — `- ## sub`, `- ***` —
/// has nothing to put after its marker. Headings and thematic breaks are
/// single-line, so they ride the marker line and the item keeps its shape.
///
/// A break has to be respelled `***` there: `- ---` is four dashes separated
/// by spaces, which is *itself* a thematic break and takes precedence over the
/// list marker, so writing it would dissolve the list around it.
fn marker_line_block(block: &Block, style: ListStyle) -> Option<String> {
    match block {
        Block::Heading { level, content, .. } => Some(heading_line(*level, content)),
        Block::ThematicBreak { .. } => Some(style.thematic_break().to_string()),
        _ => None,
    }
}

/// Which of the interchangeable list markers to spell a list with.
///
/// CommonMark ends a list where the marker character changes, so two lists that
/// sit next to each other stay two lists only as long as they are written
/// differently. The AST doesn't record the author's marker — nothing renders
/// differently for it — so the serializer picks: the first list in a run gets
/// the house style, and one directly after it gets the alternate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListStyle {
    House,
    Alternate,
}

impl ListStyle {
    fn flip(self) -> Self {
        match self {
            Self::House => Self::Alternate,
            Self::Alternate => Self::House,
        }
    }

    /// The marker for item `n` of a list: `-`/`*` or `n.`/`n)`.
    fn marker(self, ordered: bool, n: u64) -> String {
        match (ordered, self) {
            (false, Self::House) => "-".to_string(),
            (false, Self::Alternate) => "*".to_string(),
            (true, Self::House) => format!("{n}."),
            (true, Self::Alternate) => format!("{n})"),
        }
    }

    /// How to spell a thematic break riding on an item's marker line.
    ///
    /// It has to differ from the bullet: `- ---` is four dashes with spaces
    /// between them, which is *itself* a thematic break and outranks the list
    /// marker, and `* ***` is the same trap one character over. Either spelling
    /// would dissolve the list around it.
    fn thematic_break(self) -> &'static str {
        match self {
            Self::House => "***",
            Self::Alternate => "---",
        }
    }
}

/// Assign a marker style to each list in a run of blocks.
///
/// Lists only merge into each other when they agree on ordered-ness — a bullet
/// list next to a numbered one is already two lists — so the alternation only
/// has to break ties within a run of the same kind. Blank lines sit between
/// adjacent lists and don't end the run; any other block does.
fn list_styles(blocks: &[Block], after_text: bool) -> Vec<ListStyle> {
    let mut styles = Vec::new();
    let mut run: Option<(bool, ListStyle)> = None;
    let mut after_paragraph = after_text;
    for block in blocks {
        match block {
            Block::BlankLine => {}
            Block::List { ordered, items, .. } => {
                let mut style = match run {
                    Some((prev, style)) if prev == *ordered => style.flip(),
                    _ => ListStyle::House,
                };
                // A lone `-` on the line under a paragraph is a setext
                // underline, not an empty list item: CommonMark resolves the
                // ambiguity against the list, and the paragraph above comes
                // back as a heading. `*` has no setext spelling. Deciding it
                // here rather than at the marker is what keeps it from
                // colliding with the list above — which can't be a `*` list,
                // because a paragraph sits between them and breaks the run.
                if after_paragraph && !*ordered && bare_marker(items) {
                    style = ListStyle::Alternate;
                }
                run = Some((*ordered, style));
                styles.push(style);
                after_paragraph = false;
            }
            other => {
                run = None;
                after_paragraph = matches!(other, Block::Paragraph { .. });
            }
        }
    }
    styles
}

/// An ATX heading on one line, without its trailing newline.
///
/// Every heading is written ATX, which is one line by definition, so a heading
/// carrying line breaks — only a setext heading can — has to be flattened. An
/// empty heading is written bare: `# ` with nothing after it is `#` plus
/// trailing whitespace, which no reviewer wants to see in a diff.
fn heading_line(level: u8, content: &[Inline]) -> String {
    let hashes = "#".repeat(level as usize);
    let text = serialize_inlines_ctx(&flatten_breaks(content), EscapeCtx::heading());
    if text.is_empty() {
        hashes
    } else {
        format!("{hashes} {text}")
    }
}

/// Replace every line break in `inlines` with a space, recursively.
///
/// A setext heading spans as many lines as its author wrote, and its content
/// reaches the parser with the `SoftBreak`s to prove it. Written back as ATX,
/// that break ends the heading: the lines below it become a separate paragraph,
/// and a second format pass reads a different document than the first.
/// Renderers already show a soft break inside a heading as a space, so
/// collapsing it is what the page said all along.
///
/// A hard break is collapsed too. It renders as `<br>`, which an ATX heading
/// has no spelling for; the text survives, the line break does not.
///
/// Applied where headings are built, so the AST holds the flattened form and
/// `parse(serialize(d))` compares equal to `parse(d)`, and again on the way out,
/// where it is what actually guarantees an ATX heading occupies one line.
fn flatten_breaks(inlines: &[Inline]) -> Vec<Inline> {
    if !contains_break(inlines) {
        return inlines.to_vec();
    }
    let flattened: Vec<Inline> = inlines
        .iter()
        .map(|inline| match inline {
            Inline::SoftBreak | Inline::HardBreak => Inline::Text(" ".to_string()),
            Inline::Strong(inner) => Inline::Strong(flatten_breaks(inner)),
            Inline::Emphasis(inner) => Inline::Emphasis(flatten_breaks(inner)),
            Inline::Strikethrough(inner) => Inline::Strikethrough(flatten_breaks(inner)),
            Inline::Link {
                content,
                url,
                title,
            } => Inline::Link {
                content: flatten_breaks(content),
                url: url.clone(),
                title: title.clone(),
            },
            Inline::Image {
                content,
                url,
                title,
            } => Inline::Image {
                content: flatten_breaks(content),
                url: url.clone(),
                title: title.clone(),
            },
            other => other.clone(),
        })
        .collect();
    // The space a break became belongs to the text beside it, or escaping sees
    // a run starting with whitespace and quotes what does not need quoting.
    merge_text(&flattened)
}

fn contains_break(inlines: &[Inline]) -> bool {
    inlines.iter().any(|inline| match inline {
        Inline::SoftBreak | Inline::HardBreak => true,
        Inline::Strong(inner)
        | Inline::Emphasis(inner)
        | Inline::Strikethrough(inner)
        | Inline::Link { content: inner, .. }
        | Inline::Image { content: inner, .. } => contains_break(inner),
        _ => false,
    })
}

/// Write a list, numbering an ordered one from `start`.
///
/// Items are renumbered consecutively from `start` — `5./5./5.` comes back as
/// `5./6./7.` — so only the author's starting number survives, which is the
/// only part a renderer shows differently.
fn serialize_list(
    out: &mut String,
    items: &[ListItem],
    ordered: bool,
    start: u64,
    indent: &str,
    style: ListStyle,
) {
    for (idx, item) in items.iter().enumerate() {
        let marker = style.marker(ordered, start.saturating_add(idx as u64));
        // Continuation indent: spaces to align with the text that follows the marker.
        // e.g. "- " → 2 spaces, "1. " → 3 spaces, "10. " → 4 spaces.
        let continuation = format!("{}{}", indent, " ".repeat(marker.len() + 1));
        out.push_str(indent);
        out.push_str(&marker);

        let mut children = item.children.as_slice();
        let mut after_text = false;
        if !item.content.is_empty() {
            out.push(' ');
            push_inlines_indented(out, &item.content, &continuation);
            after_text = true;
        } else if let Some(line) = children.first().and_then(|b| marker_line_block(b, style)) {
            out.push(' ');
            out.push_str(&line);
            out.push('\n');
            children = &children[1..];
        } else if let Some(body) = block_from_marker_line(children, &continuation) {
            // An item with no inline content still can't leave its marker alone
            // on the line: an empty list item may not interrupt a paragraph, so
            // a bare `-` under one is read as more of that paragraph and the
            // blocks below it change owner. The first block starts on the
            // marker line instead, which is where an author would put it.
            out.push(' ');
            out.push_str(&body);
            children = &[];
        } else {
            // Nothing to sit after the marker: end the line there rather than
            // leaving a lone trailing space.
            out.push('\n');
        }

        if !children.is_empty() {
            write_blocks(out, children, &continuation, after_text);
        }
    }
}

/// An item's whole body, written to start on the marker line: the first line
/// unindented, the rest at the item's content column.
///
/// `None` when there is nothing to pull up, or when the body's first line isn't
/// at the content column to begin with — a leading blank line, which has no
/// indent and nothing to sit after a marker anyway.
fn block_from_marker_line(children: &[Block], continuation: &str) -> Option<String> {
    if children.is_empty() {
        return None;
    }
    let mut body = String::new();
    write_blocks(&mut body, children, continuation, false);
    Some(body.strip_prefix(continuation)?.to_string())
}

/// Whether the list's first item leaves its marker alone on the line — no
/// inline content, and no opening block to pull up next to the marker.
///
/// The second half mirrors [`block_from_marker_line`], which pulls up anything
/// that starts at the item's content column. Only a leading blank line doesn't,
/// having no indent of its own.
fn bare_marker(items: &[ListItem]) -> bool {
    items.first().is_some_and(|item| {
        item.content.is_empty() && matches!(item.children.first(), None | Some(Block::BlankLine))
    })
}

/// Serialize a slice of blocks to a markdown string (no frontmatter).
///
/// Used by `td json` to produce per-section `markdown` output.
pub fn serialize_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    write_blocks(&mut out, blocks, "", false);
    while out.ends_with("\n\n\n") {
        out.pop();
    }
    out
}

/// `after_text` says a paragraph line was just written above these blocks —
/// the text on a list item's marker line — so a leading thematic break would
/// underline it.
fn write_blocks(out: &mut String, blocks: &[Block], indent: &str, after_text: bool) {
    let mut styles = list_styles(blocks, after_text).into_iter();
    for (i, block) in blocks.iter().enumerate() {
        // Whether a paragraph's last line sits directly above this block, with
        // no blank line in between — which is what a `-` here would underline.
        let after_paragraph = match i.checked_sub(1) {
            Some(prev) => matches!(blocks[prev], Block::Paragraph { .. }),
            None => after_text,
        };
        match block {
            Block::Heading { level, content, .. } => {
                out.push_str(indent);
                out.push_str(&heading_line(*level, content));
                out.push('\n');
            }
            Block::Paragraph { content, .. } => {
                out.push_str(indent);
                push_inlines_indented(out, content, indent);
            }
            Block::List {
                items,
                ordered,
                start,
                ..
            } => {
                let style = styles.next().unwrap_or(ListStyle::House);
                serialize_list(out, items, *ordered, *start, indent, style);
            }
            Block::CodeBlock {
                language, content, ..
            } => {
                write_code_block(out, language.as_deref(), content, indent);
            }
            Block::BlockQuote { blocks: inner, .. } => {
                let mut bq = String::new();
                serialize_blockquote(&mut bq, inner);
                for line in bq.lines() {
                    out.push_str(indent);
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Block::Table {
                alignments,
                header,
                rows,
                ..
            } => {
                serialize_table(out, alignments, header, rows, indent);
            }
            Block::ThematicBreak { .. } => {
                out.push_str(indent);
                // A `---` on the line under a paragraph is a setext underline,
                // and the whole paragraph — the list item around it, even —
                // comes back as a heading. The blank line that defuses this at
                // top level is not available inside a list item: inserting one
                // makes the list loose, which changes the rendering on its own.
                // `***` is the same thematic break with no setext spelling.
                out.push_str(if after_paragraph { "***\n" } else { "---\n" });
            }
            Block::Html { content, .. } => {
                write_verbatim(out, content, indent);
            }
            Block::BlankLine => {
                out.push('\n');
            }
        }
    }
}

/// Write a fenced code block, indented to sit inside its container.
///
/// The one place code blocks are spelled, so the top level and the inside of a
/// list item cannot drift apart: an empty block is a bare pair of fences with
/// nothing between them, and each held line is written once.
fn write_code_block(out: &mut String, language: Option<&str>, content: &str, indent: &str) {
    let fence = crate::ast::code_fence(content);
    let lang = language.unwrap_or("");
    out.push_str(indent);
    out.push_str(&format!("{fence}{lang}\n"));
    write_verbatim(out, content, indent);
    out.push_str(indent);
    out.push_str(&format!("{fence}\n"));
}

/// Write verbatim block content line by line, indenting the non-empty ones.
///
/// A blank line gets no indent: indenting it would leave trailing whitespace,
/// which is both invisible in the file and a change to the code block's content.
fn write_verbatim(out: &mut String, content: &str, indent: &str) {
    if content.is_empty() {
        return;
    }
    for line in content.strip_suffix('\n').unwrap_or(content).split('\n') {
        if !line.is_empty() {
            out.push_str(indent);
            out.push_str(line);
        }
        out.push('\n');
    }
}

fn serialize_blockquote(out: &mut String, inner: &[Block]) {
    let inner_doc = Document {
        frontmatter: None,
        blocks: inner.to_vec(),
    };
    let inner_text = serialize_doc(&inner_doc, &[], false);
    let trimmed = inner_text.trim_end_matches('\n');
    for line in trimmed.split('\n') {
        if line.is_empty() {
            out.push_str(">\n");
        } else {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

fn serialize_table(
    out: &mut String,
    alignments: &[ColumnAlignment],
    header: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    indent: &str,
) {
    let num_cols = alignments.len().max(header.len());

    out.push_str(indent);
    out.push('|');
    for i in 0..num_cols {
        let cell = header
            .get(i)
            .map(|c| serialize_inlines_ctx(c, EscapeCtx::table_cell()))
            .unwrap_or_default();
        out.push_str(&format!(" {cell} |"));
    }
    out.push('\n');

    out.push_str(indent);
    out.push('|');
    for i in 0..num_cols {
        let sep = match alignments.get(i).copied().unwrap_or(ColumnAlignment::None) {
            ColumnAlignment::None => " --- ",
            ColumnAlignment::Left => " :--- ",
            ColumnAlignment::Center => " :---: ",
            ColumnAlignment::Right => " ---: ",
        };
        out.push_str(sep);
        out.push('|');
    }
    out.push('\n');

    for row in rows {
        out.push_str(indent);
        out.push('|');
        // A row may carry more cells than the table has columns. GFM ignores
        // the surplus when rendering, so writing it back changes nothing on
        // screen — and dropping it would delete the author's data from the file.
        for i in 0..num_cols.max(row.len()) {
            let cell = row
                .get(i)
                .map(|c| serialize_inlines_ctx(c, EscapeCtx::table_cell()))
                .unwrap_or_default();
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
    }
}

/// Serialize frontmatter alone, `---` delimiters included, exactly as
/// [`serialize`] would write it at the top of a document.
///
/// For tools that own the frontmatter and hand the body back verbatim: pair
/// with [`split_frontmatter`], rewrite this half, keep the other. Building the
/// block by hand from [`yaml_scalar`] calls is how a nested value or an
/// unquoted edge case drifts from `td fmt`; this is the same emitter, guarded
/// by the same read-back verification.
pub fn serialize_frontmatter(fm: &Frontmatter) -> String {
    frontmatter_block(fm, &[])
}

/// The frontmatter block [`serialize_doc`] writes: emitted in house style,
/// verified, wrapped in its delimiters.
fn frontmatter_block(fm: &Frontmatter, field_order: &[&str]) -> String {
    let mut body = String::new();
    if field_order.is_empty() {
        // type first, then all other fields in insertion order
        if let Some(doc_type) = &fm.doc_type {
            serialize_top_field(
                &mut body,
                "type",
                &serde_yaml::Value::String(doc_type.clone()),
                fm,
            );
        }
        for (key, value) in &fm.fields {
            serialize_top_field(&mut body, key, value, fm);
        }
    } else {
        serialize_frontmatter_ordered(&mut body, fm, field_order);
    }
    format!("---\n{}---\n", verified_frontmatter(body, fm))
}

fn serialize_frontmatter_ordered(out: &mut String, fm: &Frontmatter, field_order: &[&str]) {
    let order_set: HashSet<&str> = field_order.iter().copied().collect();

    // Write fields in specified order
    for &field in field_order {
        if field == "type" {
            if let Some(doc_type) = &fm.doc_type {
                serialize_top_field(
                    out,
                    "type",
                    &serde_yaml::Value::String(doc_type.clone()),
                    fm,
                );
            }
        } else if let Some(value) = fm.fields.get(field) {
            serialize_top_field(out, field, value, fm);
        }
    }

    // Append remaining fields not in field_order (in insertion order)
    if !order_set.contains("type") {
        if let Some(doc_type) = &fm.doc_type {
            serialize_top_field(
                out,
                "type",
                &serde_yaml::Value::String(doc_type.clone()),
                fm,
            );
        }
    }
    for (key, value) in &fm.fields {
        if !order_set.contains(key.as_str()) {
            serialize_top_field(out, key, value, fm);
        }
    }
}

/// Guard the hand-written emitter's output against silent data loss.
///
/// Re-parses the frontmatter block we just wrote and compares it to the
/// frontmatter we meant to write. On any mismatch — a value retyped by an
/// unquoted scalar, a key we couldn't render — the whole block goes to
/// serde_yaml's emitter instead. Its house style is worse (sequences lose their
/// indent, block scalars collapse), but it always reads back as itself, and a
/// block that no longer parses is unrecoverable: `format_file` refuses to touch
/// a file whose frontmatter is broken, so the corruption would be one-way.
fn verified_frontmatter(body: String, fm: &Frontmatter) -> String {
    let mut expected: indexmap::IndexMap<String, serde_yaml::Value> = indexmap::IndexMap::new();
    if let Some(doc_type) = &fm.doc_type {
        expected.insert(
            "type".to_string(),
            serde_yaml::Value::String(doc_type.clone()),
        );
    }
    for (key, value) in &fm.fields {
        expected.insert(key.clone(), value.clone());
    }
    if expected.is_empty() {
        return body;
    }
    match serde_yaml::from_str::<indexmap::IndexMap<String, serde_yaml::Value>>(&body) {
        Ok(actual) if actual == expected => body,
        _ => serde_yaml::to_string(&expected).unwrap_or(body),
    }
}

/// Write one top-level frontmatter field.
///
/// A string that came in as a block scalar and hasn't been touched since is
/// written back verbatim, preserving the author's line breaks and sparing the
/// reader a 300-column quoted line with escaped inner quotes. Everything else
/// goes through [`serialize_yaml_field`].
fn serialize_top_field(out: &mut String, key: &str, value: &serde_yaml::Value, fm: &Frontmatter) {
    if let serde_yaml::Value::String(s) = value {
        if let Some(raw) = fm.block_scalars.get(key) {
            if block_scalar_value(raw, key).as_deref() == Some(s.as_str()) {
                out.push_str(raw);
                return;
            }
        }
    }
    let rendered = yaml_key(key);
    if let serde_yaml::Value::String(s) = value {
        if let Some(folded) = fold_yaml_scalar(&rendered, s) {
            out.push_str(&folded);
            return;
        }
    }
    serialize_yaml_field(out, &rendered, value, 0);
}

/// Re-parse a captured block scalar, returning the string it stands for.
///
/// `None` if it no longer parses as `key: <string>` — the value was edited, or
/// the capture clipped something it shouldn't have.
fn block_scalar_value(raw: &str, key: &str) -> Option<String> {
    let map: indexmap::IndexMap<String, serde_yaml::Value> = serde_yaml::from_str(raw).ok()?;
    match map.get(key) {
        Some(serde_yaml::Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Longest frontmatter line we'll emit before reaching for a folded scalar.
const YAML_FOLD_WIDTH: usize = 80;

/// Fold a long single-line string into a `>-` block scalar, or `None` to leave
/// it to the normal plain/quoted path.
///
/// Only applies to values that would otherwise be written as one over-long
/// double-quoted line with escaped inner quotes. Folding is only safe when the
/// text rejoins from single-space-separated words: any other whitespace, or a
/// leading/trailing space, would not survive the round trip.
fn fold_yaml_scalar(key: &str, s: &str) -> Option<String> {
    if key.len() + 2 + s.len() <= YAML_FOLD_WIDTH || !needs_yaml_quoting(s) {
        return None;
    }
    let words: Vec<&str> = s.split(' ').collect();
    if words
        .iter()
        .any(|w| w.is_empty() || w.contains(char::is_whitespace))
    {
        return None;
    }

    let indent = "  ";
    let mut out = format!("{key}: >-\n");
    let mut line = String::new();
    for word in words {
        if line.is_empty() {
            line.push_str(word);
        } else if indent.len() + line.len() + 1 + word.len() <= YAML_FOLD_WIDTH {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push_str(&format!("{indent}{line}\n"));
            line = word.to_string();
        }
    }
    out.push_str(&format!("{indent}{line}\n"));
    Some(out)
}

/// Write `key: value` at `indent`. `key` is already-rendered YAML source text —
/// see [`render_yaml_key`], which handles quoting and non-string keys.
fn serialize_yaml_field(out: &mut String, key: &str, value: &serde_yaml::Value, indent: usize) {
    let indent_str = "  ".repeat(indent);
    match value {
        serde_yaml::Value::Null => {
            out.push_str(&format!("{indent_str}{key}:\n"));
        }
        serde_yaml::Value::Bool(b) => {
            out.push_str(&format!("{indent_str}{key}: {b}\n"));
        }
        serde_yaml::Value::Number(n) => {
            out.push_str(&format!("{indent_str}{key}: {n}\n"));
        }
        serde_yaml::Value::String(s) => {
            out.push_str(&format!("{indent_str}{key}: {}\n", yaml_scalar(s)));
        }
        serde_yaml::Value::Sequence(seq) => {
            if seq.is_empty() {
                out.push_str(&format!("{indent_str}{key}: []\n"));
            } else {
                out.push_str(&format!("{indent_str}{key}:\n"));
                for item in seq {
                    serialize_yaml_list_item(out, item, indent + 1);
                }
            }
        }
        serde_yaml::Value::Mapping(map) => {
            // An empty mapping must be written `{}`: a bare `key:` reads back
            // as null, not as an empty map.
            if map.is_empty() {
                out.push_str(&format!("{indent_str}{key}: {{}}\n"));
                return;
            }
            out.push_str(&format!("{indent_str}{key}:\n"));
            match map.keys().map(render_yaml_key).collect::<Option<Vec<_>>>() {
                Some(keys) => {
                    for (k, v) in keys.iter().zip(map.values()) {
                        serialize_yaml_field(out, k, v, indent + 1);
                    }
                }
                // A key with no inline form (a sequence or mapping key, which
                // needs YAML's `? key` syntax). serde_yaml can write it; we
                // can't, and dropping it would delete the entry.
                None => push_serde_block(out, value, indent + 1),
            }
        }
        serde_yaml::Value::Tagged(tagged) => {
            // The tag has to sit on the key's line, with the node it tags
            // beneath it: `key: !Tag`, then the mapping or sequence indented
            // under that.
            let Ok(text) = serde_yaml::to_string(value) else {
                return;
            };
            let mut lines = text.lines();
            let Some(first) = lines.next() else {
                return;
            };
            debug_assert!(first.starts_with(&tagged.tag.to_string()));
            out.push_str(&format!("{indent_str}{key}: {first}\n"));
            let inner = "  ".repeat(indent + 1);
            for line in lines {
                out.push_str(&format!("{inner}{line}\n"));
            }
        }
    }
}

/// Append `value` as serde_yaml writes it, every line shifted to `indent`.
///
/// The escape hatch for nodes the hand-written path has no house style for.
/// Shifting whole lines is safe because the emitted block is self-contained and
/// internally consistent — only its base column changes.
fn push_serde_block(out: &mut String, value: &serde_yaml::Value, indent: usize) {
    let Ok(text) = serde_yaml::to_string(value) else {
        return;
    };
    let indent_str = "  ".repeat(indent);
    for line in text.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            out.push_str(&format!("{indent_str}{line}\n"));
        }
    }
}

/// [`push_serde_block`], but the first line takes a `- ` list marker.
fn push_serde_list_item(out: &mut String, value: &serde_yaml::Value, indent: usize) {
    let Ok(text) = serde_yaml::to_string(value) else {
        return;
    };
    let indent_str = "  ".repeat(indent);
    let mut lines = text.lines();
    if let Some(first) = lines.next() {
        out.push_str(&format!("{indent_str}- {first}\n"));
    }
    for line in lines {
        out.push_str(&format!("{indent_str}  {line}\n"));
    }
}

fn serialize_yaml_list_item(out: &mut String, value: &serde_yaml::Value, indent: usize) {
    let indent_str = "  ".repeat(indent);
    match value {
        serde_yaml::Value::String(s) => {
            out.push_str(&format!("{indent_str}- {}\n", yaml_scalar(s)));
        }
        serde_yaml::Value::Number(n) => {
            out.push_str(&format!("{indent_str}- {n}\n"));
        }
        serde_yaml::Value::Bool(b) => {
            out.push_str(&format!("{indent_str}- {b}\n"));
        }
        serde_yaml::Value::Mapping(map) => {
            // A record is one list item, not one item per key: the first key
            // takes the `- ` marker and the rest line up beneath it.
            let Some(keys) = map.keys().map(render_yaml_key).collect::<Option<Vec<_>>>() else {
                push_serde_list_item(out, value, indent);
                return;
            };
            let mut body = String::new();
            for (k, v) in keys.iter().zip(map.values()) {
                serialize_yaml_field(&mut body, k, v, indent + 1);
            }
            if body.is_empty() {
                out.push_str(&format!("{indent_str}- {{}}\n"));
            } else {
                push_marked_item(out, &body, indent);
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            if seq.is_empty() {
                out.push_str(&format!("{indent_str}- []\n"));
            } else {
                let mut body = String::new();
                for item in seq {
                    serialize_yaml_list_item(&mut body, item, indent + 1);
                }
                push_marked_item(out, &body, indent);
            }
        }
        // Null and tagged nodes: no house style, so serde_yaml writes them.
        _ => push_serde_list_item(out, value, indent),
    }
}

/// Attach the `- ` marker to a block already serialized one level deeper.
///
/// The body's first line loses that extra level of indent to make room for the
/// marker, which is exactly as wide; every following line is left alone and so
/// aligns under it.
fn push_marked_item(out: &mut String, body: &str, indent: usize) {
    let inner = "  ".repeat(indent + 1);
    let mut lines = body.lines();
    if let Some(first) = lines.next() {
        let first = first.strip_prefix(&inner).unwrap_or(first);
        out.push_str(&format!("{}- {first}\n", "  ".repeat(indent)));
    }
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
}

/// Render a mapping key as YAML source text.
///
/// `None` for keys with no inline form — sequences and mappings, which need
/// YAML's `? key` syntax, and tagged nodes. The caller hands those to
/// serde_yaml rather than dropping them.
fn render_yaml_key(key: &serde_yaml::Value) -> Option<String> {
    match key {
        serde_yaml::Value::String(s) => Some(yaml_key(s)),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Null => Some("~".to_string()),
        _ => None,
    }
}

/// Render `s` as a YAML mapping key the way `td fmt` would — quoted and
/// escaped unless it reads back as itself unquoted.
///
/// The key-position sibling of [`yaml_scalar`]: the quoting rules differ (a
/// key tolerates spellings a value doesn't, and vice versa), which is exactly
/// why a tool quoting keys with value rules drifts from `td fmt`.
pub fn yaml_key(s: &str) -> String {
    if plain_yaml_key_roundtrips(s) {
        s.to_string()
    } else {
        format!("\"{}\"", escape_yaml_string(s))
    }
}

/// Render `s` as a YAML scalar the way `td fmt` would — quoted and escaped when
/// it has to be, bare when it doesn't.
///
/// Public because other tools in the monorepo write frontmatter that `td fmt`
/// then reformats. Any disagreement, in either direction, means the two rewrite
/// each other's output forever; calling this makes a format run a no-op by
/// construction instead of by iteration.
pub fn yaml_scalar(s: &str) -> String {
    if needs_yaml_quoting(s) {
        format!("\"{}\"", escape_yaml_string(s))
    } else {
        s.to_string()
    }
}

/// True when `s` must be double-quoted rather than written as a plain scalar.
///
/// The prefixes and keywords are a fast path. The authoritative clause is the
/// last one, which writes the scalar out and reads it back: anything that
/// doesn't return as the identical string gets quoted. Hand-approximating
/// YAML's plain-scalar grammar is what used to let quoted source values come
/// back retyped (`"0x1F"` as an integer, `".inf"` as a float), reinterpreted
/// (`"[draft] Foo"` as a flow sequence), or not parse at all (`"- item"`, or a
/// leading `` ` ``/`%`/`@`/`|`/`>`).
pub fn needs_yaml_quoting(s: &str) -> bool {
    s.is_empty()
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains(':')
        || s.contains('#')
        || s.contains('\n')
        || s.contains('"')
        || s.contains('\'')
        || s.starts_with('!')
        || s.starts_with('&')
        || s.starts_with('*')
        || s == "true"
        || s == "false"
        || s == "null"
        || s == "~"
        || s.parse::<f64>().is_ok()
        || !plain_yaml_scalar_roundtrips(s)
}

/// Does `s`, written unquoted after `key: `, read back as the same string?
pub fn plain_yaml_scalar_roundtrips(s: &str) -> bool {
    let Ok(serde_yaml::Value::Mapping(map)) =
        serde_yaml::from_str::<serde_yaml::Value>(&format!("k: {s}\n"))
    else {
        return false;
    };
    map.len() == 1 && map.values().next() == Some(&serde_yaml::Value::String(s.to_string()))
}

/// Does `s`, written unquoted as a mapping key, read back as the same string?
pub fn plain_yaml_key_roundtrips(s: &str) -> bool {
    let Ok(serde_yaml::Value::Mapping(map)) =
        serde_yaml::from_str::<serde_yaml::Value>(&format!("{s}: 0\n"))
    else {
        return false;
    };
    map.len() == 1 && map.keys().next() == Some(&serde_yaml::Value::String(s.to_string()))
}

pub fn escape_yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            // Control characters have no literal form inside double quotes, and
            // NEL / LS / PS would read back as line breaks.
            c if (c as u32) < 0x20 || matches!(c as u32, 0x7f | 0x85) => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blessed write path end to end: a `Serialize` struct becomes
    /// frontmatter, constructors build the body, `serialize` emits a document
    /// that needs quoting and escaping in several places — and the output is
    /// both correct and a fixed point.
    #[test]
    fn test_built_document_serializes_escaped_and_round_trips() {
        #[derive(serde::Serialize)]
        struct Meta {
            r#type: String,
            title: String,
            year: u64,
            genres: Vec<String>,
        }
        let fm = Frontmatter::from_serialize(&Meta {
            r#type: "movie".to_string(),
            title: "Mission: Impossible *Special* Edition".to_string(),
            year: 1996,
            genres: vec!["Action".to_string(), "true".to_string()],
        })
        .unwrap();
        let doc = Document::new(
            Some(fm),
            vec![
                Block::heading(1, "Mission: Impossible *Special* Edition"),
                Block::paragraph("A [very] good film."),
                Block::bullet_list([ListItem::text("stars: many"), ListItem::text("plain")]),
            ],
        );
        let out = serialize(&doc);
        assert_eq!(
            out,
            "---\n\
             type: movie\n\
             title: \"Mission: Impossible *Special* Edition\"\n\
             year: 1996\n\
             genres:\n  - Action\n  - \"true\"\n\
             ---\n\
             # Mission: Impossible \\*Special\\* Edition\n\n\
             A [very] good film.\n\n\
             - stars: many\n- plain\n"
        );
        // The escaped output reads back as the same content, and re-serializing
        // the parse is byte-identical — built documents are formatted documents.
        let reparsed = parse(&out);
        assert_eq!(serialize(&reparsed), out);
        let fm = reparsed.frontmatter.unwrap();
        assert_eq!(
            fm.scalar_str("title").as_deref(),
            Some("Mission: Impossible *Special* Edition")
        );
        assert_eq!(
            fm.field::<Vec<String>>("genres").unwrap().unwrap(),
            vec!["Action".to_string(), "true".to_string()]
        );
    }

    /// A non-mapping value is an error, not an empty frontmatter.
    #[test]
    fn test_from_serialize_rejects_non_mapping() {
        assert!(Frontmatter::from_serialize(&"just a string").is_err());
        assert!(Frontmatter::from_serialize(&vec![1, 2, 3]).is_err());
    }

    /// `serialize_frontmatter` writes exactly the block `serialize` opens a
    /// document with — one emitter, two entry points.
    #[test]
    fn test_serialize_frontmatter_matches_document_serialization() {
        let src = "---\ntype: journal\ntitle: \"one: two\"\ntags:\n  - a\n---\nBody.\n";
        let doc = parse(src);
        let fm = doc.frontmatter.clone().unwrap();
        let block = serialize_frontmatter(&fm);
        assert!(serialize(&doc).starts_with(&block));
        assert_eq!(
            block,
            "---\ntype: journal\ntitle: \"one: two\"\ntags:\n  - a\n---\n"
        );
    }

    #[test]
    fn test_frontmatter_field_reads_one_key() {
        let src = "---\ntype: recipe\nservings: 4\n---\n# Hi\n";
        assert_eq!(frontmatter_field::<u32>(src, "servings").unwrap(), Some(4));
        assert_eq!(frontmatter_field::<u32>(src, "absent").unwrap(), None);
        assert_eq!(
            frontmatter_field::<String>("no frontmatter\n", "k").unwrap(),
            None
        );
        // A present-but-wrong value must surface as an error, not as absent.
        assert!(frontmatter_field::<u32>(src, "type").is_err());
    }

    /// What a document *says*, with every source position erased.
    ///
    /// Comparing two of these answers one question: did a format pass change
    /// the content? Line numbers are the one thing a pass is allowed to move —
    /// blank lines get normalized, frontmatter gets re-emitted, and every block
    /// below shifts — so comparing whole `Document`s would report that as
    /// drift. The positions come off, adjacent text runs merge, and what's left
    /// is what the document says.
    #[derive(Debug, PartialEq)]
    struct Content {
        doc_type: Option<String>,
        fields: Vec<(String, serde_yaml::Value)>,
        blocks: Vec<Block>,
    }

    /// The [`Content`] of `input` — what survives a parse, minus positions.
    fn content_of(input: &str) -> Content {
        let doc = parse(input);
        let mut blocks = doc.blocks;
        for block in &mut blocks {
            normalize_block(block);
        }
        Content {
            doc_type: doc.frontmatter.as_ref().and_then(|f| f.doc_type.clone()),
            fields: doc
                .frontmatter
                .map(|f| f.fields.into_iter().collect())
                .unwrap_or_default(),
            blocks,
        }
    }

    /// Erase `block`'s source line and coalesce its text runs, recursively.
    fn normalize_block(block: &mut Block) {
        match block {
            Block::Heading { content, line, .. } | Block::Paragraph { content, line } => {
                *line = 0;
                normalize_inlines(content);
            }
            Block::CodeBlock { line, .. } | Block::Html { line, .. } => *line = 0,
            Block::ThematicBreak { line } => *line = 0,
            Block::Table {
                header, rows, line, ..
            } => {
                *line = 0;
                header.iter_mut().for_each(normalize_inlines);
                rows.iter_mut()
                    .flat_map(|r| r.iter_mut())
                    .for_each(normalize_inlines);
            }
            Block::BlockQuote { blocks, line } => {
                *line = 0;
                blocks.iter_mut().for_each(normalize_block);
            }
            Block::List { items, line, .. } => {
                *line = 0;
                for item in items {
                    normalize_inlines(&mut item.content);
                    item.children.iter_mut().for_each(normalize_block);
                }
            }
            Block::BlankLine => {}
        }
    }

    /// Merge adjacent [`Inline::Text`] runs and drop empty ones, recursively.
    ///
    /// Where one run ends and the next begins is an artifact of how the source
    /// spelled things — `&#42;a&#42;` splits around each entity, `\*a\*` around
    /// each backslash — and the two spellings say the same sentence. Comparing
    /// the fragments instead of the text would call that a difference.
    fn normalize_inlines(inlines: &mut Vec<Inline>) {
        let mut out: Vec<Inline> = Vec::with_capacity(inlines.len());
        for mut inline in inlines.drain(..) {
            match &mut inline {
                Inline::Text(s) => {
                    if s.is_empty() {
                        continue;
                    }
                    if let Some(Inline::Text(prev)) = out.last_mut() {
                        prev.push_str(s);
                        continue;
                    }
                }
                Inline::Strong(kids)
                | Inline::Emphasis(kids)
                | Inline::Strikethrough(kids)
                | Inline::Link { content: kids, .. }
                | Inline::Image { content: kids, .. } => normalize_inlines(kids),
                _ => {}
            }
            out.push(inline);
        }
        *inlines = out;
    }

    /// Parse, serialize, parse again, serialize again — assert idempotency.
    ///
    /// Two properties, both of which have been silently false at some point:
    ///
    /// - the second pass is a fixed point, so `td fmt` run twice is `td fmt`
    ///   run once;
    /// - the first pass didn't change what the document *says*, so a construct
    ///   can't be dropped or mangled on the way out and then read back as
    ///   perfectly stable garbage.
    ///
    /// The second is the one the old harness was missing, and it is why this
    /// helper is worth calling even where byte [`identity`] is not expected.
    fn roundtrip(input: &str) -> String {
        let doc = parse(input);
        let s1 = serialize(&doc);
        let doc2 = parse(&s1);
        let s2 = serialize(&doc2);
        assert_eq!(s1, s2, "serialization must be idempotent:\n{s1}");
        assert_eq!(
            content_of(input),
            content_of(&s1),
            "parse(serialize(d)) must say what parse(d) said\n--- in:\n{input}\n--- out:\n{s1}"
        );
        s1
    }

    /// Parse then serialize — assert the output is byte-identical to the input.
    ///
    /// Stronger than [`roundtrip`], which only proves the second pass is stable:
    /// a construct that is silently dropped is idempotent but still data loss.
    fn identity(input: &str) {
        assert_eq!(
            roundtrip(input),
            input,
            "parse → serialize must preserve the source byte-for-byte"
        );
    }

    // ── the frontmatter split ─────────────────────────────────────────────────

    #[test]
    fn test_split_frontmatter_halves_reassemble_the_source() {
        let src = "---\nshow: X\n---\n\n# Title\n";
        let split = split_frontmatter(src);
        assert_eq!(split.frontmatter, Some("show: X\n"));
        assert_eq!(split.body, "\n# Title\n");
        assert_eq!(split.frontmatter_lines, 3);
        // Everything the split names is a slice of the source, so a caller that
        // rewrites one half writes the other back byte for byte.
        let close = split.close.unwrap();
        assert_eq!(&src[..close], "---\nshow: X\n");
        assert_eq!(&src[close..], "---\n\n# Title\n");
    }

    #[test]
    fn test_split_frontmatter_close_is_where_a_field_is_spliced() {
        let src = "---\nshow: X\n---\n\n# Title\n";
        let mut out = src.to_string();
        out.insert_str(split_frontmatter(src).close.unwrap(), "imdb: tt1\n");
        assert_eq!(out, "---\nshow: X\nimdb: tt1\n---\n\n# Title\n");
    }

    #[test]
    fn test_split_frontmatter_absent_and_empty_are_different() {
        let none = split_frontmatter("# Title\n");
        assert_eq!(none.frontmatter, None);
        assert_eq!(none.body, "# Title\n");
        assert_eq!(none.close, None);

        let empty = split_frontmatter("---\n---\n\n# Title\n");
        assert_eq!(empty.frontmatter, Some(""));
        assert_eq!(empty.body, "\n# Title\n");
    }

    #[test]
    fn test_split_frontmatter_leaves_a_thematic_break_alone() {
        // An unterminated `---` is a thematic break, not an opener: neither half
        // may swallow it.
        for src in ["---\n\n# Title\n", "---foo\nbar\n"] {
            let split = split_frontmatter(src);
            assert_eq!(split.frontmatter, None, "{src:?}");
            assert_eq!(split.body, src, "{src:?}");
        }
    }

    // ── frontmatter without the body ──────────────────────────────────────────

    #[test]
    fn test_parse_frontmatter_matches_a_full_parse() {
        let src = "---\ntype: movie\nshow: X\nnote: |\n  a\n  b\n---\n\n# Title\n";
        let alone = parse_frontmatter(src).unwrap().unwrap();
        let whole = parse(src).frontmatter.unwrap();
        assert_eq!(alone.doc_type, whole.doc_type);
        assert_eq!(alone.fields, whole.fields);
        assert_eq!(alone.lines, whole.lines);
        assert_eq!(alone.block_scalars, whole.block_scalars);
    }

    #[test]
    fn test_parse_frontmatter_absent_versus_malformed() {
        assert!(parse_frontmatter("# Title\n").unwrap().is_none());
        // A scalar where a mapping belongs is an error, not an absence: a caller
        // deciding anything on a field must not read it as "no frontmatter".
        assert!(parse_frontmatter("---\njust a string\n---\n").is_err());
        assert!(get_frontmatter_error("---\njust a string\n---\n").is_some());
    }

    #[test]
    fn test_frontmatter_field_deserializes_into_a_caller_type() {
        #[derive(Debug, PartialEq, serde::Deserialize)]
        struct Placement {
            volume: String,
            path: String,
        }
        let src = "---\nplacements:\n  - volume: v\n    path: p\nempty:\n---\n\n# T\n";
        let fm = parse_frontmatter(src).unwrap().unwrap();
        assert_eq!(
            fm.field::<Vec<Placement>>("placements").unwrap(),
            Some(vec![Placement {
                volume: "v".into(),
                path: "p".into()
            }])
        );
        // Absent and null both read as "nothing recorded"...
        assert_eq!(fm.field::<Vec<Placement>>("missing").unwrap(), None);
        assert_eq!(fm.field::<Vec<Placement>>("empty").unwrap(), None);
        // ...while a key spelled wrong is an error.
        let bad = parse_frontmatter("---\nplacements: 3\n---\n")
            .unwrap()
            .unwrap();
        assert!(bad.field::<Vec<Placement>>("placements").is_err());
    }

    #[test]
    fn test_frontmatter_scalar_str_resolves_quoting_and_type() {
        let fm = parse_frontmatter(
            "---\ntype: movie\na: \"0123\"  # c\nb: 1024\nc: true\nd:\n  - x\n---\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(fm.scalar_str("a").as_deref(), Some("0123"));
        assert_eq!(fm.scalar_str("b").as_deref(), Some("1024"));
        assert_eq!(fm.scalar_str("c").as_deref(), Some("true"));
        // A sequence has no scalar spelling.
        assert_eq!(fm.scalar_str("d"), None);
        // `type` lives apart from `fields` but still answers as a field.
        assert_eq!(fm.scalar_str("type").as_deref(), Some("movie"));
        assert!(fm.has("type") && fm.has("d") && !fm.has("nope"));
    }

    // ── parsing ───────────────────────────────────────────────────────────────

    #[test]
    fn test_parse_heading_level_and_text() {
        let doc = parse("# Hello World\n");
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Heading { level, .. } => assert_eq!(*level, 1),
            _ => panic!("expected heading, got {:?}", doc.blocks[0]),
        }
        match &doc.blocks[0] {
            Block::Heading { content, .. } => {
                assert_eq!(crate::ast::inlines_to_string(content), "Hello World")
            }
            _ => panic!("expected heading"),
        }
    }

    #[test]
    fn test_parse_all_heading_levels() {
        for level in 1u8..=6 {
            let hashes = "#".repeat(level as usize);
            let doc = parse(&format!("{hashes} Title\n"));
            match &doc.blocks[0] {
                Block::Heading { level: l, .. } => assert_eq!(*l, level),
                _ => panic!("expected heading"),
            }
        }
    }

    #[test]
    fn test_parse_heading_line_number() {
        let doc = parse("# Title\n");
        assert_eq!(doc.blocks[0].line(), 1);
    }

    #[test]
    fn test_parse_heading_line_number_after_frontmatter() {
        // frontmatter = 3 lines (---\ntype: foo\n---), heading on line 4
        let input = "---\ntype: foo\n---\n# Title\n";
        let doc = parse(input);
        let h = doc
            .blocks
            .iter()
            .find(|b| matches!(b, Block::Heading { .. }))
            .unwrap();
        assert_eq!(h.line(), 4);
    }

    #[test]
    fn test_blank_line_inserted_after_heading() {
        let doc = parse("# Title\nParagraph right after.\n");
        assert!(matches!(doc.blocks[0], Block::Heading { .. }));
        assert!(matches!(doc.blocks[1], Block::BlankLine));
        assert_eq!(doc.blocks[1].line(), 0, "BlankLine has no source position");
    }

    #[test]
    fn test_parse_paragraph_line_number() {
        let doc = parse("A paragraph.\n");
        assert!(matches!(doc.blocks[0], Block::Paragraph { .. }));
        assert_eq!(doc.blocks[0].line(), 1);
    }

    /// A list item holding an inline tag *and* a nested block used to leave the
    /// old line-number pass at depth 0 partway through the list, so the nested
    /// code block was mistaken for a top-level one and every block after the
    /// list inherited a line number one slot too early.
    #[test]
    fn test_line_numbers_survive_inline_tags_inside_list_items() {
        let doc = parse("- [a](u)\n\n  ```\n  x\n  ```\n\n# H\n");
        let lines: Vec<usize> = doc
            .blocks
            .iter()
            .filter(|b| !matches!(b, Block::BlankLine))
            .map(|b| b.line())
            .collect();
        assert_eq!(lines, vec![1, 7], "list on line 1, heading on line 7");
    }

    /// The same shape one level deeper: a nested list inside a list item.
    #[test]
    fn test_line_numbers_survive_nested_lists() {
        let doc = parse("- *a*\n  - b\n\n> q\n\n# H\n");
        let lines: Vec<usize> = doc
            .blocks
            .iter()
            .filter(|b| !matches!(b, Block::BlankLine))
            .map(|b| b.line())
            .collect();
        assert_eq!(lines, vec![1, 4, 6]);
    }

    /// `parse_blocks` turns a paragraph with no collectable inline content into
    /// a `BlankLine`, which carries no line of its own. Blocks after it must
    /// still land on their own source lines.
    #[test]
    fn test_empty_paragraph_does_not_shift_later_line_numbers() {
        let body = "\n\n# H\n";
        let lines = LineIndex::new(body, 0);
        let events: Events = VecDeque::from(vec![
            (Event::Start(Tag::Paragraph), 0..1, None),
            (Event::End(TagEnd::Paragraph), 0..1, None),
            (
                Event::Start(Tag::Heading {
                    level: HeadingLevel::H1,
                    id: None,
                    classes: vec![],
                    attrs: vec![],
                }),
                2..6,
                None,
            ),
            (Event::Text("H".into()), 4..5, None),
            (Event::End(TagEnd::Heading(HeadingLevel::H1)), 2..6, None),
        ]);

        let blocks = parse_blocks(events, &lines);

        assert!(matches!(blocks[0], Block::BlankLine));
        let heading = blocks
            .iter()
            .find(|b| matches!(b, Block::Heading { .. }))
            .expect("heading");
        assert_eq!(heading.line(), 3);
    }

    #[test]
    fn test_parse_unordered_list() {
        let doc = parse("- Item 1\n- Item 2\n- Item 3\n");
        assert_eq!(doc.blocks.len(), 1);
        if let Block::List { items, ordered, .. } = &doc.blocks[0] {
            assert!(!ordered);
            assert_eq!(items.len(), 3);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn test_parse_ordered_list() {
        let doc = parse("1. First\n2. Second\n");
        if let Block::List { ordered, .. } = &doc.blocks[0] {
            assert!(ordered);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn test_parse_code_block_with_language() {
        let doc = parse("```rust\nfn main() {}\n```\n");
        assert_eq!(doc.blocks.len(), 1);
        if let Block::CodeBlock {
            language, content, ..
        } = &doc.blocks[0]
        {
            assert_eq!(language.as_deref(), Some("rust"));
            assert_eq!(content, "fn main() {}\n");
        } else {
            panic!("expected code block");
        }
    }

    #[test]
    fn test_parse_code_block_no_language() {
        let doc = parse("```\njust code\n```\n");
        if let Block::CodeBlock { language, .. } = &doc.blocks[0] {
            assert!(language.is_none());
        } else {
            panic!("expected code block");
        }
    }

    #[test]
    fn test_parse_blockquote() {
        let doc = parse("> Some quoted text\n");
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::BlockQuote { .. })));
    }

    #[test]
    fn test_parse_thematic_break() {
        let doc = parse("Text\n\n---\n\nMore text\n");
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::ThematicBreak { .. })));
    }

    #[test]
    fn test_parse_link() {
        let doc = parse("Check out [this link](https://example.com)\n");
        if let Block::Paragraph { content, .. } = &doc.blocks[0] {
            let has_link = content.iter().any(|i| {
                matches!(i, Inline::Link { url, .. }
                    if url == "https://example.com")
            });
            assert!(has_link);
            // Verify the link text via inlines_to_string
            if let Some(Inline::Link { content: inner, .. }) =
                content.iter().find(|i| matches!(i, Inline::Link { .. }))
            {
                assert_eq!(crate::ast::inlines_to_string(inner), "this link");
            }
        } else {
            panic!("expected paragraph");
        }
    }

    #[test]
    fn test_parse_image() {
        let doc = parse("![alt text](image.png)\n");
        if let Block::Paragraph { content, .. } = &doc.blocks[0] {
            let has_image = content.iter().any(|i| {
                matches!(i, Inline::Image { url, .. }
                    if url == "image.png")
            });
            assert!(has_image);
            // Verify the alt text via inlines_to_string
            if let Some(Inline::Image { content: inner, .. }) =
                content.iter().find(|i| matches!(i, Inline::Image { .. }))
            {
                assert_eq!(crate::ast::inlines_to_string(inner), "alt text");
            }
        } else {
            panic!("expected paragraph");
        }
    }

    #[test]
    fn test_parse_table() {
        let doc = parse("| Name | Age |\n| --- | --- |\n| Alice | 30 |\n");
        assert!(doc.blocks.iter().any(|b| matches!(b, Block::Table { .. })));
    }

    // ── frontmatter ───────────────────────────────────────────────────────────

    #[test]
    fn test_parse_frontmatter_type_extracted() {
        let content = "---\ntype: recipe\nservings: 4\ncuisine: italian\n---\n# Pasta\n";
        let doc = parse(content);
        let fm = doc.frontmatter.as_ref().expect("expected frontmatter");
        assert_eq!(fm.doc_type.as_deref(), Some("recipe"));
        assert!(
            !fm.fields.contains_key("type"),
            "type must not appear in fields"
        );
        assert!(fm.fields.contains_key("servings"));
        assert!(fm.fields.contains_key("cuisine"));
    }

    #[test]
    fn test_parse_frontmatter_key_lines() {
        let content = "---\ntype: recipe\nservings: 4\ntags:\n  - fast\n  - cheap\ncuisine: italian\n---\n# Pasta\n";
        let fm = parse(content).frontmatter.expect("expected frontmatter");
        assert_eq!(fm.line_of("type"), 2);
        assert_eq!(fm.line_of("servings"), 3);
        assert_eq!(fm.line_of("tags"), 4);
        // Nested items are indented, so they don't shadow the parent key.
        assert_eq!(fm.line_of("cuisine"), 7);
        // Unknown keys fall back to the opening `---`.
        assert_eq!(fm.line_of("nope"), 1);
    }

    #[test]
    fn test_parse_frontmatter_key_lines_ignore_body() {
        // A `key: value` line after the closing `---` must not be picked up.
        let content = "---\ntype: recipe\n---\n# Pasta\n\nservings: many\n";
        let fm = parse(content).frontmatter.expect("expected frontmatter");
        assert_eq!(fm.line_of("servings"), 1);
    }

    #[test]
    fn test_parse_frontmatter_no_type() {
        let content = "---\ncreated: 2024-01-01\ndescription: foo\n---\n# Title\n";
        let doc = parse(content);
        let fm = doc.frontmatter.as_ref().expect("expected frontmatter");
        assert!(fm.doc_type.is_none());
        assert!(fm.fields.contains_key("created"));
        assert!(fm.fields.contains_key("description"));
    }

    #[test]
    fn test_parse_no_frontmatter() {
        let doc = parse("# Just a heading\n");
        assert!(doc.frontmatter.is_none());
    }

    #[test]
    fn test_get_frontmatter_error_bad_type() {
        // type field is a list instead of a string
        let bad = "---\ntype:\n  - not\n  - a string\n---\n# Title\n";
        assert!(get_frontmatter_error(bad).is_some());
    }

    #[test]
    fn test_get_frontmatter_error_valid() {
        let good = "---\ntype: recipe\nservings: 4\n---\n# Title\n";
        assert!(get_frontmatter_error(good).is_none());
    }

    // ── serialization ─────────────────────────────────────────────────────────

    #[test]
    fn test_serialize_type_comes_first() {
        let content = "---\nservings: 4\ntype: recipe\ncuisine: italian\n---\n\n# Pasta\n";
        let doc = parse(content);
        let out = serialize(&doc);
        assert!(out.contains("type: recipe"));
        // type should come before servings and cuisine
        let type_pos = out.find("type:").unwrap();
        let servings_pos = out.find("servings:").unwrap();
        assert!(type_pos < servings_pos, "type must come first:\n{out}");
    }

    #[test]
    fn test_serialize_with_field_order() {
        let content = "---\ntype: recipe\nservings: 4\ncuisine: italian\n---\n\n# Pasta\n";
        let doc = parse(content);
        let out = serialize_with_field_order(&doc, &["servings", "cuisine", "type"]);
        let servings_pos = out.find("servings:").unwrap();
        let cuisine_pos = out.find("cuisine:").unwrap();
        let type_pos = out.find("type:").unwrap();
        assert!(servings_pos < cuisine_pos);
        assert!(cuisine_pos < type_pos);
    }

    // ── frontmatter block scalars ─────────────────────────────────────────────
    //
    // Fixtures have no blank line after the closing `---` and list keys in the
    // order the serializer emits them, so `identity` is testing the block
    // scalar and nothing else.

    #[test]
    fn test_folded_scalar_stays_folded() {
        identity(concat!(
            "---\n",
            "description: >-\n",
            "  A folded scalar that spans\n",
            "  several lines for readability.\n",
            "---\n",
            "# Doc\n",
        ));
    }

    #[test]
    fn test_literal_scalar_stays_literal() {
        identity(concat!(
            "---\n",
            "type: note\n",
            "steps: |\n",
            "  first line\n",
            "  second line\n",
            "---\n",
            "# Doc\n",
        ));
    }

    #[test]
    fn test_folded_scalar_keeps_inner_quotes_unescaped() {
        // A Claude Code SKILL.md description: several sentences, inner quotes,
        // a colon. Collapsed, it came back as one 300-column line with
        // `\"why won't this TSM string import.\"` escaped inside it.
        let content = concat!(
            "---\n",
            "description: >-\n",
            "  Work with the TradeSkillMaster (TSM) addon: hand-author or debug group\n",
            "  import strings, or read TSM's addon internals. Load when the task\n",
            "  involves TSM groups, or \"why won't this TSM string import.\"\n",
            "---\n",
            "# Skill\n",
        );
        identity(content);
        assert!(
            !serialize(&parse(content)).contains("\\\""),
            "inner quotes must not be escaped"
        );
    }

    #[test]
    fn test_block_scalar_indentation_is_preserved() {
        identity(concat!(
            "---\n",
            "description: >-\n",
            "    four-space indented\n",
            "    continuation\n",
            "---\n",
            "# Doc\n",
        ));
    }

    #[test]
    fn test_block_scalar_followed_by_another_key() {
        identity(concat!(
            "---\n",
            "description: >-\n",
            "  folded across\n",
            "  two lines\n",
            "title: After the block\n",
            "---\n",
            "# Doc\n",
        ));
    }

    #[test]
    fn test_edited_block_scalar_value_is_not_written_verbatim() {
        let mut doc = parse("---\ndescription: >-\n  original text\n---\n# Doc\n");
        let fm = doc.frontmatter.as_mut().unwrap();
        fm.fields.insert(
            "description".to_string(),
            serde_yaml::Value::String("replaced".to_string()),
        );
        let out = serialize(&doc);
        assert!(out.contains("description: replaced\n"), "{out}");
        assert!(!out.contains("original text"), "{out}");
    }

    #[test]
    fn test_long_quoted_value_is_folded_on_the_way_out() {
        let long = "Import a Battle.net parental-controls \"Gameplay report\" PDF into the \
                    project journal: weekly login and signoff telemetry, one row per session.";
        // As `td fmt` used to write it: one line, inner quotes escaped.
        let escaped = long.replace('"', "\\\"");
        let out = roundtrip(&format!("---\ndescription: \"{escaped}\"\n---\n# Doc\n"));
        assert!(out.contains("description: >-\n"), "{out}");
        assert!(
            !out.contains('\\'),
            "a folded scalar needs no escaping:\n{out}"
        );
        for line in out.lines() {
            assert!(line.len() <= YAML_FOLD_WIDTH, "over-long line: {line:?}");
        }
        // The value itself survives the fold.
        let fm = parse(&out).frontmatter.unwrap();
        assert_eq!(
            fm.fields["description"],
            serde_yaml::Value::String(long.into())
        );
    }

    #[test]
    fn test_short_values_keep_their_existing_style() {
        identity("---\ntype: note\ntitle: A short title\n---\n# Doc\n");
        identity("---\ntitle: \"Has: a colon\"\n---\n# Doc\n");
    }

    #[test]
    fn test_long_unquoted_value_is_left_alone() {
        // Nothing to gain from folding a plain scalar that needs no escaping.
        let long = "word ".repeat(30);
        identity(&format!("---\ndescription: {}\n---\n# Doc\n", long.trim()));
    }

    #[test]
    fn test_frontmatter_record_array_keeps_record_boundaries() {
        // Regression: every key used to become its own single-key list entry,
        // dissolving the records with no diagnostic and no way back.
        identity(concat!(
            "---\n",
            "type: journal\n",
            "workouts:\n",
            "  - type: walking\n",
            "    duration_minutes: 36\n",
            "    distance_km: 2.8\n",
            "    calories: 196\n",
            "  - type: running\n",
            "    duration_minutes: 12\n",
            "---\n",
            "# Doc\n",
        ));
    }

    #[test]
    fn test_frontmatter_keeps_authored_key_order() {
        // Frontmatter used to be deserialized via a `Pod` intermediate, which
        // routes through a sorted serde_json::Map and alphabetized every key.
        identity("---\nzebra: 1\napple: 2\nmango: 3\n---\n# Doc\n");
        let fm = parse("---\nzebra: 1\napple: 2\n---\n# Doc\n")
            .frontmatter
            .expect("expected frontmatter");
        assert_eq!(fm.fields.keys().collect::<Vec<_>>(), ["zebra", "apple"]);
    }

    #[test]
    fn test_frontmatter_nested_sequences_roundtrip() {
        // A record holding a list, and a list of lists.
        identity(concat!(
            "---\n",
            "by_app:\n",
            "  - name: Safari\n",
            "    tags:\n",
            "      - web\n",
            "      - reading\n",
            "    nested:\n",
            "      - inner: 1\n",
            "        other: 2\n",
            "pairs:\n",
            "  - - a\n",
            "    - b\n",
            "  - - c\n",
            "---\n",
            "# Doc\n",
        ));
    }

    #[test]
    fn test_frontmatter_empty_collections_in_list_roundtrip() {
        identity("---\nitems:\n  - {}\n  - []\n---\n# Doc\n");
    }

    // ── frontmatter emitter fidelity ──────────────────────────────────────────
    //
    // The emitter is hand-written for style, not for YAML grammar. Every value
    // it writes has to read back as the value it was handed — a scalar that
    // comes back retyped, or a key that comes back missing, is silent data loss,
    // and a block that no longer parses is unrecoverable.

    /// Emit `value` through the hand-written emitter alone, then read it back.
    ///
    /// Deliberately bypasses [`verified_frontmatter`]: the net would rewrite a
    /// bad block with serde_yaml and hide the defect. Going straight at
    /// `serialize_top_field` is what pins the quoting and rendering decisions —
    /// falling back to serde_yaml is correct but costs the file its house style,
    /// so it has to stay reserved for what the emitter genuinely can't write.
    fn emit_and_reparse(value: &serde_yaml::Value) -> serde_yaml::Value {
        let mut body = String::new();
        serialize_top_field(&mut body, "field", value, &Frontmatter::default());
        let parsed: serde_yaml::Value = serde_yaml::from_str(&body)
            .unwrap_or_else(|e| panic!("emitter wrote YAML that will not parse ({e}):\n{body}"));
        let map = parsed
            .as_mapping()
            .unwrap_or_else(|| panic!("emitter wrote a non-mapping:\n{body}"));
        assert_eq!(
            map.keys().next(),
            Some(&serde_yaml::Value::String("field".to_string())),
            "key did not survive:\n{body}"
        );
        map.values()
            .next()
            .cloned()
            .unwrap_or_else(|| panic!("field vanished from:\n{body}"))
    }

    /// Re-emit a frontmatter body through the hand-written emitter and assert it
    /// comes back byte-identical. Bypasses the net, for the reason above.
    fn emitter_identity(frontmatter_body: &str) {
        let source = format!("---\n{frontmatter_body}---\n# Doc\n");
        let fm = parse(&source)
            .frontmatter
            .unwrap_or_else(|| panic!("fixture frontmatter does not parse:\n{source}"));
        let mut body = String::new();
        if let Some(doc_type) = &fm.doc_type {
            serialize_top_field(
                &mut body,
                "type",
                &serde_yaml::Value::String(doc_type.clone()),
                &fm,
            );
        }
        for (key, value) in &fm.fields {
            serialize_top_field(&mut body, key, value, &fm);
        }
        assert_eq!(
            body, frontmatter_body,
            "hand-written emitter did not reproduce the source"
        );
    }

    #[test]
    fn test_tricky_scalars_survive_the_emitter() {
        // Each of these was, or could plausibly become, a plain scalar that
        // YAML reads back as something other than the string we wrote.
        for s in [
            "[draft] Foo",   // flow sequence
            "{a: b}",        // flow mapping
            "- item",        // block sequence entry — a parse error
            "`backtick",     // reserved indicator
            "%directive",    // reserved indicator
            "@reserved",     // reserved indicator
            "?question",     // complex mapping key
            "|literal",      // block scalar header
            ">folded",       // block scalar header
            "0x1F",          // hex integer
            "0o17",          // octal integer
            "-42",           // integer
            "1e3",           // float
            ".inf",          // float
            "-.NaN",         // float
            "12:30",         // sexagesimal in YAML 1.1
            "yes",
            "no",
            "on",
            "off",
            "True",
            "NULL",
            "~",
            "",
            " leading space",
            "trailing space ",
            "has # hash",
            "has: colon",
            "\"quoted\"",
            "it's",
            "multi\nline",
            "tab\there",
            "carriage\rreturn",
            "nel \u{85} here",
            "line separator \u{2028} here",
            "--- document marker",
            "...",
            "*alias",
            "&anchor",
            "!tag",
            "back\\slash",
            "trailing backslash \\",
            "2024-01-01",
            "emoji 🐈 ok",
            // Long enough to take the folding path.
            "A description that runs well past the fold width and so gets written as a block scalar",
        ] {
            let value = serde_yaml::Value::String(s.to_string());
            assert_eq!(emit_and_reparse(&value), value, "scalar did not survive: {s:?}");
        }
    }

    #[test]
    fn test_tricky_scalars_survive_inside_collections() {
        // The quoting decision has to hold wherever a scalar lands: as a list
        // item, and as a mapping key.
        for s in [
            "[draft] Foo",
            "- item",
            "0x1F",
            ".inf",
            "has: colon",
            "%x",
            "",
        ] {
            let scalar = serde_yaml::Value::String(s.to_string());

            let seq = serde_yaml::Value::Sequence(vec![scalar.clone()]);
            assert_eq!(
                emit_and_reparse(&seq),
                seq,
                "list item did not survive: {s:?}"
            );

            let mut map = serde_yaml::Mapping::new();
            map.insert(scalar.clone(), serde_yaml::Value::String("v".to_string()));
            let map = serde_yaml::Value::Mapping(map);
            assert_eq!(
                emit_and_reparse(&map),
                map,
                "mapping key did not survive: {s:?}"
            );
        }
    }

    #[test]
    fn test_tagged_value_is_not_dropped() {
        // `Value::Tagged` used to fall through a `=> {}` arm, deleting the key.
        emitter_identity("key: !Foo bar\n");
        emitter_identity("key: !Foo\n  a: 1\n  b: 2\n");
        emitter_identity("key: !Foo\n  - 1\n  - 2\n");
        emitter_identity("key:\n  - !Foo bar\n  - plain\n");
        emitter_identity("outer:\n  inner: !Foo\n    a: 1\n");
        identity("---\nkey: !Foo bar\n---\n# Doc\n");
    }

    #[test]
    fn test_non_string_mapping_keys_are_not_dropped() {
        // Anything but a string key used to be skipped by the `if let
        // Value::String` guard, so the entry vanished with no diagnostic.
        emitter_identity("years:\n  2024: good\n  2025: better\n");
        emitter_identity("flags:\n  true: yes-branch\n  false: no-branch\n");
        emitter_identity("ratios:\n  1.5: one and a half\n");
        emitter_identity("records:\n  - 2024: good\n    2025: better\n");
        // A nested sequence, where handing the mapping to serde_yaml instead of
        // rendering the key inline would cost the list its indent.
        emitter_identity("years:\n  2024:\n    - spring\n    - summer\n");
        identity("---\nyears:\n  2024: good\n  2025: better\n---\n# Doc\n");
    }

    #[test]
    fn test_complex_mapping_keys_are_not_dropped() {
        // Sequence and mapping keys have no inline form, so serde_yaml's `? key`
        // syntax writes them.
        emitter_identity("pairs:\n  ? - a\n    - b\n  : v\n");
        emitter_identity("items:\n  - ? - a\n      - b\n    : v\n");
        identity("---\npairs:\n  ? - a\n    - b\n  : v\n---\n# Doc\n");
    }

    #[test]
    fn test_empty_nested_mapping_stays_a_mapping() {
        // A bare `meta:` reads back as null, not as `{}`.
        emitter_identity("meta: {}\n");
        emitter_identity("outer:\n  inner: {}\n");
        emitter_identity("items:\n  - meta: {}\n");
        identity("---\nmeta: {}\n---\n# Doc\n");
    }

    #[test]
    fn test_top_level_keys_are_quoted_when_they_need_it() {
        let mut doc = parse("---\nplaceholder: 1\n---\n# Doc\n");
        let fm = doc.frontmatter.as_mut().expect("expected frontmatter");
        fm.fields.clear();
        fm.fields.insert(
            "needs: quoting".to_string(),
            serde_yaml::Value::String("v".to_string()),
        );
        let out = serialize(&doc);
        let reparsed = parse(&out)
            .frontmatter
            .unwrap_or_else(|| panic!("emitted frontmatter no longer parses:\n{out}"));
        assert_eq!(
            reparsed.fields.keys().collect::<Vec<_>>(),
            ["needs: quoting"],
            "key did not survive:\n{out}"
        );
    }

    #[test]
    fn test_verified_frontmatter_falls_back_when_the_body_lies() {
        // The net behind the emitter: if what we wrote doesn't read back as
        // what we meant, serde_yaml writes the block instead.
        let fm = parse("---\ntitle: real\n---\n# Doc\n")
            .frontmatter
            .expect("expected frontmatter");

        // A body that parses but says something else.
        assert_eq!(
            verified_frontmatter("title: bogus\n".to_string(), &fm),
            "title: real\n"
        );
        // A body that doesn't parse at all.
        assert_eq!(
            verified_frontmatter("title: [unclosed\n".to_string(), &fm),
            "title: real\n"
        );
        // A body that is right is left exactly as it was written.
        assert_eq!(
            verified_frontmatter("title:   real\n".to_string(), &fm),
            "title:   real\n"
        );
    }

    // ── round-trip idempotency ─────────────────────────────────────────────────

    #[test]
    fn test_roundtrip_basic() {
        roundtrip("# Title\n\nSome paragraph text.\n\n- Item 1\n- Item 2\n");
    }

    #[test]
    fn test_roundtrip_with_frontmatter() {
        roundtrip(
            "---\ntype: recipe\nservings: 4\ncuisine: italian\n---\n\n# Pasta\n\nSome text.\n",
        );
    }

    #[test]
    fn test_roundtrip_code_block() {
        roundtrip("```bash\necho hello\n```\n");
    }

    #[test]
    fn test_roundtrip_blockquote() {
        roundtrip("> *Go ask Alice* — Jefferson Airplane\n");
    }

    #[test]
    fn test_roundtrip_nested_blockquote() {
        let content = "> Outer quote\n>\n> > Inner quote\n";
        let doc = parse(content);
        let s1 = serialize(&doc);
        assert!(s1.contains("> > "), "nested blockquote must survive:\n{s1}");
        let doc2 = parse(&s1);
        let s2 = serialize(&doc2);
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_roundtrip_thematic_break() {
        roundtrip("Some text\n\n---\n\nMore text\n");
    }

    #[test]
    fn test_roundtrip_bold_italic() {
        let out = roundtrip("This has **bold** and *italic* text.\n");
        assert!(out.contains("**bold**"));
        assert!(out.contains("*italic*"));
    }

    #[test]
    fn test_roundtrip_nested_list() {
        let content =
            "1. **First** - with description:\n   - sub a\n   - sub b\n\n2. **Second**:\n   - sub c\n";
        let out = roundtrip(content);
        assert!(out.contains("**First**"));
        assert!(out.contains("   - sub a"));
        assert!(out.contains("**Second**"));
    }

    #[test]
    fn test_roundtrip_list_item_multiline_unordered() {
        // A bullet whose text wraps via soft-break must preserve continuation indentation.
        // pulldown-cmark strips leading whitespace from continuation lines, so the
        // serializer must re-add it (2 spaces to align after "- ").
        let input = "- Named topic clusters with specific entities (ship names, legislation,\n  geographic chokepoints, people) — not vague categories\n";
        let out = roundtrip(input);
        assert!(
            out.contains("  geographic chokepoints"),
            "continuation line lost indentation:\n{out}"
        );
        // The first line should not have extra indentation
        assert!(
            out.contains("- Named topic clusters"),
            "bullet marker mangled:\n{out}"
        );
    }

    #[test]
    fn test_roundtrip_list_item_multiline_ordered() {
        // An ordered item whose text wraps via soft-break must preserve continuation
        // indentation (3 spaces to align after "1. ").
        let input = "1. **Acknowledge the connection.** \"You've watched a lot of Sal's coverage\n   on this — his channel covers [topic] extensively.\"\n";
        let out = roundtrip(input);
        assert!(
            out.contains("   on this"),
            "continuation line lost indentation:\n{out}"
        );
        assert!(
            out.contains("1. **Acknowledge"),
            "ordered marker mangled:\n{out}"
        );
    }

    #[test]
    fn test_roundtrip_list_item_multiple_soft_breaks() {
        // Multiple continuation lines in one bullet must all be indented.
        let input = "- **Offer to fetch specific videos.** \"Want me to pull\n  the transcript from Sal's explainer on the dark fleet? That\n  one's more evergreen than his daily updates.\"\n";
        let out = roundtrip(input);
        assert!(
            out.contains("  the transcript"),
            "second continuation line lost indentation:\n{out}"
        );
        assert!(
            out.contains("  one's more evergreen"),
            "third continuation line lost indentation:\n{out}"
        );
    }

    #[test]
    fn test_roundtrip_hard_break_backslash_form() {
        // The canonical spelling, so it survives byte-for-byte.
        identity("Line one\\\nLine two\n");
        identity("> quoted\\\n> next\n");
        identity("- item one\\\n  item two\n");
    }

    #[test]
    fn test_roundtrip_hard_break_two_spaces_becomes_backslash() {
        // Two trailing spaces are a valid hard break but invisible, so they get
        // normalized to the backslash form. What must not happen is the break
        // vanishing and the two lines welding into one word.
        for (input, want) in [
            ("Line one  \nLine two\n", "Line one\\\nLine two\n"),
            ("> quoted  \n> next\n", "> quoted\\\n> next\n"),
            ("- item one  \n  item two\n", "- item one\\\n  item two\n"),
        ] {
            assert_eq!(roundtrip(input), want, "hard break lost from {input:?}");
        }
    }

    #[test]
    fn test_roundtrip_hard_break_after_literal_backslash() {
        // `Back\` + hard break: the author's own backslash has to stay escaped
        // or the two collapse into one and the break is read as a literal.
        identity("Back\\\\\\\nSlash\n");
        assert_eq!(
            crate::ast::inlines_to_string(&[
                Inline::Text("Back\\".into()),
                Inline::HardBreak,
                Inline::Text("Slash".into()),
            ]),
            "Back\\ Slash"
        );
    }

    #[test]
    fn test_parse_hard_break_is_not_a_soft_break() {
        let doc = parse("a  \nb\n");
        let Block::Paragraph { content, .. } = &doc.blocks[0] else {
            panic!("expected paragraph, got {:?}", doc.blocks[0]);
        };
        assert!(
            content.contains(&Inline::HardBreak),
            "hard break dropped: {content:?}"
        );
    }

    #[test]
    fn test_roundtrip_tight_item_with_fence_between_paragraphs() {
        // A tight item holding prose / fence / prose. The trailing sentence used
        // to be hoisted above the fence and welded onto the intro, silently
        // merging two sentences into one.
        identity(concat!(
            "- **Item.** Intro sentence that is\n",
            "  hard-wrapped across two lines:\n",
            "  ```bash\n",
            "  ls -la\n",
            "  ```\n",
            "  A following sentence that is also\n",
            "  hard-wrapped across two lines.\n",
        ));
    }

    #[test]
    fn test_roundtrip_loose_ordered_item_with_fence() {
        // A loose ordered item: the blank lines between its blocks must survive,
        // and the wrapped sentence after the fence must keep the item's content
        // indent instead of dropping to column 0 and escaping the item.
        identity(concat!(
            "1. **Sync the offset.** Run the helper:\n",
            "\n",
            "   ```bash\n",
            "   character-sync --check\n",
            "   ```\n",
            "\n",
            "   It re-derives the offset from the files, which is a sentence\n",
            "   long enough to wrap onto a second line.\n",
            "\n",
            "2. **Check the report.** Second item, so the list stays loose.\n",
        ));
    }

    #[test]
    fn test_list_item_children_keep_source_order() {
        let doc = parse(concat!(
            "- Intro:\n",
            "  ```bash\n",
            "  ls\n",
            "  ```\n",
            "  Trailing prose.\n",
        ));
        let Block::List { items, .. } = &doc.blocks[0] else {
            panic!("expected list");
        };
        assert_eq!(serialize_inlines(&items[0].content, false), "Intro:");
        assert!(
            matches!(
                items[0].children.as_slice(),
                [Block::CodeBlock { .. }, Block::Paragraph { .. }]
            ),
            "blocks out of order: {:?}",
            items[0].children
        );
    }

    #[test]
    fn test_roundtrip_item_starting_with_a_fence() {
        // No leading prose to fill the item's inline content, so the fence
        // itself opens on the marker line: leaving the marker bare would make
        // this an empty list item, which can't interrupt a paragraph above it.
        // The fence must still come first, ahead of the paragraph.
        let out = roundtrip(concat!(
            "- ```bash\n",
            "  leading fence\n",
            "  ```\n",
            "\n",
            "  Text after a leading fence.\n",
        ));
        assert_eq!(
            out,
            concat!(
                "- ```bash\n",
                "  leading fence\n",
                "  ```\n",
                "\n",
                "  Text after a leading fence.\n",
            )
        );
    }

    #[test]
    fn test_roundtrip_loose_list_stays_loose() {
        identity("- Loose one\n\n- Loose two\n\n- Loose three\n");
    }

    #[test]
    fn test_roundtrip_tight_list_stays_tight() {
        identity("- Tight one\n- Tight two\n- Tight three\n");
    }

    #[test]
    fn test_roundtrip_tight_list_above_a_loose_list_stays_tight() {
        // `19-technical-data.md` and `20-display-messages-…md` of the Mercedes
        // manual both spell a one-item aside as its own list directly above a
        // loose one. Writing both with the same marker welded them into a
        // single list on the next parse, and the blank lines the loose list
        // needs made the whole thing loose: the tight item's `<li>text</li>`
        // came back `<li><p>text</p></li>`.
        identity("- Tight aside\n\n* Loose a\n\n* Loose b\n");
    }

    #[test]
    fn test_roundtrip_loose_list_above_a_tight_list_stays_loose() {
        identity("- Loose a\n\n- Loose b\n\n* Tight one\n* Tight two\n");
    }

    #[test]
    fn test_roundtrip_item_with_blockquote_and_trailing_prose() {
        identity(concat!(
            "- Item with a quote:\n",
            "\n",
            "  > Quoted line.\n",
            "\n",
            "  And a trailing note.\n",
        ));
    }

    #[test]
    fn test_roundtrip_table() {
        roundtrip("| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |\n");
    }

    #[test]
    fn test_roundtrip_table_alignment() {
        let content = "| Left | Center | Right |\n| :--- | :---: | ---: |\n| a | b | c |\n";
        let doc = parse(content);
        let s1 = serialize(&doc);
        assert!(s1.contains(":---"), "left alignment marker must survive");
        assert!(s1.contains(":---:"), "center alignment marker must survive");
        assert!(s1.contains("---:"), "right alignment marker must survive");
        let doc2 = parse(&s1);
        let s2 = serialize(&doc2);
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_roundtrip_complex() {
        let content = r#"---
type: recipe
servings: 4
cuisine: italian
---

# Pasta

A simple pasta dish.

## Ingredients

- 400g pasta
- 2 cloves garlic
- olive oil

## Instructions

1. Boil water
2. Cook pasta

```bash
echo "done"
```

> Buon appetito!
"#;
        roundtrip(content);
    }

    #[test]
    fn test_roundtrip_inline_code() {
        let out = roundtrip("Use `cargo build` to compile.\n");
        assert!(out.contains("`cargo build`"));
    }

    #[test]
    fn test_roundtrip_code_span_with_backticks() {
        // The concrete case from claude-prompts command-authoring SKILL.md:
        // `` `!`command` `` must survive a roundtrip.
        let input = "Inject output with `` `!`command` `` syntax.\n";
        let out = roundtrip(input);
        assert!(
            out.contains("`` `!`command` ``"),
            "backtick code span mangled:\n{out}"
        );
    }

    #[test]
    fn test_roundtrip_code_span_single_backtick_content() {
        // Code span whose content is just a backtick.
        let input = "The `` ` `` character.\n";
        let out = roundtrip(input);
        assert!(
            out.contains("`` ` ``"),
            "single-backtick code span mangled:\n{out}"
        );
    }

    #[test]
    fn test_identity_code_span_backtick_runs() {
        // Spans holding runs of 1, 2 and 3 backticks. The delimiter has to
        // widen with the content; a fixed width would end the span early.
        identity("A ``` `` ``` pair, a `` ` `` single, a ```` ``` ```` triple.\n");
    }

    #[test]
    fn test_identity_code_span_edge_spaces() {
        // A reader strips one space from each end of a padded span, so
        // content with spaces on both ends has to be padded to survive.
        identity("The `  x  ` token.\n");
    }

    #[test]
    fn test_identity_tsm_group_string() {
        // A README documenting an addon's group-string syntax, where
        // doubled backticks are the subject matter rather than markup.
        identity("A literal comma in a name is ``` `` ```.\n");
        identity("Tokens look like `` `group:Mats`Cloth`01. Classic,i:2589` ``.\n");
    }

    #[test]
    fn test_code_span_delimiters_survive_every_content() {
        // The serializer's own wrapper must parse back to exactly what it
        // wrapped, whatever the content — backticks and edge spaces included.
        let bodies = [
            "x", "`", "``", "```", "`x", "x`", "`x`", "``x``", "a`b", "a``b", " x ", " x", "x ",
            "  x  ", "`` x ``", "a ``` b", "` `", "  ",
        ];
        for body in bodies {
            let span = crate::ast::format_code_span(body);
            let doc = parse(&format!("pre {span} post\n"));
            let Block::Paragraph { content, .. } = &doc.blocks[0] else {
                panic!("expected paragraph for {span:?}, got {:?}", doc.blocks[0]);
            };
            let Some(Inline::Code(parsed)) = content.get(1) else {
                panic!("{span:?} did not parse back to a code span: {content:?}");
            };
            assert_eq!(
                parsed.text, body,
                "{span:?} did not parse back to a code span holding {body:?}"
            );
        }
    }

    #[test]
    fn test_identity_fence_wrapping_nested_example() {
        // A markdown example that itself contains a fenced block: the outer
        // fence must be wider, or it terminates at the inner ``` and the rest
        // of the block spills into the document as prose.
        identity("````markdown\n# Example\n\n```bash\nls\n```\n\nstill outer\n````\n");
    }

    #[test]
    fn test_identity_fence_widths_scale_with_content() {
        identity("```text\nno backticks\n```\n");
        identity("````text\nholds ```\n````\n");
        identity("`````text\nholds ````\n`````\n");
    }

    #[test]
    fn test_identity_ambiguous_double_backtick_span() {
        // `` `` `` is not a span holding a backtick pair: per CommonMark the
        // opening run closes at the next run of the same width, so this is a
        // span holding a space, followed by two literal backticks. Nothing can
        // recover the intent. This used to be re-spelled with the narrowest
        // delimiter that fits a single space, `` ` `` — stable for one span in
        // isolation, which is all the old assertion covered, but the width is
        // what a *later* literal run pairs against, so on a line carrying more
        // of either the narrowing re-cut every span (see the test below).
        // Keeping the authored width sidesteps the question entirely.
        identity("names can't contain `` `` ``.\n");
    }

    #[test]
    fn test_identity_double_backtick_spans_among_literal_backticks() {
        // A skill file documenting a crafting addon's group-string syntax.
        // Narrowing the first span left its trailing `` behind as a loose
        // literal run for the next span to pair with, so the prose between
        // the spans was swallowed *into* them: real, visible data loss.
        identity(
            "A literal comma is `` `` `` (double). \
             Names can't contain `` ` `` or `` `` ``.\n",
        );
    }

    #[test]
    fn test_identity_code_span_padding_is_kept() {
        // The padding CommonMark strips is not part of the span's content, so a
        // serializer working from content alone never puts it back and the span
        // ends up welded to its neighbours.
        // Found in a journal entry quoting a padded code span from a
        // failing test's output.
        identity("A padded ` foo ` span.\n");
        identity("the ` line, the failing-test ` marker\n");
    }

    #[test]
    fn test_identity_table_cell_escaped_pipe() {
        // A journal entry's income-tracking table, whose cell held unescaped
        // pipes in a formula string. GFM splits a row into cells on
        // unescaped pipes *before* inline parsing, so a cell that
        // loses its backslashes grows extra columns and everything past the
        // first pipe is dropped from the rendered table. A code span is no
        // shelter: the split happens before it exists.
        identity("| a | b |\n| --- | --- |\n| x | y \\| z |\n");
        identity(
            "| Metric | Source |\n| --- | --- |\n\
             | Income | sum from `desc:Dividend\\|Distribution\\|Interest` then reconciled |\n",
        );
    }

    #[test]
    fn test_table_cell_pipe_escape_keeps_the_column_count() {
        // The escape is what holds the row together: re-parsing must find the
        // same two columns, not four.
        let out = roundtrip("| a | b |\n| --- | --- |\n| x | `p\\|q\\|r` |\n");
        let Block::Table { rows, .. } = &parse(&out).blocks[0] else {
            panic!("expected table:\n{out}");
        };
        assert_eq!(rows[0].len(), 2, "row split on an unescaped pipe:\n{out}");
    }

    #[test]
    fn test_identity_table_inside_a_list_item() {
        // A table indented into an item is one of the item's child blocks.
        // With no `Table` arm in the item's event loop the whole table fell
        // through to the inline arms and came back as its cell texts run
        // together — `- item` followed by `ab12`.
        identity("- item intro\n\n  | a | b |\n  | --- | --- |\n  | 1 | 2 |\n");
    }

    #[test]
    fn test_identity_heading_inside_a_list_item() {
        // With no `Heading` arm in the item's event loop the `#`s were dropped
        // and the text fell through to the inline arms: the loose spelling came
        // back demoted to a paragraph, and the tight one welded the heading's
        // text onto the item's own — `- item\n  ## sub` became `- itemsub`.
        identity("- item\n\n  ## sub\n");
        identity("- item\n  ## sub\n");
        identity("- a\n\n  ### h\n\n  more\n");
        // Nothing precedes it, so the heading rides the marker line.
        identity("- ## h\n- b\n");
    }

    #[test]
    fn test_setext_heading_in_an_item_becomes_atx() {
        // Same normalization the serializer applies at the top level: the AST
        // records a heading's level, not which spelling produced it.
        assert_eq!(
            roundtrip("- item\n\n  sub\n  ---\n"),
            "- item\n\n  ## sub\n"
        );
    }

    #[test]
    fn test_identity_thematic_break_inside_a_list_item() {
        identity("- item\n\n  ---\n\n- next\n");
        identity("- a\n  - b\n\n    ---\n");
        // An item that is *only* a break has nothing to precede it, so it rides
        // the marker line — spelled `***`, because `- ---` is four dashes
        // separated by spaces and would parse back as a top-level break.
        identity("- a\n- ***\n- b\n");
        identity("- ***\n");
        identity("1. a\n2. ***\n3. b\n");
    }

    #[test]
    fn test_dash_run_after_a_marker_is_a_thematic_break_not_an_item() {
        // `- ---` is a run of four dashes separated by spaces, and a thematic
        // break takes precedence over a list marker, so this really is
        // list / break / list rather than a break nested in the middle item.
        // The output says so out loud instead of round-tripping the input.
        assert_eq!(roundtrip("- a\n- ---\n- b\n"), "- a\n\n---\n\n- b\n");
    }

    #[test]
    fn test_lazy_setext_underline_stays_prose() {
        // A setext underline cannot be a lazy continuation line, so the
        // `======` is simply more of the item's paragraph. Re-indenting it to
        // the item's content column would promote it to a real underline —
        // making the item's prose a heading, which then vanished along with
        // the line. Escaping the run pins it as prose instead.
        let out = roundtrip("- b\n======\n");
        assert_eq!(out, "- b\n  \\======\n");
        let Some(Block::List { items, .. }) = parse(&out).blocks.first().cloned() else {
            panic!("expected a list:\n{out}");
        };
        assert_eq!(crate::ast::inlines_to_string(&items[0].content), "b ======");
    }

    #[test]
    fn test_identity_lazy_table_rows_stay_outside_the_item() {
        // A journal entry's bullet list followed by an unindented table.
        // Unindented table rows after a bullet are a lazy continuation of its
        // paragraph — prose, not a table, because the table extension doesn't
        // run on lazy lines. Indenting them to the item's content column
        // promotes them to a real table and swallows it into the bullet,
        // which changes the rendering.
        identity("- last bullet text\n| Test | Direct |\n| --- | --- |\n| a | b |\n");
        // Prose wrapped in among them is anchored at column 0 too, or the run
        // comes back split across two indentations.
        identity("- bullet one\n**Lazy prose**\n| A | B |\n| --- | --- |\n| 1 | 2 |\n");
    }

    #[test]
    fn test_lazy_table_rows_in_a_blockquote_get_escaped() {
        // The same lazy continuation one container over, found by
        // `test_generated_documents_round_trip`. A blockquote can't anchor the
        // run at column 0 the way a list item does — every line takes a `> ` —
        // so on the next parse the rows were no longer lazy and the table
        // extension ran, eating the paragraph's tail into a table. The
        // delimiter row is escaped instead, which is what keeps it prose.
        let out = roundtrip("> quoted\n| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        assert_eq!(
            out,
            "> quoted\n> | a | b |\n> \\| --- | --- |\n> | 1 | 2 |\n"
        );
        // The pipe-less delimiter spelling opens a table just the same.
        roundtrip("> quoted\n| a | b |\n--- | ---\n");
        // Outside a blockquote nothing is escaped: the line stays lazy.
        identity("- bullet\n| a | b |\n| --- | --- |\n");
        identity("para\n\n| a | b |\n| --- | --- |\n");
    }

    #[test]
    fn test_thematic_break_under_a_paragraph_in_an_item() {
        // Found by `test_generated_documents_round_trip`. `---` on the line
        // below a paragraph is a setext underline, and inside a list item
        // there is no blank line to separate them — inserting one would make
        // the list loose. The break is spelled `***` there instead; before
        // this, the item came back as `- ## text`.
        assert_eq!(roundtrip("- a\n  ***\n"), "- a\n  ***\n");
        assert_eq!(roundtrip("- a\n\n  b\n\n  ---\n"), "- a\n\n  b\n\n  ---\n");
    }

    #[test]
    fn test_empty_item_marker_cannot_interrupt_a_paragraph() {
        // Found by `test_generated_documents_round_trip`. An empty list item
        // may not interrupt a paragraph, so a bare `-` under one is read as
        // more of that paragraph — and CommonMark reads a lone `-` there as a
        // setext underline besides. The item's first block opens on the marker
        // line, and a list that still can't avoid a bare marker takes `*`.
        identity("- b\n  - | h |\n    | --- |\n    | c |\n");
        identity("- ```\n  code\n  ```\n");
        // The respelling can't collide with the list below it: a paragraph
        // sits between them, so that one starts a fresh alternation run.
        roundtrip("- a\n  - | h |\n    | --- |\n  - x\n  - y\n");
    }

    #[test]
    fn test_blank_line_inside_a_blockquote_is_a_gap() {
        // Found by `test_generated_documents_round_trip`. The gap between two
        // blocks in a blockquote is spelled `>`, which is not an empty line;
        // read literally it wasn't a gap at all, and an item's second
        // paragraph came back welded onto its first as `> - a\n>   para`.
        identity("> - a\n>\n>   para\n");
        identity("> 1. a\n>\n>    para\n");
        identity("> - a\n>\n> - b\n");
    }

    #[test]
    fn test_indented_html_block_loses_its_indent() {
        // Found by `test_generated_documents_round_trip`. A top-level HTML
        // block keeps whatever indent it was written with, and re-emitting it
        // under a list put it inside the last item on the next parse.
        let out = roundtrip("- a\n\n  <div>\n  raw\n  </div>\n");
        assert_eq!(out, "- a\n\n  <div>\n  raw\n  </div>\n");
        assert_eq!(
            roundtrip("  <div>\n  raw\n  </div>\n"),
            "<div>\nraw\n</div>\n"
        );
        // Indentation *within* the block is content and stays.
        identity("<div>\n  <p>x</p>\n</div>\n");
    }

    #[test]
    fn test_identity_blank_line_before_a_list_item_continuation() {
        // An instruction file with a nested list followed by trailing prose
        // in the same item. The gap between a nested list and the paragraph
        // that follows it in the same item: the nested list's range runs
        // past the blank line, so measured from the front the gap is
        // invisible. Lose it and the paragraph comes back as a lazy
        // continuation of the nested list's last item, a level deeper each
        // pass.
        identity("- outer intro\n  - nested a\n  - nested b\n\n  trailing prose\n- next\n");
    }

    #[test]
    fn test_list_item_continuation_indent_converges() {
        // `td fmt` has to reach a fixed point. This shape used to oscillate
        // forever: the continuation paragraph came out at the outer item's
        // indent on one pass and the nested item's on the next.
        let src = "- **One concept.** A task is either:\n  1. a file, or\n  \
                   2. a branch push.\n\n  Frontmatter notes follow.\n";
        let pass1 = serialize(&parse(src));
        let pass2 = serialize(&parse(&pass1));
        assert_eq!(pass1, pass2, "second pass must be a no-op:\n{pass1}");
        assert_eq!(pass1, src, "and the first should not have moved anything");
    }

    #[test]
    fn test_lazy_table_rows_converge() {
        // `td fmt` has to reach a fixed point, and this shape used not to: the
        // indented rows parsed as a real table on the next pass, which then
        // serialized as the item's cell texts run together. Assert the two
        // passes explicitly rather than leaning on `roundtrip`'s check.
        let src = "- bullet one\n**Lazy prose**\n| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let pass1 = serialize(&parse(src));
        let pass2 = serialize(&parse(&pass1));
        assert_eq!(pass1, pass2, "second pass must be a no-op:\n{pass1}");
        assert_eq!(pass1, src, "and the first should not have moved anything");
    }

    #[test]
    fn test_roundtrip_code_span_across_line_break() {
        // A code span straddling a hard wrap used to collapse the paragraph
        // onto one line: the parser turns the span's newline into a space and
        // nothing put it back. Authored wraps are preserved here like anywhere
        // else in a paragraph.
        identity(
            "This is a wrapped paragraph where an inline code span like `blob\n\
             clean` straddles the line break, plus more words to push it long.\n",
        );
    }

    #[test]
    fn test_roundtrip_code_span_break_among_other_breaks() {
        // The break inside the span is handled the same as its neighbours.
        identity(
            "Alpha beta gamma and here is a span like `blob\n\
             clean` that straddles\n\
             the break, plus more\n\
             words to push it long.\n",
        );
    }

    #[test]
    fn test_roundtrip_code_span_across_line_break_in_list() {
        // The block indent is not part of the span content: it is stripped on
        // parse and re-added on serialize, exactly as for a soft break.
        identity("- foo `blob\n  clean` bar\n");
        identity("1. foo `blob\n   clean` bar\n");
    }

    #[test]
    fn test_roundtrip_code_span_across_line_break_in_blockquote() {
        identity("> quoted `blob\n> clean` tail\n");
    }

    #[test]
    fn test_code_span_break_is_content_agnostic() {
        // Only line endings become breaks -- literal spaces stay spaces.
        let doc = parse("a `blob\nclean  x` b\n");
        let Block::Paragraph { content, .. } = &doc.blocks[0] else {
            panic!("expected paragraph");
        };
        assert!(
            content
                .iter()
                .any(|i| matches!(i, Inline::Code(span) if span.text == "blob\nclean  x")),
            "unexpected inlines: {content:?}"
        );
        // Plain-text rendering still collapses the break to a space.
        assert_eq!(crate::ast::inlines_to_string(content), "a blob clean  x b");
    }

    #[test]
    fn test_roundtrip_code_span_across_line_break_with_padding() {
        // Content that begins and ends with a space keeps its padding stripped;
        // the break in the middle still survives.
        identity("The `` ` foo\nbar ` `` span.\n");
    }

    #[test]
    fn test_roundtrip_link_in_list() {
        roundtrip("- See [the docs](https://example.com) for details\n");
    }

    #[test]
    fn test_blankline_line_is_zero() {
        let doc = parse("# Title\nText.\n");
        let blank = doc
            .blocks
            .iter()
            .find(|b| matches!(b, Block::BlankLine))
            .unwrap();
        assert_eq!(blank.line(), 0);
    }

    // ── Tilde / strikethrough regression ──────────────────────────────────────

    #[test]
    fn test_tilde_not_corrupted() {
        // ~$5 must not be converted to ~~$5 (strikethrough opener)
        let out = roundtrip("Costs approximately ~$5 per month.\n");
        assert!(out.contains("~$5"), "tilde corrupted in output: {out}");
        assert!(
            !out.contains("~~$5"),
            "tilde doubled into strikethrough: {out}"
        );
    }

    #[test]
    fn test_strikethrough_literal_roundtrip() {
        // ~~deleted~~ should survive as plain text (parsing disabled)
        let input = "Text with ~~deleted~~ content.\n";
        let out = roundtrip(input);
        assert!(
            out.contains("~~deleted~~"),
            "strikethrough literal lost: {out}"
        );
    }

    // ── Image roundtrip ───────────────────────────────────────────────────────

    #[test]
    fn test_roundtrip_image() {
        let out = roundtrip("![A diagram](diagram.png)\n");
        assert!(out.contains("![A diagram](diagram.png)"), "got: {out}");
    }

    #[test]
    fn test_roundtrip_image_in_paragraph() {
        let out = roundtrip("See the ![architecture diagram](arch.png) above.\n");
        assert!(
            out.contains("![architecture diagram](arch.png)"),
            "got: {out}"
        );
        assert!(out.contains("See the"), "surrounding text lost: {out}");
    }

    // ── Table inline formatting ───────────────────────────────────────────────

    #[test]
    fn test_roundtrip_table_inline_formatting() {
        let input =
            "| Feature | Status |\n| --- | --- |\n| **Bold** | `code` |\n| [Link](url) | plain |\n";
        let out = roundtrip(input);
        assert!(out.contains("**Bold**"), "bold lost in table: {out}");
        assert!(out.contains("`code`"), "inline code lost in table: {out}");
        assert!(out.contains("[Link](url)"), "link lost in table: {out}");
    }

    // ── Block boundary interactions ───────────────────────────────────────────

    #[test]
    fn test_roundtrip_blockquote_after_paragraph() {
        // Blockquote following a paragraph must keep both the paragraph and blockquote marker
        let out = roundtrip("Some context.\n\n> *Important insight.*\n");
        assert!(out.contains("Some context."), "paragraph lost: {out}");
        assert!(
            out.contains("> *Important insight.*"),
            "blockquote lost: {out}"
        );
    }

    #[test]
    fn test_roundtrip_thematic_break_before_italic() {
        // Thematic break separating a link from italic text
        let out = roundtrip("[Prev](a.md)\n\n---\n\n*Next section*\n");
        assert!(out.contains("[Prev](a.md)"), "link lost: {out}");
        assert!(out.contains("---"), "thematic break lost: {out}");
        assert!(out.contains("*Next section*"), "italic lost: {out}");
    }

    #[test]
    fn test_roundtrip_consecutive_paragraphs() {
        // Consecutive paragraphs must stay separate — a blank line between them
        // is required by CommonMark. Without it they merge into one paragraph
        // with soft breaks, which is a structural change.
        let input = "**Arguments** — first.\n\n**Shell output** — second.\n\n**File references** — third.\n";
        let out = roundtrip(input);

        // All three paragraphs must survive as separate lines with a blank line between each.
        assert!(
            out.contains("**Arguments** — first.\n\n**Shell output** — second."),
            "paragraphs merged:\n{out}"
        );

        // Verify the AST has three separate Paragraph blocks.
        let doc = parse(&out);
        let para_count = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Paragraph { .. }))
            .count();
        assert_eq!(para_count, 3, "expected 3 paragraphs, got {para_count}");
    }

    // ── Link text with emphasis-like characters ───────────────────────────────

    #[test]
    fn test_roundtrip_link_with_asterisks_in_text() {
        // M*A*S*H contains asterisks that pulldown-cmark parses as emphasis.
        // The formatter must preserve them in the serialized link text.
        let input = "- [M*A*S*H](../tvshows/MASH/README.md) S01E03\n";
        let out = roundtrip(input);
        assert!(
            out.contains("[M*A*S*H]"),
            "asterisks in link text lost:\n{out}"
        );
    }

    #[test]
    fn test_roundtrip_link_with_bold_in_text() {
        // Bold inside link text must survive roundtrip.
        let input = "See [the **important** docs](https://example.com) for details.\n";
        let out = roundtrip(input);
        assert!(
            out.contains("[the **important** docs]"),
            "bold in link text lost:\n{out}"
        );
    }

    #[test]
    fn test_roundtrip_image_with_emphasis_in_alt() {
        // Emphasis inside image alt text must survive roundtrip.
        let input = "![A *very* important diagram](diagram.png)\n";
        let out = roundtrip(input);
        assert!(
            out.contains("![A *very* important diagram]"),
            "emphasis in alt text lost:\n{out}"
        );
    }

    // ── raw HTML and autolinks ────────────────────────────────────────────────

    #[test]
    fn test_roundtrip_inline_html_placeholder() {
        // The original repro: an angle-bracket placeholder in prose used to be
        // dropped outright, taking the surrounding backtick parse with it.
        identity("the `Mats` <Cat> parent\n");
    }

    #[test]
    fn test_parse_inline_html_produces_html_inline() {
        let doc = parse("a <br/> b\n");
        match &doc.blocks[0] {
            Block::Paragraph { content, .. } => assert_eq!(
                content,
                &vec![
                    Inline::Text("a ".to_string()),
                    Inline::Html("<br/>".to_string()),
                    Inline::Text(" b".to_string()),
                ]
            ),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn test_roundtrip_inline_html_void_tag() {
        identity("a <br/> b\n");
    }

    #[test]
    fn test_roundtrip_inline_html_paired_tags() {
        identity("wrapping <span class=\"x\">some words</span> here\n");
    }

    #[test]
    fn test_roundtrip_inline_html_in_heading_and_list() {
        identity("# Types like <T>\n\n- generic <K, V> pair\n- a <b>bold</b> item\n");
    }

    #[test]
    fn test_roundtrip_inline_html_in_table_cell() {
        identity("| a | b |\n| --- | --- |\n| <x> | y |\n");
    }

    #[test]
    fn test_roundtrip_html_block() {
        identity("<div align=\"center\">\n  <img src=\"logo.png\">\n</div>\n\nAfter the block.\n");
    }

    #[test]
    fn test_parse_html_block_produces_html_block() {
        let doc = parse("<div>\nhi\n</div>\n");
        assert_eq!(
            doc.blocks,
            vec![Block::Html {
                // The block keeps the newline that ends its last line, the same
                // as an HTML block anywhere else in the document.
                content: "<div>\nhi\n</div>\n".to_string(),
                line: 1,
            }]
        );
    }

    #[test]
    fn test_roundtrip_html_comment_block() {
        identity("<!-- prettier-ignore -->\n\nSome text.\n");
    }

    #[test]
    fn test_html_block_gets_the_same_line_number_as_any_other_block() {
        // An HTML block must take part in offset tracking; if it didn't, every
        // block after it would inherit the wrong line.
        let doc = parse("---\ntype: note\n---\n\n<div>\nhi\n</div>\n\n# Title\n");
        let html_line = doc.blocks[0].line();
        let heading_line = doc
            .blocks
            .iter()
            .find(|b| matches!(b, Block::Heading { .. }))
            .unwrap()
            .line();
        assert_eq!(
            heading_line - html_line,
            4,
            "html block spans 3 lines + blank"
        );

        let same_shape = parse("---\ntype: note\n---\n\n> quote\n> more\n> end\n\n# Title\n");
        assert_eq!(html_line, same_shape.blocks[0].line());
    }

    #[test]
    fn test_roundtrip_autolink_url() {
        identity("See <https://example.com> for docs.\n");
    }

    #[test]
    fn test_roundtrip_autolink_email() {
        identity("Mail <foo@example.com> now.\n");
    }

    #[test]
    fn test_autolink_still_counts_as_a_link() {
        // Link checking must keep seeing autolink destinations.
        let doc = parse("See <https://example.com> now.\n");
        match &doc.blocks[0] {
            Block::Paragraph { content, .. } => assert!(
                content.contains(&Inline::Autolink("https://example.com".to_string())),
                "expected an autolink inline, got {content:?}"
            ),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    #[test]
    fn test_inline_link_with_url_text_stays_a_link() {
        // Written as an explicit link, not an autolink — keep the bracket form.
        identity("See [https://example.com](https://example.com) now.\n");
    }

    // ── link and image titles ─────────────────────────────────────────────────

    #[test]
    fn test_roundtrip_link_title() {
        identity("See [x](/url \"Tooltip\") now.\n");
    }

    #[test]
    fn test_roundtrip_image_title() {
        identity("![a](/i.png \"t\")\n");
    }

    #[test]
    fn test_roundtrip_titles_in_nested_positions() {
        identity("- [x](/url \"t\") item\n- ![a](/i.png \"pic\") item\n");
        identity("> [x](/url \"t\")\n");
        identity("| a | b |\n| --- | --- |\n| [x](/url \"t\") | y |\n");
        identity("# Heading with [x](/url \"t\")\n");
    }

    #[test]
    fn test_link_title_lands_in_the_ast() {
        let doc = parse("[x](/url \"Tooltip\")\n");
        let Block::Paragraph { content, .. } = &doc.blocks[0] else {
            panic!("expected paragraph, got {:?}", doc.blocks[0]);
        };
        assert_eq!(
            content[0],
            Inline::Link {
                content: vec![Inline::Text("x".to_string())],
                url: "/url".to_string(),
                title: Some("Tooltip".to_string()),
            }
        );
    }

    #[test]
    fn test_untitled_link_stays_untitled() {
        identity("[x](/url)\n");
        let doc = parse("[x](/url)\n");
        let Block::Paragraph { content, .. } = &doc.blocks[0] else {
            panic!("expected paragraph, got {:?}", doc.blocks[0]);
        };
        assert!(matches!(&content[0], Inline::Link { title: None, .. }));
    }

    #[test]
    fn test_roundtrip_title_containing_quotes_and_backslashes() {
        // Re-quoted with double quotes, so the inner ones need escaping —
        // and the escaped form must survive a second pass unchanged.
        identity("[x](/url \"a \\\" b\")\n");
        identity("[x](/url \"back \\\\ slash\")\n");
    }

    #[test]
    fn test_title_quote_style_normalizes_and_settles() {
        // Single quotes and parens are re-quoted with double quotes — a change,
        // but a stable one.
        assert_eq!(roundtrip("[x](/url 't')\n"), "[x](/url \"t\")\n");
        assert_eq!(roundtrip("[x](/url (t))\n"), "[x](/url \"t\")\n");
        identity("[x](/url \"t\")\n");
    }

    #[test]
    fn test_empty_destination_with_title_keeps_its_brackets() {
        // Without `<>` the title would be re-read as the destination.
        identity("[x](<> \"t\")\n");
    }

    // ── ordered list start numbers ────────────────────────────────────────────

    #[test]
    fn test_roundtrip_ordered_list_start() {
        identity("5. five\n6. six\n");
    }

    #[test]
    fn test_ordered_list_start_lands_in_the_ast() {
        let doc = parse("5. five\n6. six\n");
        let Block::List {
            ordered,
            start,
            items,
            ..
        } = &doc.blocks[0]
        else {
            panic!("expected list, got {:?}", doc.blocks[0]);
        };
        assert!(ordered);
        assert_eq!(*start, 5);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_ordered_list_renumbers_from_its_start() {
        // Numbering within the list is normalized; only the start survives.
        assert_eq!(
            roundtrip("5. five\n5. six\n5. seven\n"),
            "5. five\n6. six\n7. seven\n"
        );
        assert_eq!(roundtrip("1. one\n1. two\n"), "1. one\n2. two\n");
        // ...and the normalized form is a fixed point.
        identity("5. five\n6. six\n7. seven\n");
    }

    #[test]
    fn test_ordered_list_zero_start() {
        identity("0. zero\n1. one\n");
    }

    #[test]
    fn test_nested_ordered_list_keeps_its_own_start() {
        identity("3. three\n   1. inner one\n   2. inner two\n4. four\n");
    }

    #[test]
    fn test_unordered_list_unaffected_by_start() {
        identity("- a\n- b\n");
        let doc = parse("- a\n- b\n");
        let Block::List { ordered, start, .. } = &doc.blocks[0] else {
            panic!("expected list, got {:?}", doc.blocks[0]);
        };
        assert!(!ordered);
        assert_eq!(*start, 1);
    }

    #[test]
    fn test_ordered_list_start_widens_the_continuation_indent() {
        // "9. " is 3 wide, "10. " is 4 — wrapped lines follow the marker.
        identity("9. nine\n10. ten\n11. eleven\n");
    }

    // ── escapes and entities in text ──────────────────────────────────────────

    /// Assert the serialized form re-parses to the same AST.
    ///
    /// Weaker than [`identity`], for text the serializer is free to respell:
    /// `&#42;` and `\*` are the same character, so only the meaning survives.
    fn preserves(input: &str) {
        let out = roundtrip(input);
        assert_eq!(
            merged_blocks(&out),
            merged_blocks(input),
            "re-parse differs\n--- in\n{input}--- out\n{out}"
        );
    }

    /// Blocks with adjacent text nodes collapsed, so a respelled escape does
    /// not read as a difference.
    fn merged_blocks(md: &str) -> Vec<Block> {
        parse(md)
            .blocks
            .into_iter()
            .map(|block| match block {
                Block::Paragraph { content, line } => Block::Paragraph {
                    content: merge_text(&content),
                    line,
                },
                Block::Heading {
                    level,
                    content,
                    line,
                } => Block::Heading {
                    level,
                    content: merge_text(&content),
                    line,
                },
                other => other,
            })
            .collect()
    }

    #[test]
    fn test_identity_backslash_escapes_survive() {
        // Parsing resolves escapes into plain Text, so a serializer that emits
        // Text verbatim turns every one of these into live markup.
        identity("\\*not em\\*\n");
        identity("\\_not em\\_\n");
        identity("\\# not heading\n");
        identity("\\- x\n");
        identity("\\> x\n");
        identity("\\`x\\`\n");
        identity("1\\. not a list\n");
        identity("\\---\n");
        identity("a\n\\===\n");
        identity("\\<div>\n");
    }

    #[test]
    fn test_html_entities_come_back_as_escapes() {
        // An entity decodes to the same character an escape does; either
        // spelling is fine as long as it stays inert.
        preserves("&#42;emphasis&#42;\n");
        preserves("&lt;div&gt;\n");
        preserves("&#35; not a heading\n");
    }

    #[test]
    fn test_identity_escaped_pipe_in_table_cell() {
        // Unescaped, the pipe splits the cell and the last column falls off
        // the row on the following pass.
        identity("| a \\| b | c |\n| --- | --- |\n| d | e |\n");
    }

    #[test]
    fn test_identity_link_destination_with_a_space() {
        identity("[y](</url with space>)\n");
        identity("[y](/a\\)b)\n");
    }

    #[test]
    fn test_clean_prose_is_not_over_escaped() {
        // The escaping pass must not tax documents that never needed it.
        identity("2 * 3 = 6 and 5 - 3 = 2\n");
        identity("snake_case_name and dunder_style_names\n");
        identity("a < b and c > d, R&D at AT&T\n");
        identity("paths like C:\\path\\to and a\\b\n");
        identity("an [aside] in brackets, call foo(bar)\n");
        identity("# Heading with a # inside\n");
        identity("| a | b |\n| --- | --- |\n| c | d |\n");
    }

    #[test]
    fn test_identity_heading_closing_sequence() {
        identity("# trailing hash \\#\n");
    }

    // ── thematic break at the start of a document ─────────────────────────────
    //
    // `---` on line 1 is a frontmatter opener, so a thematic break that lands
    // there is read back as a delimiter and lost. The serializer writes `***`
    // for a document-leading break, and the parser only strips frontmatter it
    // can see both delimiters of.

    #[test]
    fn test_identity_leading_thematic_break() {
        identity("***\n\n# Hi\n");
    }

    #[test]
    fn test_roundtrip_leading_thematic_break_dashes() {
        let out = roundtrip("---\n\n# Hi\n");
        assert_eq!(out, "***\n\n# Hi\n", "leading thematic break lost: {out:?}");
        assert!(
            parse(&out).frontmatter.is_none(),
            "read back as frontmatter"
        );
    }

    #[test]
    fn test_roundtrip_lone_thematic_break() {
        let out = roundtrip("---\n");
        assert_eq!(out, "***\n", "document reduced to nothing: {out:?}");
    }

    #[test]
    fn test_roundtrip_leading_thematic_break_after_frontmatter() {
        let out = roundtrip("---\ntype: note\n---\n\n---\n\n# Hi\n");
        let doc = parse(&out);
        assert_eq!(
            doc.frontmatter
                .as_ref()
                .and_then(|fm| fm.doc_type.as_deref()),
            Some("note"),
            "frontmatter lost: {out:?}"
        );
        assert!(
            matches!(doc.blocks.first(), Some(Block::ThematicBreak { .. })),
            "thematic break lost: {out:?}"
        );
        assert!(out.contains("***"), "break spelled ambiguously: {out:?}");
    }

    #[test]
    fn test_unterminated_frontmatter_keeps_its_dashes() {
        // No closing `---`, so there is no frontmatter — the opener is a
        // thematic break and the rest is prose. Neither may be swallowed.
        let out = roundtrip("---\ntype: note\n# Doc\n");
        let doc = parse(&out);
        assert!(doc.frontmatter.is_none(), "phantom frontmatter: {out:?}");
        assert!(
            matches!(doc.blocks.first(), Some(Block::ThematicBreak { .. })),
            "thematic break lost: {out:?}"
        );
        assert!(out.contains("type: note"), "prose lost: {out:?}");
        assert!(out.contains("# Doc"), "heading lost: {out:?}");
    }

    #[test]
    fn test_identity_blockquote_leading_thematic_break() {
        // Only a break at the very start of the *file* is ambiguous; one behind
        // a blockquote marker keeps the spelling the author used.
        identity("> ---\n");
    }

    // ── adjacent lists ────────────────────────────────────────────────────────

    #[test]
    fn test_identity_adjacent_lists_keep_their_own_markers() {
        // Two lists in a row merge into one if they are written the same way,
        // which changes the paragraph wrapping the renderer applies. The
        // alternate marker is what keeps them two.
        identity("- a\n- b\n\n* c\n* d\n");
    }

    #[test]
    fn test_adjacent_lists_do_not_merge_on_reparse() {
        let out = roundtrip("- a\n- b\n\n* c\n* d\n");
        let blocks: Vec<_> = parse(&out)
            .blocks
            .into_iter()
            .filter(|b| matches!(b, Block::List { .. }))
            .collect();
        assert_eq!(blocks.len(), 2, "lists merged: {out:?}");
    }

    #[test]
    fn test_three_adjacent_lists_alternate_back() {
        // The marker only has to differ from its neighbour, so a third list
        // returns to the house style rather than reaching for a third spelling.
        let out = roundtrip("- a\n\n* b\n\n+ c\n");
        assert_eq!(out, "- a\n\n* b\n\n- c\n");
        assert_eq!(
            parse(&out)
                .blocks
                .iter()
                .filter(|b| matches!(b, Block::List { .. }))
                .count(),
            3,
            "lists merged: {out:?}"
        );
    }

    #[test]
    fn test_adjacent_ordered_lists_stay_apart() {
        // Numbered lists merge the same way, and `1)` is the alternate spelling.
        let out = roundtrip("1. a\n\n1) b\n");
        assert_eq!(out, "1. a\n\n1) b\n");
    }

    #[test]
    fn test_adjacent_lists_of_different_kinds_need_no_alternate() {
        // A bullet list and a numbered one are already two lists.
        identity("- a\n\n1. b\n");
    }

    #[test]
    fn test_nested_adjacent_lists_keep_their_own_markers() {
        identity("- x\n  - a\n\n  * b\n");
    }

    #[test]
    fn test_thematic_break_on_alternate_marker_line() {
        // `* ***` is five asterisks separated by a space — a thematic break in
        // its own right, which would dissolve the list. The alternate marker
        // takes the alternate break spelling.
        let out = roundtrip("- a\n\n* ---\n");
        assert_eq!(out, "- a\n\n* ---\n");
        let lists: Vec<_> = parse(&out)
            .blocks
            .into_iter()
            .filter(|b| matches!(b, Block::List { .. }))
            .collect();
        assert_eq!(lists.len(), 2, "list dissolved: {out:?}");
    }

    // ── multi-line setext headings ────────────────────────────────────────────

    #[test]
    fn test_multiline_setext_heading_collapses_to_one_line() {
        // An ATX heading is one line, so the break has to go — otherwise the
        // second line falls out of the heading and becomes its own paragraph.
        let out = roundtrip("A\nB\n===\n");
        assert_eq!(out, "# A B\n");
    }

    #[test]
    fn test_multiline_setext_h2_collapses_to_one_line() {
        assert_eq!(roundtrip("A\nB\n---\n"), "## A B\n");
    }

    #[test]
    fn test_hard_break_in_heading_collapses_too() {
        assert_eq!(roundtrip("A\\\nB\n===\n"), "# A B\n");
    }

    #[test]
    fn test_break_nested_in_heading_emphasis_collapses() {
        assert_eq!(roundtrip("*A\nB*\n===\n"), "# *A B*\n");
    }

    #[test]
    fn test_multiline_setext_heading_stays_one_block() {
        let out = roundtrip("A\nB\n===\n\ntext\n");
        let doc = parse(&out);
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b, Block::Heading { .. }))
                .count(),
            1
        );
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b, Block::Paragraph { .. }))
                .count(),
            1,
            "heading's second line became a paragraph: {out:?}"
        );
    }

    // ── empty headings ────────────────────────────────────────────────────────

    #[test]
    fn test_identity_empty_atx_heading() {
        // `# ` is `#` plus trailing whitespace, which nothing wants in a diff.
        identity("#\n");
        identity("###\n");
    }

    #[test]
    fn test_empty_heading_among_content() {
        identity("#\n\ntext\n");
    }

    #[test]
    fn test_identity_empty_list_item() {
        // The marker line ends at the marker rather than carrying a lone space.
        identity("- a\n-\n");
    }

    // ── ragged table rows ─────────────────────────────────────────────────────

    #[test]
    fn test_identity_table_row_longer_than_header() {
        // GFM renders the surplus cell as nothing, but deleting it from the
        // file is data loss the author never asked for.
        identity("| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n");
    }

    #[test]
    fn test_table_row_overflow_reaches_the_ast() {
        let doc = parse("| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n");
        let Some(Block::Table { rows, .. }) = doc.blocks.first() else {
            panic!("expected a table, got {:?}", doc.blocks);
        };
        assert_eq!(rows[0].len(), 3, "overflow cell dropped: {rows:?}");
        assert_eq!(crate::ast::inlines_to_string(&rows[0][2]), "3");
    }

    #[test]
    fn test_table_row_overflow_without_closing_pipe() {
        let out = roundtrip("| a | b |\n| --- | --- |\n| 1 | 2 | 3\n");
        assert_eq!(out, "| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n");
    }

    #[test]
    fn test_table_row_overflow_empty_cell_survives() {
        identity("| a | b |\n| --- | --- |\n| 1 | 2 |  |\n");
    }

    #[test]
    fn test_table_row_overflow_with_escaped_pipe() {
        // `\|` is content, not a cell boundary — even inside a surplus cell.
        let out = roundtrip("| a | b |\n| --- | --- |\n| 1 | 2 | x \\| y |\n");
        assert_eq!(out, "| a | b |\n| --- | --- |\n| 1 | 2 | x \\| y |\n");
        let Some(Block::Table { rows, .. }) = parse(&out).blocks.first().cloned() else {
            panic!("expected a table");
        };
        assert_eq!(crate::ast::inlines_to_string(&rows[0][2]), "x | y");
    }

    #[test]
    fn test_table_row_overflow_carries_inline_markup() {
        let out = roundtrip("| a | b |\n| --- | --- |\n| 1 | 2 | **x** |\n");
        assert_eq!(out, "| a | b |\n| --- | --- |\n| 1 | 2 | **x** |\n");
    }

    #[test]
    fn test_table_row_shorter_than_header_is_still_padded() {
        // The other direction is unchanged: a short row gets its empty cells.
        assert_eq!(
            roundtrip("| a | b |\n| --- | --- |\n| 1 |\n"),
            "| a | b |\n| --- | --- |\n| 1 |  |\n"
        );
    }

    #[test]
    fn test_table_without_outer_pipes_keeps_its_overflow() {
        assert_eq!(
            roundtrip("a | b\n--- | ---\n1 | 2 | 3\n"),
            "| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n"
        );
    }

    // ── empty and nested code blocks ──────────────────────────────────────────

    #[test]
    fn test_identity_empty_fenced_code_block() {
        // An empty fence holds no lines; it used to gain a blank one per pass.
        identity("```\n```\n");
    }

    #[test]
    fn test_identity_empty_fenced_code_block_in_list() {
        identity("- x\n\n  ```\n  ```\n");
    }

    #[test]
    fn test_empty_code_block_content_stays_empty() {
        let out = roundtrip("```\n```\n");
        let doc = parse(&out);
        let Some(Block::CodeBlock { content, .. }) = doc.blocks.first() else {
            panic!("expected a code block");
        };
        assert_eq!(content, "", "empty fence grew a line: {out:?}");
    }

    #[test]
    fn test_identity_blank_line_inside_nested_code_block() {
        // The blank line gets no indent: indenting it would leave trailing
        // whitespace, which is both invisible and a change to the code.
        identity("- x\n\n  ```\n  a\n\n  b\n  ```\n");
    }

    #[test]
    fn test_indented_code_block_keeps_its_trailing_newline() {
        // `terraform/providers/aws/us-east-1/email/handler/README.md` ends on an
        // indented code block. Its final `\n` never reached the AST, so the
        // serializer had to invent one and `parse(serialize(d))` disagreed with
        // `parse(d)` — an invisible drift that only bites once something reads
        // the content instead of writing it straight back out.
        let doc = parse("Run it:\n\n    ./smoke_invoke.sh\n");
        let Some(Block::CodeBlock { content, .. }) = doc.blocks.last() else {
            panic!("expected a code block, got {:?}", doc.blocks);
        };
        assert_eq!(content, "./smoke_invoke.sh\n");
    }

    #[test]
    fn test_indented_code_block_at_eof_comes_back_fenced() {
        // Indented code is normalized to a fence — the block itself has to
        // survive that intact, trailing newline included.
        assert_eq!(
            roundtrip("Run it:\n\n    AWS_PROFILE=x \\\n      ./smoke_invoke.sh\n"),
            "Run it:\n\n```\nAWS_PROFILE=x \\\n  ./smoke_invoke.sh\n```\n"
        );
    }

    #[test]
    fn test_nested_code_block_blank_line_has_no_trailing_space() {
        let out = roundtrip("- x\n\n  ```\n  a\n\n  b\n  ```\n");
        assert!(
            !out.lines().any(|l| !l.is_empty() && l.trim().is_empty()),
            "trailing whitespace on a blank line: {out:?}"
        );
    }

    // ── CRLF and lone CR ──────────────────────────────────────────────────────

    #[test]
    fn test_crlf_document_comes_out_all_lf() {
        let out = roundtrip("# H\r\n\r\n```\r\nfn a() {}\r\n\r\nfn b() {}\r\n```\r\n\r\ntext\r\n");
        assert!(!out.contains('\r'), "carriage return survived: {out:?}");
        assert_eq!(out, "# H\n\n```\nfn a() {}\n\nfn b() {}\n```\n\ntext\n");
    }

    #[test]
    fn test_crlf_inside_a_fence_does_not_reach_the_ast() {
        let doc = parse("```\r\na\r\nb\r\n```\r\n");
        let Some(Block::CodeBlock { content, .. }) = doc.blocks.first() else {
            panic!("expected a code block, got {:?}", doc.blocks);
        };
        assert_eq!(content, "a\nb\n");
    }

    #[test]
    fn test_lone_cr_is_a_line_ending() {
        // CommonMark counts a bare CR as a line ending; pulldown-cmark does not,
        // and an indented code block written that way came back with the source
        // indent baked into its content.
        let out = roundtrip("    a\r    b\r");
        assert_eq!(out, "```\na\nb\n```\n");
    }

    #[test]
    fn test_crlf_frontmatter_still_parses() {
        let doc = parse("---\r\ntype: note\r\n---\r\n\r\n# H\r\n");
        assert_eq!(
            doc.frontmatter.as_ref().and_then(|f| f.doc_type.as_deref()),
            Some("note")
        );
    }

    // ── AST stability across a format pass ────────────────────────────────────

    #[test]
    fn test_reparse_of_output_equals_parse_of_input() {
        // `parse(serialize(d)) == parse(d)`: the text was already stable, but a
        // code or HTML block that ran to EOF without a trailing newline came
        // back with one and the two ASTs disagreed.
        for input in [
            "```\na",
            "<div>\nhi\n</div>",
            "```\n```\n",
            "# H\n\ntext\n\n```rust\nlet a = 1;\n```\n",
            "text\n\n    indented at eof\n",
            "    a\n    b\n",
            "| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n",
            "- a\n- b\n\n* c\n* d\n",
            "* one tight\n\n- loose a\n\n- loose b\n",
            "A\nB\n===\n",
            "#\n",
        ] {
            let doc = parse(input);
            let reparsed = parse(&serialize(&doc));
            assert_eq!(doc.blocks, reparsed.blocks, "AST drifted for {input:?}");
        }
    }

    // ── Byte identity, block by block ─────────────────────────────────────────
    //
    // Everything here is already spelled the way the serializer spells it, so
    // the output has to come back byte-for-byte. `roundtrip` alone would pass
    // on a construct that vanished, which is how tables and hard breaks stayed
    // broken under a green suite; `identity` is what actually pins them down.

    #[test]
    fn test_identity_tables() {
        identity("| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        identity("| left | center | right |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 |\n");
        identity("| head |\n| --- |\n");
        identity("| a | b |\n| --- | --- |\n| **bold** | [link](x.md) |\n");
        identity("| pipe |\n| --- |\n| a \\| b |\n");
        identity("| code |\n| --- |\n| `a | b` |\n");
        identity("| empty |\n| --- |\n|  |\n");
    }

    #[test]
    fn test_identity_blockquotes() {
        identity("> quoted\n");
        identity("> one\n> two\n");
        identity("> > nested\n");
        identity("> - item\n> - item\n");
        identity("> ```\n> code\n> ```\n");
        identity("> # Heading\n");
        identity("> para one\n>\n> para two\n");
        identity("> | a |\n> | --- |\n> | 1 |\n");
    }

    #[test]
    fn test_identity_images_and_links() {
        identity("![alt](img.png)\n");
        identity("![alt](img.png \"title\")\n");
        identity("![](img.png)\n");
        identity("![**bold** alt](img.png)\n");
        identity("[text](page.md)\n");
        identity("[text](page.md \"title\")\n");
        identity("[![alt](img.png)](page.md)\n");
        identity("<https://example.com>\n");
        identity("[spaced](<a b.md>)\n");
    }

    #[test]
    fn test_identity_thematic_breaks() {
        identity("para\n\n---\n\npara\n");
        identity("> ---\n");
        identity("- item\n\n---\n\n- item\n");
        // Opening the file, where `---` would be read back as a frontmatter
        // fence, the break is spelled `***` instead.
        identity("***\n");
        identity("***\n\npara\n");
    }

    #[test]
    fn test_identity_lists() {
        identity("- a\n- b\n");
        identity("1. a\n2. b\n");
        identity("5. a\n6. b\n");
        identity("- a\n  - b\n    - c\n");
        identity("- a\n\n- b\n");
        identity("- para\n\n  second para\n");
        // An item with no inline content opens its first block on the marker
        // line rather than leaving the marker bare.
        identity("- | a | b |\n  | --- | --- |\n  | 1 | 2 |\n");
        identity("- ```\n  code\n  ```\n");
        identity("- > quoted\n");
        identity("1. a\n   1. b\n");
    }

    #[test]
    fn test_identity_escapes() {
        identity("\\*not emphasis\\*\n");
        identity("\\_not emphasis\\_\n");
        identity("\\# not a heading\n");
        identity("\\- not a list\n");
        identity("1\\. not a list\n");
        identity("\\`not code\\`\n");
        identity("\\[not a link](x)\n");
        identity("100% | pipe\n");
        // `\s` is not an escape, so the backslash stays bare.
        identity("back\\slash\n");
    }

    #[test]
    fn test_identity_hard_breaks() {
        identity("a\\\nb\n");
        identity("**a\\\nb**\n");
        identity("| a |\n| --- |\n| 1 |\n\nx\\\ny\n");
        identity("> a\\\n> b\n");
        identity("- a\\\n  b\n");
    }

    #[test]
    fn test_identity_mixed_document() {
        // The constructs above in one file, since block boundaries are where
        // blank-line normalization gets a chance to eat something.
        identity(concat!(
            "---\n",
            "type: note\n",
            "---\n",
            "# Title\n",
            "\n",
            "Intro with `code`, **bold**, *em*, ~~struck~~, and a \\*star\\*.\n",
            "\n",
            "## Section\n",
            "\n",
            "> A quote with a hard break\\\n",
            "> and a second line.\n",
            "\n",
            "- list item\n",
            "  - nested\n",
            "\n",
            "| a | b |\n",
            "| --- | --- |\n",
            "| 1 | 2 |\n",
            "\n",
            "---\n",
            "\n",
            "![picture](img.png \"caption\")\n",
            "\n",
            "```rust\n",
            "let x = 1;\n",
            "```\n",
            "\n",
            "<div>\n",
            "raw\n",
            "</div>\n",
        ));
    }

    // ── Non-canonical spellings survive the rewrite ───────────────────────────
    //
    // These do *not* come back byte-identical — that is the point, the
    // serializer has one spelling for each of them. What must hold is that the
    // rewrite says the same thing, which `roundtrip` now asserts.

    #[test]
    fn test_alternate_spellings_survive() {
        for input in [
            // setext headings become ATX
            "Title\n=====\n",
            "Sub\n---\n",
            "Two\nLines\n=====\n",
            // entities and escapes are two spellings of one character
            "&#42;star&#42;\n",
            "&amp; and &lt;\n",
            "&copy; 2026\n",
            // thematic breaks have six spellings
            "---\n\npara\n",
            "___\n\npara\n",
            "- - -\n\npara\n",
            // list markers
            "* a\n* b\n",
            "+ a\n+ b\n",
            "1) a\n2) b\n",
            // renumbering
            "1. a\n1. b\n1. c\n",
            "3. a\n9. b\n",
            // indented code becomes fenced
            "    let x = 1;\n",
            // tables with ragged rows
            "| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n",
            "| a | b |\n| --- | --- |\n| 1 |\n",
            // hard break spelled with trailing spaces
            "a  \nb\n",
            // emphasis spellings
            "__bold__\n",
            "_em_\n",
            // blockquote without the space, and lazy continuation
            ">quoted\n",
            "> lazy\ncontinuation\n",
            // no trailing newline at EOF
            "# H",
            "text",
            "- a",
            "| a |\n| --- |\n| 1 |",
            // blank-line runs collapse
            "a\n\n\n\n\nb\n",
            // CRLF
            "# H\r\n\r\ntext\r\n",
        ] {
            roundtrip(input);
        }
    }

    // ── Generated documents ───────────────────────────────────────────────────

    /// A deterministic 64-bit linear congruential generator.
    ///
    /// Deterministic on purpose: a property test that picks a fresh seed each
    /// run reports failures nobody can reproduce. Walking a fixed sequence of
    /// seeds covers the same ground and a failure names the seed that found it.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            // Numerical Recipes' constants.
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 16
        }

        fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
            choices[self.next() as usize % choices.len()]
        }
    }

    /// Block-level fragments, each a complete block with its trailing newline.
    const BLOCKS: &[&str] = &[
        "# Heading\n",
        "###### Deep heading\n",
        "Title\n=====\n",
        "Sub\n---\n",
        "A paragraph.\n",
        "Two\nsoft-broken\nlines.\n",
        "A hard\\\nbreak.\n",
        "Trailing  \nspaces break.\n",
        "- a\n- b\n",
        "* a\n* b\n",
        "1. a\n2. b\n",
        "7) a\n8) b\n",
        "- a\n  - nested\n",
        "- a\n\n- loose\n",
        "- | h |\n  | --- |\n  | c |\n",
        "> quoted\n",
        "> > deep\n",
        "> - item\n",
        "| a | b |\n| --- | --- |\n| 1 | 2 |\n",
        "| l | c | r |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 |\n",
        "| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n",
        "```\ncode\n```\n",
        "```rust\nlet x = 1;\n```\n",
        "    indented code\n",
        "***\n",
        "---\n",
        "<div>\nraw\n</div>\n",
        "\n",
        "\n\n\n",
    ];

    /// Inline fragments, spliced into a paragraph.
    const INLINES: &[&str] = &[
        "plain",
        "**bold**",
        "__bold__",
        "*em*",
        "_em_",
        "~~struck~~",
        "`code`",
        "`` ` ``",
        "[link](page.md)",
        "[titled](page.md \"t\")",
        "![img](a.png)",
        "<https://example.com>",
        "<span>html</span>",
        "\\*escaped\\*",
        "&#42;",
        "&amp;",
        "a|b",
        "1. not a list",
        "#hash",
        "under_score_word",
        "star*inside",
        "trailing\\\\",
    ];

    /// What a generated document is wrapped in. Container prefixes are where
    /// the interesting failures live: a line that is inert at column 0 can open
    /// a block once something is written down the left of it.
    ///
    /// One container per document, not per block. Alternating them mid-document
    /// mostly produces half-closed containers and lazy continuations spanning
    /// them — shapes that say more about pulldown-cmark's recovery than about
    /// this serializer, and that no author writes.
    /// Each entry is (opening prefix, prefix for every line after it). A
    /// blockquote keeps its marker down the whole left edge; a list marker is
    /// replaced by the spaces that align with its content column.
    const CONTAINERS: &[(&str, &str)] = &[("", ""), ("> ", "> "), ("- ", "  "), ("> > ", "> > ")];

    #[test]
    fn test_generated_documents_round_trip() {
        for seed in 0..20_000u64 {
            let mut rng = Lcg(seed);
            // Only the first line of the document takes the marker; the rest
            // take the matching indent, which is what makes `- ` one list item
            // holding everything rather than one bullet per line.
            let (prefix, indent) = CONTAINERS[rng.next() as usize % CONTAINERS.len()];
            let mut doc = String::new();
            let mut first = true;
            for _ in 0..(rng.next() % 6) + 1 {
                let fragment = if rng.next().is_multiple_of(4) {
                    // A paragraph assembled out of inline fragments, which is
                    // where escaping decisions actually collide with each other.
                    let n = (rng.next() % 4) + 1;
                    let parts: Vec<&str> = (0..n).map(|_| rng.pick(INLINES)).collect();
                    format!("{}\n", parts.join(" "))
                } else {
                    rng.pick(BLOCKS).to_string()
                };
                for line in fragment.trim_end_matches('\n').split('\n') {
                    doc.push_str(if std::mem::take(&mut first) {
                        prefix
                    } else {
                        indent
                    });
                    doc.push_str(line);
                    doc.push('\n');
                }
            }
            // Twenty thousand keeps `cargo test` under a couple of seconds.
            // The same generator was run to a million seeds while the bugs
            // below were being fixed; raise the bound to sweep again.
            //
            // `roundtrip` asserts both properties and names the input it broke
            // on; the seed is what makes that input reproducible.
            let result = std::panic::catch_unwind(|| roundtrip(&doc));
            assert!(result.is_ok(), "seed {seed} failed on:\n{doc}");
        }
    }

    // ── The real-document corpus ──────────────────────────────────────────────
    //
    // Hand-written fixtures test the constructs someone thought to write down.
    // These two walk every markdown file the checkout holds instead — this
    // crate's own prose, and, next door, typedown's test corpus under
    // `test-docs/`, its journal, its presets, its prose at the root — and hold
    // the serializer to them. It is the only coverage that grows on its own:
    // every document anyone adds joins the corpus.
    //
    // The corpus is whatever the checkout happens to hold. A `cargo test` in a
    // git worktree sees all of it; a worktree sparse enough to leave typedown
    // out, or the vendored copy of this crate inside typedown's public mirror,
    // sees only the files here. Both are valid runs of these tests — they just
    // cover different numbers of documents.

    /// The directories these tests read markdown from: this crate, plus the
    /// typedown crate beside it when the checkout has it.
    fn corpus_roots() -> Vec<std::path::PathBuf> {
        let here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let sibling = here.join("../typedown");
        let mut roots = vec![here];
        if sibling.is_dir() {
            roots.push(sibling);
        }
        roots
    }

    /// Every markdown file under [`corpus_roots`], as (labelled path, source).
    fn corpus() -> Vec<(String, String)> {
        let mut files: Vec<_> = corpus_roots()
            .into_iter()
            .flat_map(|root| {
                // The label carries the crate directory so a failure names a
                // file someone can open; `root` itself is an absolute path
                // that says nothing.
                let label = root
                    .canonicalize()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                walkdir::WalkDir::new(&root)
                    .into_iter()
                    // `target/` holds vendored markdown from every dependency,
                    // which says nothing about this serializer and takes
                    // seconds to walk.
                    .filter_entry(|e| {
                        !matches!(e.file_name().to_str(), Some("target") | Some(".git"))
                    })
                    .filter_map(Result::ok)
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
                    .map(|e| {
                        let rel = e.path().strip_prefix(&root).unwrap().display().to_string();
                        (
                            format!("{label}/{rel}"),
                            std::fs::read_to_string(e.path()).unwrap(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        files.sort();
        // A walk that finds nothing passes both corpus tests vacuously, so
        // anchor it on a file that is present in every environment these tests
        // run in: this crate's own README, which ships wherever the crate does.
        // A count can't play this role — see the note above the walk.
        assert!(
            files
                .iter()
                .any(|(p, _)| p.ends_with("typedown-md/README.md")),
            "corpus is missing this crate's README.md ({} file(s) found) — \
             the walk is broken, not the corpus",
            files.len()
        );
        files
    }

    /// Whether the corpus expects `path` to already be in canonical form.
    ///
    /// `tasks/` is a drop-box: files land there written to an external tool's
    /// task convention (a blank line under the frontmatter, descriptions quoted
    /// rather than folded), get worked, and get deleted. Holding them to
    /// typedown's own output spelling would fail on the next task anyone files.
    /// They still have to survive a round trip — see
    /// [`test_corpus_loses_nothing`] — they just don't have to be a fixed point
    /// of it.
    fn is_canonical(path: &str) -> bool {
        !path.contains("/tasks/")
    }

    #[test]
    fn test_corpus_is_byte_identical() {
        let mut drifted = Vec::new();
        for (path, src) in corpus().into_iter().filter(|(p, _)| is_canonical(p)) {
            let out = serialize(&parse(&src));
            if out != src {
                let first = src
                    .lines()
                    .zip(out.lines())
                    .enumerate()
                    .find(|(_, (a, b))| a != b)
                    .map(|(i, (a, b))| format!("line {}: {a:?} -> {b:?}", i + 1))
                    .unwrap_or_else(|| {
                        format!("{} lines -> {}", src.lines().count(), out.lines().count())
                    });
                drifted.push(format!("  {path}\n    {first}"));
            }
        }
        assert!(
            drifted.is_empty(),
            "{} canonical file(s) changed under parse → serialize:\n{}",
            drifted.len(),
            drifted.join("\n")
        );
    }

    #[test]
    fn test_corpus_loses_nothing() {
        let mut lossy = Vec::new();
        for (path, src) in corpus() {
            // Not `roundtrip`, which panics on the first failure: a whole-corpus
            // sweep is far more useful when it reports every bad file at once.
            let once = serialize(&parse(&src));
            let twice = serialize(&parse(&once));
            if once != twice {
                lossy.push(format!("  {path}: second pass differs from the first"));
            } else if content_of(&src) != content_of(&once) {
                lossy.push(format!("  {path}: the document says something else now"));
            }
        }
        assert!(
            lossy.is_empty(),
            "{} file(s) did not survive a format pass:\n{}",
            lossy.len(),
            lossy.join("\n")
        );
    }
}
