//! Markdown parsing and serialization using pulldown-cmark.

use crate::ast::{
    inlines_to_string, Block, ColumnAlignment, Document, Frontmatter, Inline, ListItem,
};
use gray_matter::{engine::YAML, Matter};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::collections::{HashSet, VecDeque};

/// Parser options: CommonMark + GFM tables only.
///
/// Strikethrough is intentionally excluded: pulldown-cmark treats `~` as a
/// strikethrough delimiter, corrupting prose like `~$5` (approximately $5).
fn parser_options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts
}

/// Parse markdown content into a Document AST.
pub fn parse(content: &str) -> Document {
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(content);
    let frontmatter: Option<Frontmatter> = parsed.data.and_then(|d| d.deserialize().ok());

    let parser = Parser::new_ext(&parsed.content, parser_options());
    let events: VecDeque<Event> = parser.collect();
    let mut blocks = parse_blocks(events);

    let fm_lines = count_frontmatter_lines(content);
    attach_line_numbers(&mut blocks, &parsed.content, fm_lines);

    Document {
        frontmatter,
        blocks,
    }
}

/// Get the frontmatter parse error from raw content, if any.
///
/// Returns `Some(message)` if frontmatter exists but failed to deserialize,
/// `None` if there is no frontmatter or it parsed successfully.
pub fn get_frontmatter_error(content: &str) -> Option<String> {
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(content);
    parsed
        .data
        .and_then(|d| d.deserialize::<Frontmatter>().err().map(|e| e.to_string()))
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
    let mut out = String::new();

    if let Some(fm) = &doc.frontmatter {
        out.push_str("---\n");
        if field_order.is_empty() {
            // type first, then all other fields in insertion order
            if let Some(doc_type) = &fm.doc_type {
                serialize_yaml_field(
                    &mut out,
                    "type",
                    &serde_yaml::Value::String(doc_type.clone()),
                    0,
                );
            }
            for (key, value) in &fm.fields {
                serialize_yaml_field(&mut out, key, value, 0);
            }
        } else {
            serialize_frontmatter_ordered(&mut out, fm, field_order);
        }
        out.push_str("---\n");
    }

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
                out.push_str(&"#".repeat(*level as usize));
                out.push(' ');
                out.push_str(&serialize_inlines(content));
                out.push('\n');
            }
            Block::Paragraph { content, .. } => {
                out.push_str(&serialize_inlines(content));
                out.push('\n');
            }
            Block::List { items, ordered, .. } => {
                serialize_list(&mut out, items, *ordered, "");
            }
            Block::CodeBlock {
                language, content, ..
            } => {
                let lang = language.as_deref().unwrap_or("");
                out.push_str(&format!("```{lang}\n"));
                out.push_str(content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
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
                out.push_str("---\n");
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

fn parse_blocks(mut events: VecDeque<Event>) -> Vec<Block> {
    let mut blocks = Vec::new();

    while let Some(event) = events.pop_front() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let content = collect_inlines(&mut events, TagEnd::Heading(level));
                blocks.push(Block::Heading {
                    level: heading_level_to_u8(level),
                    content,
                    line: 0,
                });
            }
            Event::Start(Tag::Paragraph) => {
                let content = collect_inlines(&mut events, TagEnd::Paragraph);
                if content.is_empty() {
                    blocks.push(Block::BlankLine);
                } else {
                    blocks.push(Block::Paragraph { content, line: 0 });
                }
            }
            Event::Start(Tag::List(start_num)) => {
                let ordered = start_num.is_some();
                let items = collect_list_items(&mut events);
                blocks.push(Block::List {
                    items,
                    ordered,
                    line: 0,
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
                    line: 0,
                });
            }
            Event::Start(Tag::BlockQuote(_kind)) => {
                let inner_events = collect_container_events(&mut events, |e| {
                    matches!(e, Event::End(TagEnd::BlockQuote(_)))
                });
                let inner_blocks = parse_blocks(inner_events);
                blocks.push(Block::BlockQuote {
                    blocks: inner_blocks,
                    line: 0,
                });
            }
            Event::Start(Tag::Table(alignments)) => {
                let aligns = alignments.into_iter().map(convert_alignment).collect();
                let (header, rows) = collect_table(&mut events);
                blocks.push(Block::Table {
                    alignments: aligns,
                    header,
                    rows,
                    line: 0,
                });
            }
            Event::Rule => {
                blocks.push(Block::ThematicBreak { line: 0 });
            }
            _ => {}
        }
    }

    normalize_blank_lines(&mut blocks);
    blocks
}

