//! Auto-fix logic: apply fixes for diagnostics that have a known correction.
//!
//! Fixes are separate from `Diagnostic` — they describe the action, not the
//! problem. A fix operates on `&mut Document` and requires no I/O.

#[cfg(test)]
use crate::ast::inlines_to_string;
use crate::{
    ast::{Block, Document, Inline, ListItem},
    validate::{Diagnostic, SortedEntry},
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
        /// Blocks to write into the section: the template, with its lists
        /// already upserted against what the document had.
        managed_blocks: Vec<Block>,
        /// User-written content the template didn't account for (preserved,
        /// appended after the managed blocks).
        custom_content: Vec<Block>,
    },
    /// Rebuild the document with date entries in the correct sort order.
    SortEntries {
        preamble: Vec<Block>,
        sorted_entries: Vec<SortedEntry>,
    },
    /// Remove empty optional sections.
    ///
    /// `section_ranges` is a list of `(start_block_idx, end_block_idx_exclusive)`
    /// pairs in document order.  The fix removes them in reverse to preserve
    /// earlier indices.
    RemoveEmptySections { section_ranges: Vec<(usize, usize)> },
    /// Convert paragraph blocks to bullet list items in a bullets-only section.
    ConvertParagraphsToBullets {
        /// Block indices of `Block::Paragraph` to convert (in document order).
        paragraph_indices: Vec<usize>,
        /// Whether to produce ordered lists.
        ordered: bool,
    },
    /// Flip a list's ordered/unordered flag.
    ConvertListType {
        /// Block index of the `Block::List` to convert.
        list_index: usize,
        /// Target: `true` for ordered, `false` for unordered.
        ordered: bool,
    },
    /// Rebuild the document with sections in schema-defined order.
    ReorderSections {
        preamble: Vec<Block>,
        sorted_sections: Vec<Vec<Block>>,
    },
}

impl Fix {
    /// Returns `true` if this diagnostic variant has a corresponding auto-fix.
    pub fn is_fixable(diag: &Diagnostic) -> bool {
        matches!(
            diag,
            Diagnostic::MissingH1 { .. }
                | Diagnostic::H1Mismatch { .. }
                | Diagnostic::ManagedSectionNeedsUpdate { .. }
                | Diagnostic::EntriesOutOfOrder { .. }
                | Diagnostic::EmptyOptionalSection { .. }
                | Diagnostic::SectionNotBullets { .. }
                | Diagnostic::WrongListType { .. }
                | Diagnostic::SectionOutOfOrder { .. }
                | Diagnostic::SectionsOutOfOrder { .. }
        )
    }

