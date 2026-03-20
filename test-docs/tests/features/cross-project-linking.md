---
status: passing
complexity: 3
added: 2026-03-18
breaking: false
tags:
  - links
---
# cross-project-linking

## What It Tests

When a section declares `links: target_type: <type>` and the link resolves
outside the current validation root, typedown walks up from the target file to
find its nearest `.typedown/` schema directory, loads it, and resolves the
target's type from frontmatter or path patterns. The type check then works
cross-project.

The `External Links` section below links to `../../hardware/homelab.md`. When
running `td check test-docs/tests/`, that file is outside the root — typedown
discovers `test-docs/hardware/.typedown/`, resolves the target as
`test-hardware`, and the `target_type: test-hardware` constraint passes. From
the repo root both schemas are in scope and validation works through the
ordinary path.

## How to Break It

- **Wrong target type**: Change the `External Links` link to `./section-structure.md` — type is `test-feature`, not `test-hardware`
- **Remove external schema**: Delete `test-docs/hardware/.typedown/` and run `td check test-docs/tests/` — target type cannot be resolved
- **Untype the target**: Remove `type: test-hardware` from `homelab.md` and run `td check test-docs/tests/` — target resolves as untyped

## Expected Errors

1. Wrong target type: `LinkTargetTypeMismatch` — expected `test-hardware`, got `test-feature`
2. Missing external schema: `LinkTargetTypeMismatch` — expected `test-hardware`, actual: none
3. Untyped target: `LinkTargetTypeMismatch` — expected `test-hardware`, actual: none

## Related Features

- [Link Validation](./link-validation.md)

## External Links

- [Homelab](../../hardware/homelab.md)