fn collect_inlines(events: &mut VecDeque<Event>, end_tag: TagEnd) -> Vec<Inline> {
    let mut inlines = Vec::new();

    while let Some(event) = events.pop_front() {
        match event {
            Event::End(tag) if tag == end_tag => break,
            Event::Text(text) => inlines.push(Inline::Text(text.to_string())),
            Event::Code(code) => inlines.push(Inline::Code(code.to_string())),
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
            Event::Start(Tag::Link { dest_url, .. }) => {
                let inner = collect_inlines(events, TagEnd::Link);
                let text = inlines_to_string(&inner);
                inlines.push(Inline::Link {
                    text,
                    url: dest_url.to_string(),
                });
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let inner = collect_inlines(events, TagEnd::Image);
                let alt = inlines_to_string(&inner);
                inlines.push(Inline::Image {
                    alt,
                    url: dest_url.to_string(),
                });
            }
            Event::SoftBreak => inlines.push(Inline::SoftBreak),
            _ => {}
        }
    }

    inlines
}

fn collect_list_items(events: &mut VecDeque<Event>) -> Vec<ListItem> {
    let mut items = Vec::new();

    while let Some(event) = events.pop_front() {
        match event {
            Event::End(TagEnd::List(_)) => break,
            Event::Start(Tag::Item) => {
                let (content, children) = collect_item_content(events);
                items.push(ListItem { content, children });
            }
            _ => {}
        }
    }

    items
}

fn collect_item_content(events: &mut VecDeque<Event>) -> (Vec<Inline>, Vec<Block>) {
    let mut inlines = Vec::new();
    let mut children = Vec::new();
    let mut first_paragraph = true;

    while let Some(event) = events.pop_front() {
        match event {
            Event::End(TagEnd::Item) => break,
            Event::Start(Tag::Paragraph) => {
                let para_inlines = collect_inlines(events, TagEnd::Paragraph);
                if first_paragraph {
                    inlines = para_inlines;
                    first_paragraph = false;
                } else {
                    children.push(Block::Paragraph {
                        content: para_inlines,
                        line: 0,
                    });
                }
            }
            Event::Start(Tag::List(start_num)) => {
                let ordered = start_num.is_some();
                let items = collect_list_items(events);
                children.push(Block::List {
                    items,
                    ordered,
                    line: 0,
                });
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    _ => None,
                };
                let content = collect_code_block(events);
                children.push(Block::CodeBlock {
                    language,
                    content,
                    line: 0,
                });
            }
            Event::Start(Tag::BlockQuote(_kind)) => {
                let inner_events = collect_container_events(events, |e| {
                    matches!(e, Event::End(TagEnd::BlockQuote(_)))
                });
                let inner_blocks = parse_blocks(inner_events);
                children.push(Block::BlockQuote {
                    blocks: inner_blocks,
                    line: 0,
                });
            }
            Event::Rule => {
                children.push(Block::ThematicBreak { line: 0 });
            }
            Event::Text(text) => {
                inlines.push(Inline::Text(text.to_string()));
                first_paragraph = false;
            }
            Event::Code(code) => {
                inlines.push(Inline::Code(code.to_string()));
                first_paragraph = false;
            }
            Event::Start(Tag::Strong) => {
                let inner = collect_inlines(events, TagEnd::Strong);
                inlines.push(Inline::Strong(inner));
                first_paragraph = false;
            }
            Event::Start(Tag::Emphasis) => {
                let inner = collect_inlines(events, TagEnd::Emphasis);
                inlines.push(Inline::Emphasis(inner));
                first_paragraph = false;
            }
            Event::Start(Tag::Strikethrough) => {
                let inner = collect_inlines(events, TagEnd::Strikethrough);
                inlines.push(Inline::Strikethrough(inner));
                first_paragraph = false;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let inner = collect_inlines(events, TagEnd::Link);
                let text = inlines_to_string(&inner);
                inlines.push(Inline::Link {
                    text,
                    url: dest_url.to_string(),
                });
                first_paragraph = false;
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let inner = collect_inlines(events, TagEnd::Image);
                let alt = inlines_to_string(&inner);
                inlines.push(Inline::Image {
                    alt,
                    url: dest_url.to_string(),
                });
                first_paragraph = false;
            }
            Event::SoftBreak => {
                inlines.push(Inline::SoftBreak);
            }
            _ => {}
        }
    }

    (inlines, children)
}

