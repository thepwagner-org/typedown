---
status: passing
complexity: 3
added: 2026-03-03
breaking: true
tags:
  - structure
  - auto-fix
---
# section-structure

## What It Tests

Typedown enforces section presence, order, and content type. With
`strict_sections: true`, unlisted H2s are rejected and listed sections must
appear in schema order. Required sections must be present. `bullets: unordered`
and `bullets: ordered` enforce list type. The `template` field validates each
bullet item against a pattern. `td fmt` can reorder out-of-order sections and
insert managed content.

## How to Break It

- **Reorder sections**: Move `Expected Errors` above `How to Break It`
- **Add unexpected section**: Insert a `## History` heading
- **Remove required section**: Delete `## What It Tests` entirely
- **Wrong list type in ordered section**: Use `-` bullets in `Expected Errors`
- **Wrong list type in unordered section**: Use `1.` numbering in `How to Break It`
- **Template mismatch**: Write a bullet without the `**Bold**: Text` pattern in `How to Break It`

## Expected Errors

1. Section out of order: `Expected Errors` before `How to Break It`
2. Unexpected section: `History`
3. Missing required section: `What It Tests`
4. Wrong list type in `Expected Errors`: expected ordered
5. Wrong list type in `How to Break It`: expected unordered
6. Template mismatch in `How to Break It`

## Related Features

- [Frontmatter Validation](./frontmatter-validation.md)
- [Link Validation](./link-validation.md)