    /// Derive a fix from a diagnostic, if one exists.
    ///
    /// For [`Diagnostic::SectionNotBullets`] and [`Diagnostic::WrongListType`],
    /// use [`build_content_fixes`] instead — those require document context
    /// to locate the correct block indices.
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
                managed_blocks,
                custom_content,
            } => Some(Fix::UpdateManagedSection {
                section_start: *section_start,
                section_end: *section_end,
                managed_blocks: managed_blocks.clone(),
                custom_content: custom_content.clone(),
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
            Diagnostic::SectionsOutOfOrder {
                preamble,
                sorted_sections,
            } => Some(Fix::ReorderSections {
                preamble: preamble.clone(),
                sorted_sections: sorted_sections.clone(),
            }),
            // SectionOutOfOrder is informational (per-section); the fix comes from SectionsOutOfOrder
            // SectionNotBullets and WrongListType are handled by build_content_fixes
            Diagnostic::SectionOutOfOrder { .. }
            | Diagnostic::SectionNotBullets { .. }
            | Diagnostic::WrongListType { .. } => None,
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
                managed_blocks,
                custom_content,
            } => {
                let mut new_section = managed_blocks.clone();
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

            Fix::SortEntries {
                preamble,
                sorted_entries,
            } => {
                doc.blocks = preamble.clone();
                for (_, _, _, entry_blocks) in sorted_entries {
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

            Fix::ConvertParagraphsToBullets {
                paragraph_indices,
                ordered,
            } => {
                let mut indices = paragraph_indices.clone();
                indices.sort_unstable();
                indices.retain(|&i| matches!(doc.blocks.get(i), Some(Block::Paragraph { .. })));
                // Convert a run at a time, in reverse order to preserve earlier
                // indices. Paragraphs with nothing but blank lines between them
                // become items of one list rather than a list apiece: adjacent
                // lists have to be written with different markers to stay
                // adjacent, and a section of bullets that alternates `-` and `*`
                // is not what "convert to bullets" was asked for.
                for run in paragraph_runs(&doc.blocks, &indices).into_iter().rev() {
                    let last = *run.last().expect("runs are never empty");
                    let items = run
                        .iter()
                        .map(|&i| ListItem {
                            content: match &doc.blocks[i] {
                                Block::Paragraph { content, .. } => content.clone(),
                                _ => unreachable!("runs hold paragraph indices"),
                            },
                            // The blank line that separated the paragraphs is
                            // what keeps the list loose, so each item but the
                            // last carries it into the list.
                            children: if i == last {
                                vec![]
                            } else {
                                vec![Block::BlankLine]
                            },
                        })
                        .collect();
                    let start_idx = run[0];
                    let line = doc.blocks[start_idx].line();
                    doc.blocks.splice(
                        start_idx..=last,
                        [Block::List {
                            items,
                            ordered: *ordered,
                            start: 1,
                            line,
                        }],
                    );
                }
            }

            Fix::ConvertListType {
                list_index,
                ordered,
            } => {
                if let Some(Block::List {
                    ordered: current, ..
                }) = doc.blocks.get_mut(*list_index)
                {
                    *current = *ordered;
                }
            }

            Fix::ReorderSections {
                preamble,
                sorted_sections,
            } => {
                doc.blocks = preamble.clone();
                for section in sorted_sections {
                    doc.blocks.extend(section.clone());
                }
            }
        }
    }
}

/// Group sorted block indices into runs separated by nothing but blank lines.
///
/// Two paragraphs with a gap between them are still neighbours; one with a
/// heading or a table in between is not, and the two belong to different lists.
fn paragraph_runs(blocks: &[Block], indices: &[usize]) -> Vec<Vec<usize>> {
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for &idx in indices {
        let adjacent = runs.last().and_then(|run| run.last()).is_some_and(|&prev| {
            blocks[prev + 1..idx]
                .iter()
                .all(|b| matches!(b, Block::BlankLine))
        });
        match runs.last_mut() {
            Some(run) if adjacent => run.push(idx),
            _ => runs.push(vec![idx]),
        }
    }
    runs
}

// ── Content fixes (paragraph ↔ bullet conversion) ────────────────────────────

/// Build fixes for `SectionNotBullets` and `WrongListType` diagnostics.
///
/// These diagnostics carry only a line number, so we need to scan the document
/// to find the block indices of the offending blocks.
pub fn build_content_fixes(doc: &Document, diagnostics: &[Diagnostic]) -> Vec<Fix> {
    let mut fixes = Vec::new();

    // Group SectionNotBullets by line → collect paragraph block indices
    let not_bullets_lines: std::collections::HashSet<usize> = diagnostics
        .iter()
        .filter_map(|d| match d {
            Diagnostic::SectionNotBullets { line, .. } => Some(*line),
            _ => None,
        })
        .collect();

    if !not_bullets_lines.is_empty() {
        let para_indices: Vec<usize> = doc
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| match b {
                Block::Paragraph { line, .. } if not_bullets_lines.contains(line) => Some(i),
                _ => None,
            })
            .collect();

        if !para_indices.is_empty() {
            fixes.push(Fix::ConvertParagraphsToBullets {
                paragraph_indices: para_indices,
                ordered: false, // default to unordered for paragraph conversion
            });
        }
    }

    // WrongListType → flip ordered flag on the matching list block
    for diag in diagnostics {
        if let Diagnostic::WrongListType { line, expected, .. } = diag {
            let target_ordered = expected == "ordered";
            if let Some(idx) = doc.blocks.iter().position(|b| match b {
                Block::List {
                    line: l, ordered, ..
                } => *l == *line && *ordered != target_ordered,
                _ => false,
            }) {
                fixes.push(Fix::ConvertListType {
                    list_index: idx,
                    ordered: target_ordered,
                });
            }
        }
    }

    fixes
}

// ── apply_fixes ───────────────────────────────────────────────────────────────