fn collect_code_block(events: &mut VecDeque<Event>) -> String {
    let mut content = String::new();
    while let Some(event) = events.pop_front() {
        match event {
            Event::End(TagEnd::CodeBlock) => break,
            Event::Text(text) => content.push_str(&text),
            _ => {}
        }
    }
    content
}

fn collect_container_events<'a, F>(
    events: &mut VecDeque<Event<'a>>,
    is_end: F,
) -> VecDeque<Event<'a>>
where
    F: Fn(&Event) -> bool,
{
    let mut inner = VecDeque::new();
    while let Some(event) = events.pop_front() {
        if is_end(&event) {
            break;
        }
        inner.push_back(event);
    }
    inner
}

fn collect_table(events: &mut VecDeque<Event>) -> (Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>) {
    let mut header = Vec::new();
    let mut rows = Vec::new();

    while let Some(event) = events.pop_front() {
        match event {
            Event::End(TagEnd::Table) => break,
            Event::Start(Tag::TableHead) => {
                header = collect_table_row(events, TagEnd::TableHead);
            }
            Event::Start(Tag::TableRow) => {
                rows.push(collect_table_row(events, TagEnd::TableRow));
            }
            _ => {}
        }
    }

    (header, rows)
}

fn collect_table_row(events: &mut VecDeque<Event>, end_tag: TagEnd) -> Vec<Vec<Inline>> {
    let mut cells = Vec::new();

    while let Some(event) = events.pop_front() {
        match event {
            Event::End(tag) if tag == end_tag => break,
            Event::Start(Tag::TableCell) => {
                cells.push(collect_inlines(events, TagEnd::TableCell));
            }
            _ => {}
        }
    }

    cells
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

/// Count lines occupied by YAML frontmatter (including `---` delimiters).
/// Returns 0 if there is no frontmatter.
fn count_frontmatter_lines(content: &str) -> usize {
    if !content.starts_with("---") {
        return 0;
    }
    if let Some(end) = content[3..].find("\n---") {
        let frontmatter_section = &content[..3 + end + 4];
        frontmatter_section.lines().count()
    } else {
        0
    }
}

/// Attach 1-based source line numbers to top-level blocks.
///
/// Uses a second pulldown-cmark pass with offset tracking to map byte offsets
/// to line numbers. Synthetic `BlankLine` blocks are skipped (line stays 0).
fn attach_line_numbers(blocks: &mut [Block], body: &str, fm_lines: usize) {
    // Table of byte offsets marking the start of each line
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(body.match_indices('\n').map(|(i, _)| i + 1))
        .collect();

    // Collect byte offsets of top-level block-start events
    let parser = Parser::new_ext(body, parser_options());
    let mut block_offsets: Vec<usize> = Vec::new();
    let mut depth: usize = 0;

    for (event, range) in parser.into_offset_iter() {
        match &event {
            Event::Start(Tag::Heading { .. })
            | Event::Start(Tag::Paragraph)
            | Event::Start(Tag::List(_))
            | Event::Start(Tag::CodeBlock(_))
            | Event::Start(Tag::BlockQuote(_))
            | Event::Start(Tag::Table(_)) => {
                if depth == 0 {
                    block_offsets.push(range.start);
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
            }
            Event::Rule => {
                if depth == 0 {
                    block_offsets.push(range.start);
                }
            }
            _ => {}
        }
    }

    // Assign line numbers to non-synthetic blocks in order
    let mut offset_idx = 0;
    for block in blocks.iter_mut() {
        if matches!(block, Block::BlankLine) {
            continue; // synthetic, no corresponding event
        }
        if offset_idx < block_offsets.len() {
            let byte_offset = block_offsets[offset_idx];
            let line_idx = line_starts.partition_point(|&start| start <= byte_offset);
            set_block_line(block, line_idx + fm_lines);
            offset_idx += 1;
        }
    }
}

fn set_block_line(block: &mut Block, line: usize) {
    match block {
        Block::Heading { line: l, .. }
        | Block::Paragraph { line: l, .. }
        | Block::List { line: l, .. }
        | Block::CodeBlock { line: l, .. }
        | Block::BlockQuote { line: l, .. }
        | Block::Table { line: l, .. }
        | Block::ThematicBreak { line: l } => *l = line,
        Block::BlankLine => {}
    }
}

// ── serialization internals ───────────────────────────────────────────────────

fn serialize_inlines(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(s) => out.push_str(s),
            Inline::Strong(inner) => {
                out.push_str("**");
                out.push_str(&serialize_inlines(inner));
                out.push_str("**");
            }
            Inline::Emphasis(inner) => {
                out.push('*');
                out.push_str(&serialize_inlines(inner));
                out.push('*');
            }
            Inline::Strikethrough(inner) => {
                out.push_str("~~");
                out.push_str(&serialize_inlines(inner));
                out.push_str("~~");
            }
            Inline::Link { text, url } => {
                out.push_str(&format!("[{text}]({url})"));
            }
            Inline::Image { alt, url } => {
                out.push_str(&format!("![{alt}]({url})"));
            }
            Inline::Code(s) => {
                out.push_str(&crate::ast::format_code_span(s));
            }
            Inline::SoftBreak => out.push('\n'),
        }
    }
    out
}

