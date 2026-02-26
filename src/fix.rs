//! Auto-fix logic: apply fixes for diagnostics that have a known correction.
//!
//! Fixes are separate from `Diagnostic` — they describe the action, not the
//! problem. A fix operates on `&mut Document` and requires no I/O.

use crate::{
    ast::{Block, Document, Inline},
    validate::Diagnostic,
};

// ── Fix ───────────────────────────────────────────────────────────────────────

/// An auto-correctable action derived from one or more diagnostics.
#[derive(Debug, Clone)]
pub enum Fix {
    /// Insert an H1 heading at the top of the document.
    InsertH1 { text: String },
    /// Replace the existing H1 heading text.
    ReplaceH1 { text: String },
    /// Replace or append a managed section.
    UpdateManagedSection {
        /// Block index of section start (None if section doesn't exist yet).
        section_start: Option<usize>,
        /// Block index of section end (exclusive).
        section_end: usize,
        /// Template blocks to write into the section.
        template_blocks: Vec<Block>,
        /// User-written content appended after the template (preserved).
        custom_content: Vec<Block>,
    },
    /// Insert an intro paragraph into a section.
    InsertSectionIntro {
        /// Line number after which to insert (used for display; insertion uses block index).
        insert_after_line: usize,
        /// The intro paragraph text.
        text: String,
    },
    /// Rebuild the document with date entries in the correct sort order.
    SortEntries {
        preamble: Vec<Block>,
        sorted_entries: Vec<(String, Option<String>, Vec<Block>)>,
    },
    /// Remove empty optional sections.
    ///
    /// `section_ranges` is a list of `(start_block_idx, end_block_idx_exclusive)`
    /// pairs in document order.  The fix removes them in reverse to preserve
    /// earlier indices.
    RemoveEmptySections { section_ranges: Vec<(usize, usize)> },
}

impl Fix {
    /// Returns `true` if this diagnostic variant has a corresponding auto-fix.
    pub fn is_fixable(diag: &Diagnostic) -> bool {
        matches!(
            diag,
            Diagnostic::MissingH1 { .. }
                | Diagnostic::H1Mismatch { .. }
                | Diagnostic::ManagedSectionNeedsUpdate { .. }
                | Diagnostic::SectionNeedsIntro { .. }
                | Diagnostic::EntriesOutOfOrder { .. }
                | Diagnostic::EmptyOptionalSection { .. }
        )
    }

    /// Derive a fix from a diagnostic, if one exists.
    pub fn from_diagnostic(diag: &Diagnostic) -> Option<Self> {
        match diag {
            Diagnostic::MissingH1 { expected } => Some(Fix::InsertH1 {
                text: expected.clone(),
            }),
            Diagnostic::H1Mismatch { expected, .. } => Some(Fix::ReplaceH1 {
                text: expected.clone(),
            }),
            Diagnostic::ManagedSectionNeedsUpdate {
                section_start,
                section_end,
                template_blocks,
                custom_content,
            } => Some(Fix::UpdateManagedSection {
                section_start: *section_start,
                section_end: *section_end,
                template_blocks: template_blocks.clone(),
                custom_content: custom_content.clone(),
            }),
            Diagnostic::SectionNeedsIntro {
                insert_after_line,
                text,
            } => Some(Fix::InsertSectionIntro {
                insert_after_line: *insert_after_line,
                text: text.clone(),
            }),
            Diagnostic::EntriesOutOfOrder {
                preamble,
                sorted_entries,
            } => Some(Fix::SortEntries {
                preamble: preamble.clone(),
                sorted_entries: sorted_entries.clone(),
            }),
            Diagnostic::EmptyOptionalSection { section_ranges } => Some(Fix::RemoveEmptySections {
                section_ranges: section_ranges.clone(),
            }),
            _ => None,
        }
    }

