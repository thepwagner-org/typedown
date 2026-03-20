---
status: passing
complexity: 3
added: 2026-03-03
breaking: false
tags:
  - links
---
# link-validation

## What It Tests

Typedown unconditionally checks that relative links in a document resolve to real
files -- every `[text](./path)` is verified. Link extraction covers all block
types: paragraphs, list items, headings, blockquotes, and table cells. Image
references (`![alt](path)`) are checked too. Query strings and fragment
identifiers (`?query` / `#fragment`) are stripped before resolution.

Sections can declare `links: target_type: <type>` to require links point to a
specific schema type. With `links: bidirectional: true`, the linked file must
also link back. Links to `#anchors` and `https://` URLs are skipped.

## How to Break It

- **Add a broken link**: Write `[see this](./does-not-exist.md)` anywhere in the file
- **Add a broken image**: Write `![screenshot](./missing-screenshot.png)` in a section
- **Broken link in blockquote**: Add `> See [this](./no-such-file.md)` in a section
- **Link to wrong type**: Add a link to `../INDEX.md` in the `Related Features` section (it is type `test-index`, not `test-feature`)
- **Break bidirectionality**: Remove the `[Link Validation]` link from `section-structure.md`'s Related Features section

## Expected Errors

1. Broken link: `./does-not-exist.md` does not exist
2. Broken image: `./missing-screenshot.png` does not exist
3. Broken link in blockquote: `./no-such-file.md` does not exist
4. Link target type mismatch: expected `test-feature`, got `test-index`
5. Missing backlink from `section-structure.md` to this file

## Related Features

- [Section Structure](./section-structure.md)
- [Auto Fix](./auto-fix.md)
- [Cross-Project Linking](./cross-project-linking.md)