fn serialize_list(out: &mut String, items: &[ListItem], ordered: bool, indent: &str) {
    for (idx, item) in items.iter().enumerate() {
        let marker = if ordered {
            format!("{}.", idx + 1)
        } else {
            "-".to_string()
        };
        out.push_str(indent);
        out.push_str(&marker);
        out.push(' ');
        out.push_str(&serialize_inlines(&item.content));
        out.push('\n');
        if !item.children.is_empty() {
            let child_indent = format!("{indent}   ");
            serialize_blocks(out, &item.children, &child_indent);
        }
    }
}

fn serialize_blocks(out: &mut String, blocks: &[Block], indent: &str) {
    for block in blocks {
        match block {
            Block::Heading { level, content, .. } => {
                out.push_str(indent);
                out.push_str(&"#".repeat(*level as usize));
                out.push(' ');
                out.push_str(&serialize_inlines(content));
                out.push('\n');
            }
            Block::Paragraph { content, .. } => {
                out.push_str(indent);
                out.push_str(&serialize_inlines(content));
                out.push('\n');
            }
            Block::List { items, ordered, .. } => {
                serialize_list(out, items, *ordered, indent);
            }
            Block::CodeBlock {
                language, content, ..
            } => {
                let lang = language.as_deref().unwrap_or("");
                out.push_str(indent);
                out.push_str(&format!("```{lang}\n"));
                for line in content.lines() {
                    out.push_str(indent);
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str(indent);
                out.push_str("```\n");
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
                out.push_str("---\n");
            }
            Block::BlankLine => {
                out.push('\n');
            }
        }
    }
}

fn serialize_blockquote(out: &mut String, inner: &[Block]) {
    let inner_doc = Document {
        frontmatter: None,
        blocks: inner.to_vec(),
    };
    let inner_text = serialize(&inner_doc);
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
            .map(|c| serialize_inlines(c))
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
        for i in 0..num_cols {
            let cell = row.get(i).map(|c| serialize_inlines(c)).unwrap_or_default();
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
    }
}

fn serialize_frontmatter_ordered(out: &mut String, fm: &Frontmatter, field_order: &[&str]) {
    let order_set: HashSet<&str> = field_order.iter().copied().collect();

    // Write fields in specified order
    for &field in field_order {
        if field == "type" {
            if let Some(doc_type) = &fm.doc_type {
                serialize_yaml_field(out, "type", &serde_yaml::Value::String(doc_type.clone()), 0);
            }
        } else if let Some(value) = fm.fields.get(field) {
            serialize_yaml_field(out, field, value, 0);
        }
    }

    // Append remaining fields not in field_order (in insertion order)
    if !order_set.contains("type") {
        if let Some(doc_type) = &fm.doc_type {
            serialize_yaml_field(out, "type", &serde_yaml::Value::String(doc_type.clone()), 0);
        }
    }
    for (key, value) in &fm.fields {
        if !order_set.contains(key.as_str()) {
            serialize_yaml_field(out, key, value, 0);
        }
    }
}

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
            if needs_yaml_quoting(s) {
                out.push_str(&format!(
                    "{indent_str}{key}: \"{}\"\n",
                    escape_yaml_string(s)
                ));
            } else {
                out.push_str(&format!("{indent_str}{key}: {s}\n"));
            }
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
            out.push_str(&format!("{indent_str}{key}:\n"));
            for (k, v) in map {
                if let serde_yaml::Value::String(k_str) = k {
                    serialize_yaml_field(out, k_str, v, indent + 1);
                }
            }
        }
        serde_yaml::Value::Tagged(_) => {}
    }
}