    /// Apply this fix to the document.
    pub fn apply(&self, doc: &mut Document) {
        match self {
            Fix::InsertH1 { text } => {
                if text.is_empty() {
                    return; // RequiredAny with no expected text — nothing to insert
                }
                doc.blocks.insert(
                    0,
                    Block::Heading {
                        level: 1,
                        content: vec![Inline::Text(text.clone())],
                        line: 0,
                    },
                );
            }

            Fix::ReplaceH1 { text } => {
                if let Some(block) = doc
                    .blocks
                    .iter_mut()
                    .find(|b| matches!(b, Block::Heading { level: 1, .. }))
                {
                    *block = Block::Heading {
                        level: 1,
                        content: vec![Inline::Text(text.clone())],
                        line: block.line(),
                    };
                }
            }

            Fix::UpdateManagedSection {
                section_start,
                section_end,
                template_blocks,
                custom_content,
            } => {
                let mut new_section = template_blocks.clone();
                if !custom_content.is_empty() {
                    new_section.push(Block::BlankLine);
                    new_section.extend(custom_content.clone());
                }

                match section_start {
                    Some(idx) => {
                        doc.blocks.splice(*idx..*section_end, new_section);
                    }
                    None => {
                        // Append section at the end
                        if !doc.blocks.is_empty()
                            && !matches!(doc.blocks.last(), Some(Block::BlankLine))
                        {
                            doc.blocks.push(Block::BlankLine);
                        }
                        doc.blocks.extend(new_section);
                    }
                }
            }

            Fix::InsertSectionIntro {
                insert_after_line,
                text,
            } => {
                // Find the block whose line number matches the heading we insert after,
                // then skip past any immediately-following BlankLine so the paragraph
                // lands directly before the section content (no extra blank between them).
                let heading_idx = doc
                    .blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.line() > 0 && b.line() <= *insert_after_line)
                    .map(|(i, _)| i)
                    .next_back()
                    .unwrap_or(0);

                // Skip over a BlankLine immediately after the heading.
                let insert_idx =
                    if matches!(doc.blocks.get(heading_idx + 1), Some(Block::BlankLine)) {
                        heading_idx + 2
                    } else {
                        heading_idx + 1
                    };

                doc.blocks.insert(
                    insert_idx,
                    Block::Paragraph {
                        content: vec![Inline::Text(text.clone())],
                        line: 0,
                    },
                );
            }

            Fix::SortEntries {
                preamble,
                sorted_entries,
            } => {
                doc.blocks = preamble.clone();
                for (_, _, entry_blocks) in sorted_entries {
                    doc.blocks.extend(entry_blocks.clone());
                }
            }

            Fix::RemoveEmptySections { section_ranges } => {
                // Remove in reverse document order so that earlier indices
                // remain valid after each splice.
                let mut ranges = section_ranges.clone();
                ranges.sort_unstable_by_key(|&(start, _)| start);
                for (start, end) in ranges.into_iter().rev() {
                    if start < doc.blocks.len() {
                        let end = end.min(doc.blocks.len());
                        doc.blocks.drain(start..end);
                    }
                }
            }
        }
    }
}

// ── apply_fixes ───────────────────────────────────────────────────────────────

