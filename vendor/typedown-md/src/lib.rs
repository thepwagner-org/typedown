//! The markdown document layer behind typedown.
//!
//! Three modules, no I/O, no schema knowledge:
//!
//! - [`ast`] — `Document`, `Block`, `Inline`, `Frontmatter`. Line numbers live
//!   on the block, not in a parallel array.
//! - [`parse`] — pulldown-cmark to AST, and AST back to markdown. Takes
//!   `&str`, returns a [`ast::Document`]; serializing a parsed document is a
//!   fixed point for anything already formatted.
//! - [`escape`] — re-escaping for serialized text. Parsing resolves `\*` and
//!   `&#42;` into plain text, so output has to put the escapes back wherever a
//!   character would parse as markup again.
//!
//! Much of the surface is public for the tools next door rather than for `td`
//! itself. Other programs in the monorepo read and write these documents too,
//! and a program that disagrees with `td fmt` about formatting rewrites its
//! output forever.
//!
//! Writing a document has one blessed path — build the AST, serialize it:
//!
//! - [`ast::Frontmatter::from_serialize`] — frontmatter from any `Serialize`
//!   type; [`ast::Frontmatter::set`] for one field at a time.
//! - [`ast::Document::new`] with [`ast::Block`]'s constructors
//!   ([`ast::Block::heading`], [`ast::Block::paragraph`],
//!   [`ast::Block::bullet_list`], …) — the body. Text is content here:
//!   serialization escapes whatever would read back as markup.
//! - [`parse::serialize`] — the whole document, byte-identical to what
//!   `td fmt` would leave. [`parse::serialize_frontmatter`] emits the
//!   frontmatter block alone, for a tool that hands the body back verbatim.
//!
//! Assembling markdown or frontmatter with `format!` instead is how a stray
//! `*`, `:` or `#` corrupts a document — every escaping and quoting rule
//! above exists because some hand-writer missed it.
//!
//! Reading without a full parse:
//!
//! - [`parse::split_frontmatter`] — where frontmatter ends, with nothing
//!   parsed and both halves borrowed.
//! - [`parse::parse_frontmatter`], [`parse::frontmatter_field`] and
//!   [`ast::Frontmatter::field`] — the frontmatter alone, deserialized into
//!   the caller's own type. The body is never parsed, which is what makes
//!   reading one key across a corpus cheap.
//! - [`ast::links`] — every link in a parsed body, one walk shared by every
//!   consumer.
//!
//! Lower-level pieces — [`parse::yaml_scalar`], [`parse::needs_yaml_quoting`]
//! and friends — stay public for tools that splice a single value into text
//! they otherwise don't own.

pub mod ast;
pub mod escape;
pub mod parse;
