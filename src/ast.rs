//! AST types: Document, Frontmatter, Block, Inline.

use indexmap::IndexMap;
use serde::Deserialize;
use serde_yaml::Value;

/// A parsed markdown document.
#[derive(Debug, Clone)]
pub struct Document {
    pub frontmatter: Option<Frontmatter>,
    pub blocks: Vec<Block>,
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
    /// Synthetic blank line inserted by normalization. Has no source position.
    BlankLine,
}

impl Block {
    /// Returns the 1-based source line number, or 0 for synthetic `BlankLine`.
    pub fn line(&self) -> usize {
        match self {
            Self::Heading { line, .. }
            | Self::Paragraph { line, .. }
            | Self::List { line, .. }
            | Self::CodeBlock { line, .. }
            | Self::BlockQuote { line, .. }
            | Self::Table { line, .. }
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

/// An inline markdown element.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link { text: String, url: String },
    Image { alt: String, url: String },
    Code(String),
    SoftBreak,
}

/// Format a code span with the correct number of backtick delimiters.
///
/// Per CommonMark spec §6.1:
/// - Use N+1 backticks where N is the longest run of consecutive backticks
///   in `content` (minimum 1).
/// - If content starts or ends with a backtick, pad with one space on each
///   side so the delimiter backticks aren't ambiguous.
pub fn format_code_span(content: &str) -> String {
    // Find the longest run of consecutive backticks in the content.
    let mut max_run = 0;
    let mut current_run = 0;
    for ch in content.chars() {
        if ch == '`' {
            current_run += 1;
            if current_run > max_run {
                max_run = current_run;
            }
        } else {
            current_run = 0;
        }
    }

    let delim_count = max_run + 1;
    let delim: String = "`".repeat(delim_count);

    // Pad with spaces when content starts or ends with a backtick so the
    // delimiter isn't confused with the content.
    let needs_pad = content.starts_with('`') || content.ends_with('`');
    if needs_pad {
        format!("{delim} {content} {delim}")
    } else {
        format!("{delim}{content}{delim}")
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
            Inline::Link { text, .. } => out.push_str(text),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::Code(s) => out.push_str(s),
            Inline::SoftBreak => out.push(' '),
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
            Inline::Link { text, url } => {
                out.push('[');
                out.push_str(text);
                out.push_str("](");
                out.push_str(url);
                out.push(')');
            }
            Inline::Image { alt, url } => {
                out.push_str("![");
                out.push_str(alt);
                out.push_str("](");
                out.push_str(url);
                out.push(')');
            }
            Inline::Code(s) => {
                out.push_str(&format_code_span(s));
            }
            Inline::SoftBreak => out.push(' '),
        }
    }
    out
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
            text: "click here".to_string(),
            url: "https://example.com".to_string(),
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
            text: "docs".to_string(),
            url: "https://example.com".to_string(),
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
}