/// Apply all fixable diagnostics to a document in one pass.
///
/// Diagnostics are applied in order. Callers should re-validate after fixing if
/// precise line numbers matter (block indices shift after insertions/removals).
pub fn apply_fixes(doc: &mut Document, diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        if let Some(fix) = Fix::from_diagnostic(diag) {
            fix.apply(doc);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse, serialize};

    fn apply_and_serialize(input: &str, diagnostics: &[Diagnostic]) -> String {
        let mut doc = parse(input);
        apply_fixes(&mut doc, diagnostics);
        serialize(&doc)
    }

    // ── InsertH1 ──────────────────────────────────────────────────────────────

    #[test]
    fn test_insert_h1_prepends_heading() {
        let result = apply_and_serialize(
            "---\ntype: t\n---\nSome content.\n",
            &[Diagnostic::MissingH1 {
                expected: "My Title".to_string(),
            }],
        );
        assert!(result.contains("# My Title\n"), "got: {result}");
        // H1 should appear before the paragraph
        let h1_pos = result.find("# My Title").unwrap();
        let para_pos = result.find("Some content").unwrap();
        assert!(h1_pos < para_pos);
    }

    #[test]
    fn test_insert_h1_empty_expected_is_noop() {
        let input = "---\ntype: t\n---\nContent.\n";
        let result = apply_and_serialize(
            input,
            &[Diagnostic::MissingH1 {
                expected: String::new(),
            }],
        );
        // No heading should have been inserted
        assert!(!result.contains("# \n"), "got: {result}");
    }

    // ── ReplaceH1 ─────────────────────────────────────────────────────────────

    #[test]
    fn test_replace_h1() {
        let result = apply_and_serialize(
            "---\ntype: t\n---\n# Old Title\n\nContent.\n",
            &[Diagnostic::H1Mismatch {
                line: 3,
                expected: "New Title".to_string(),
                actual: "Old Title".to_string(),
            }],
        );
        assert!(result.contains("# New Title\n"), "got: {result}");
        assert!(!result.contains("# Old Title"), "old title should be gone");
    }

    // ── UpdateManagedSection (append) ─────────────────────────────────────────

    #[test]
    fn test_managed_section_appended_when_missing() {
        let template_doc = parse("## Related\n\n- placeholder\n");
        let result = apply_and_serialize(
            "---\ntype: t\n---\n# Doc\n\nIntro.\n",
            &[Diagnostic::ManagedSectionNeedsUpdate {
                section_start: None,
                section_end: 0, // unused when appending
                template_blocks: template_doc.blocks.clone(),
                custom_content: vec![],
            }],
        );
        assert!(result.contains("## Related\n"), "got: {result}");
        assert!(result.contains("- placeholder"), "got: {result}");
    }

    #[test]
    fn test_managed_section_replaced() {
        let template_doc = parse("## Related\n\n- new content\n");
        let input = "---\ntype: t\n---\n# Doc\n\n## Related\n\n- old content\n";
        let mut doc = parse(input);

        // Find the H2 block index
        let section_start = doc
            .blocks
            .iter()
            .position(|b| matches!(b, Block::Heading { level: 2, .. }))
            .unwrap();
        let section_end = doc.blocks.len();

        apply_fixes(
            &mut doc,
            &[Diagnostic::ManagedSectionNeedsUpdate {
                section_start: Some(section_start),
                section_end,
                template_blocks: template_doc.blocks.clone(),
                custom_content: vec![],
            }],
        );

        let result = serialize(&doc);
        assert!(result.contains("- new content"), "got: {result}");
        assert!(!result.contains("- old content"), "got: {result}");
    }

    // ── is_fixable / from_diagnostic ─────────────────────────────────────────

    #[test]
    fn test_is_fixable_missing_h1() {
        assert!(Fix::is_fixable(&Diagnostic::MissingH1 {
            expected: "X".to_string()
        }));
    }

    #[test]
    fn test_is_fixable_unknown_type_is_not_fixable() {
        assert!(!Fix::is_fixable(&Diagnostic::UnknownType {
            line: 1,
            message: "bad type".to_string()
        }));
    }

    #[test]
    fn test_from_diagnostic_missing_section_is_none() {
        assert!(Fix::from_diagnostic(&Diagnostic::MissingSection {
            section: "Goals".to_string()
        })
        .is_none());
    }

    // ── RemoveEmptySections ───────────────────────────────────────────────────

    #[test]
    fn test_remove_empty_section_single() {
        // Document: H1 + H2 "Notes" with no body
        let result = apply_and_serialize(
            "---\ntype: t\n---\n# Title\n\n## Notes\n",
            &[Diagnostic::EmptyOptionalSection {
                section_ranges: vec![(2, 3)], // H2 is block index 2, end = 3
            }],
        );
        assert!(
            !result.contains("## Notes"),
            "section should be gone: {result}"
        );
        assert!(result.contains("# Title"), "H1 should remain: {result}");
    }

    #[test]
    fn test_remove_empty_sections_multiple_reverse_order() {
        // Two empty optional sections; ensure both are removed cleanly
        let input = "---\ntype: t\n---\n# Title\n\n## Alpha\n\n## Beta\n";
        let mut doc = parse(input);

        // Find actual block indices
        let alpha_idx = doc
            .blocks
            .iter()
            .position(|b| matches!(b, Block::Heading { level: 2, .. }))
            .unwrap();
        let beta_idx = doc
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::Heading { level: 2, .. }))
            .unwrap();

        let doc_len = doc.blocks.len();
        apply_fixes(
            &mut doc,
            &[Diagnostic::EmptyOptionalSection {
                section_ranges: vec![
                    (alpha_idx, beta_idx), // Alpha: up to (but not including) Beta
                    (beta_idx, doc_len),   // Beta: to end
                ],
            }],
        );
        let result = serialize(&doc);
        assert!(
            !result.contains("## Alpha"),
            "Alpha should be gone: {result}"
        );
        assert!(!result.contains("## Beta"), "Beta should be gone: {result}");
        assert!(result.contains("# Title"), "H1 should remain: {result}");
    }

    #[test]
    fn test_is_fixable_empty_optional_section() {
        assert!(Fix::is_fixable(&Diagnostic::EmptyOptionalSection {
            section_ranges: vec![(0, 1)],
        }));
    }

    #[test]
    fn test_malformed_link_is_not_fixable() {
        assert!(!Fix::is_fixable(&Diagnostic::MalformedLink {
            line: 5,
            url: "bad url.md".to_string(),
        }));
    }
}