fn serialize_yaml_list_item(out: &mut String, value: &serde_yaml::Value, indent: usize) {
    let indent_str = "  ".repeat(indent);
    match value {
        serde_yaml::Value::String(s) => {
            if needs_yaml_quoting(s) {
                out.push_str(&format!("{indent_str}- \"{}\"\n", escape_yaml_string(s)));
            } else {
                out.push_str(&format!("{indent_str}- {s}\n"));
            }
        }
        serde_yaml::Value::Number(n) => {
            out.push_str(&format!("{indent_str}- {n}\n"));
        }
        serde_yaml::Value::Bool(b) => {
            out.push_str(&format!("{indent_str}- {b}\n"));
        }
        _ => {
            if let Ok(yaml) = serde_yaml::to_string(value) {
                for line in yaml.trim().lines() {
                    out.push_str(&format!("{indent_str}- {line}\n"));
                }
            }
        }
    }
}

fn needs_yaml_quoting(s: &str) -> bool {
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
}

fn escape_yaml_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse, serialize, parse again, serialize again — assert idempotency.
    fn roundtrip(input: &str) -> String {
        let doc = parse(input);
        let s1 = serialize(&doc);
        let doc2 = parse(&s1);
        let s2 = serialize(&doc2);
        assert_eq!(s1, s2, "serialization must be idempotent:\n{s1}");
        s1
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
                matches!(i, Inline::Link { text, url }
                    if text == "this link" && url == "https://example.com")
            });
            assert!(has_link);
        } else {
            panic!("expected paragraph");
        }
    }

    #[test]
    fn test_parse_image() {
        let doc = parse("![alt text](image.png)\n");
        if let Block::Paragraph { content, .. } = &doc.blocks[0] {
            let has_image = content.iter().any(|i| {
                matches!(i, Inline::Image { alt, url }
                    if alt == "alt text" && url == "image.png")
            });
            assert!(has_image);
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
}