/// Apply all fixable diagnostics to a document in one pass.
///
/// Diagnostics are applied in order. Callers should re-validate after fixing if
/// precise line numbers matter (block indices shift after insertions/removals).
pub fn apply_fixes(doc: &mut Document, diagnostics: &[Diagnostic]) {
    // First apply content fixes (paragraph→bullet, list type conversion)
    let content_fixes = build_content_fixes(doc, diagnostics);
    for fix in &content_fixes {
        fix.apply(doc);
    }

    // Then apply structural fixes
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
                managed_blocks: template_doc.blocks.clone(),
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
                managed_blocks: template_doc.blocks.clone(),
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

    // ── ConvertParagraphsToBullets ─────────────────────────────────────────────

    #[test]
    fn test_convert_paragraph_to_bullet() {
        let input = "---\ntype: t\n---\n## Goals\n\nThis is a paragraph.\n";
        let doc = parse(input);
        // Find the paragraph's line number for the diagnostic
        let para_line = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Paragraph { line, .. } => Some(*line),
                _ => None,
            })
            .unwrap();

        let result = apply_and_serialize(
            input,
            &[Diagnostic::SectionNotBullets {
                line: para_line,
                context: "section 'Goals'".to_string(),
            }],
        );
        assert!(
            result.contains("- This is a paragraph."),
            "paragraph should be converted to bullet: {result}"
        );
        assert!(
            !result.contains("\nThis is a paragraph."),
            "bare paragraph should be gone: {result}"
        );
    }

    #[test]
    fn test_convert_multiple_paragraphs_to_bullets() {
        let input = "---\ntype: t\n---\n## Goals\n\nFirst paragraph.\n\nSecond paragraph.\n";
        let doc = parse(input);
        let para_diags: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { line, .. } => Some(Diagnostic::SectionNotBullets {
                    line: *line,
                    context: "section 'Goals'".to_string(),
                }),
                _ => None,
            })
            .collect();

        let mut doc = parse(input);
        apply_fixes(&mut doc, &para_diags);
        let result = serialize(&doc);
        assert!(result.contains("- First paragraph."), "got: {result}");
        assert!(result.contains("- Second paragraph."), "got: {result}");
    }

    #[test]
    fn test_paragraphs_converted_to_bullets_form_one_list() {
        // The bullets fix used to make a list per paragraph and lean on the
        // serializer's adjacent-list merge to weld them; now it builds the list
        // it means, and the serialized output says so.
        let mut doc = parse("## Goals\n\nFirst.\n\nSecond.\n");
        let diags: Vec<_> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { line, .. } => Some(Diagnostic::SectionNotBullets {
                    line: *line,
                    context: "section 'Goals'".to_string(),
                }),
                _ => None,
            })
            .collect();
        apply_fixes(&mut doc, &diags);
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b, Block::List { .. }))
                .count(),
            1,
            "expected one list: {:?}",
            doc.blocks
        );
        assert_eq!(serialize(&doc), "## Goals\n\n- First.\n\n- Second.\n");
    }

    // ── ConvertListType ───────────────────────────────────────────────────────

    #[test]
    fn test_convert_unordered_to_ordered() {
        let input = "---\ntype: t\n---\n## Steps\n\n- First\n- Second\n";
        let doc = parse(input);
        let list_line = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::List { line, .. } => Some(*line),
                _ => None,
            })
            .unwrap();

        let result = apply_and_serialize(
            input,
            &[Diagnostic::WrongListType {
                line: list_line,
                context: "section 'Steps'".to_string(),
                expected: "ordered".to_string(),
            }],
        );
        assert!(result.contains("1."), "should be ordered list: {result}");
        assert!(
            !result.contains("- First"),
            "should not have unordered markers: {result}"
        );
    }

    #[test]
    fn test_convert_ordered_to_unordered() {
        let input = "---\ntype: t\n---\n## Items\n\n1. First\n2. Second\n";
        let doc = parse(input);
        let list_line = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::List { line, .. } => Some(*line),
                _ => None,
            })
            .unwrap();

        let result = apply_and_serialize(
            input,
            &[Diagnostic::WrongListType {
                line: list_line,
                context: "section 'Items'".to_string(),
                expected: "unordered".to_string(),
            }],
        );
        assert!(
            result.contains("- First"),
            "should be unordered list: {result}"
        );
        assert!(
            !result.contains("1."),
            "should not have ordered markers: {result}"
        );
    }

    // ── is_fixable for new diagnostics ────────────────────────────────────────

    #[test]
    fn test_is_fixable_section_not_bullets() {
        assert!(Fix::is_fixable(&Diagnostic::SectionNotBullets {
            line: 5,
            context: "section 'Goals'".to_string(),
        }));
    }

    #[test]
    fn test_is_fixable_wrong_list_type() {
        assert!(Fix::is_fixable(&Diagnostic::WrongListType {
            line: 5,
            context: "section 'Steps'".to_string(),
            expected: "ordered".to_string(),
        }));
    }

    // ── ReorderSections ──────────────────────────────────────────────────────

    #[test]
    fn test_reorder_sections_swaps_two() {
        let input =
            "---\ntype: t\n---\n# Doc\n\n## Beta\n\nBeta content.\n\n## Alpha\n\nAlpha content.\n";
        let doc = parse(input);

        // Find H2 block indices
        let h2_indices: Vec<usize> = doc
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| match b {
                Block::Heading { level: 2, .. } => Some(i),
                _ => None,
            })
            .collect();

        let first_h2 = h2_indices[0];
        let second_h2 = h2_indices[1];
        let preamble = doc.blocks[..first_h2].to_vec();
        let beta_section = doc.blocks[first_h2..second_h2].to_vec();
        let alpha_section = doc.blocks[second_h2..].to_vec();

        let result = apply_and_serialize(
            input,
            &[Diagnostic::SectionsOutOfOrder {
                preamble,
                sorted_sections: vec![alpha_section, beta_section],
            }],
        );
        let alpha_pos = result.find("## Alpha").expect("Alpha should exist");
        let beta_pos = result.find("## Beta").expect("Beta should exist");
        assert!(
            alpha_pos < beta_pos,
            "Alpha should come before Beta: {result}"
        );
    }

    #[test]
    fn test_reorder_sections_preserves_content() {
        let input = "---\ntype: t\n---\n# Doc\n\n## Gamma\n\nG content.\n\n## Alpha\n\nA content.\n\n## Beta\n\nB content.\n";
        let doc = parse(input);

        // Build preamble + sorted sections from the parsed doc
        // Blocks: [frontmatter, H1, blank, H2-Gamma, blank, para-G, blank, H2-Alpha, blank, para-A, blank, H2-Beta, blank, para-B]
        let h2_indices: Vec<usize> = doc
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| match b {
                Block::Heading { level: 2, .. } => Some(i),
                _ => None,
            })
            .collect();

        let first_h2 = h2_indices[0];
        let preamble = doc.blocks[..first_h2].to_vec();

        // Extract sections
        let mut sections: Vec<(usize, Vec<Block>)> = Vec::new();
        for (pos, &start) in h2_indices.iter().enumerate() {
            let end = h2_indices.get(pos + 1).copied().unwrap_or(doc.blocks.len());
            // Schema order: Alpha=0, Beta=1, Gamma=2
            let order = match &doc.blocks[start] {
                Block::Heading { content, .. } => {
                    let title = inlines_to_string(content);
                    match title.as_str() {
                        "Alpha" => 0,
                        "Beta" => 1,
                        "Gamma" => 2,
                        _ => 99,
                    }
                }
                _ => 99,
            };
            sections.push((order, doc.blocks[start..end].to_vec()));
        }
        sections.sort_by_key(|(order, _)| *order);
        let sorted_sections: Vec<Vec<Block>> =
            sections.into_iter().map(|(_, blocks)| blocks).collect();

        let result = apply_and_serialize(
            input,
            &[Diagnostic::SectionsOutOfOrder {
                preamble,
                sorted_sections,
            }],
        );

        // Verify order
        let alpha_pos = result.find("## Alpha").unwrap();
        let beta_pos = result.find("## Beta").unwrap();
        let gamma_pos = result.find("## Gamma").unwrap();
        assert!(alpha_pos < beta_pos, "Alpha < Beta: {result}");
        assert!(beta_pos < gamma_pos, "Beta < Gamma: {result}");

        // Verify content preserved
        assert!(
            result.contains("A content."),
            "Alpha content preserved: {result}"
        );
        assert!(
            result.contains("B content."),
            "Beta content preserved: {result}"
        );
        assert!(
            result.contains("G content."),
            "Gamma content preserved: {result}"
        );
    }

    #[test]
    fn test_is_fixable_section_out_of_order() {
        assert!(Fix::is_fixable(&Diagnostic::SectionOutOfOrder {
            line: 5,
            section: "Alpha".to_string(),
        }));
    }

    #[test]
    fn test_is_fixable_sections_out_of_order() {
        assert!(Fix::is_fixable(&Diagnostic::SectionsOutOfOrder {
            preamble: vec![],
            sorted_sections: vec![],
        }));
    }
}
