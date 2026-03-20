---
maintained_by: typedown project
version: "1.0"
---
# tests

- Self-referential test documents for the typedown schema engine
- Each file documents a feature while being validated by that feature
- Run `td check` to validate or `td fmt` to auto-fix

## Sections

- [Test Index](./INDEX.md)

## Coverage

- **Field types**: string, integer, date, bool, enum, list-of-enum
- **Title modes**: from_filename, from_directory, from_date, fixed string
- **Section features**: strict_sections, required, bullets (ordered/unordered), template, intro_text, managed_content
- **Link features**: target_type, bidirectional, cross-project schema discovery
- **Date headings**: oldest_first sort (chronological log)
- **Intro section**: structure-level intro between H1 and first H2 (this file)
- **Size warning**: enforced on feature docs (2000 byte limit)
- **JSON output**: td json, frontmatter coercion, JSONL aggregation with jq
- **Properties**: section-level key-value property maps, typed sub-items, td json coercion (test-hardware schema + hardware/homelab.md)

## Running the Suite

Run `td check` from the repo root to validate all test documents against their
schemas. A clean run produces no output and exits 0. Run `td fmt` to auto-fix
formatting issues -- the result should be idempotent (a second run changes nothing).
