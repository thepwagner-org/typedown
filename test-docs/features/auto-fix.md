---
status: passing
complexity: 2
added: 2026-03-03
breaking: false
tags:
  - auto-fix
  - structure
---
# auto-fix

## What It Tests

`td fmt` auto-fixes certain classes of errors that are unambiguous to correct.
It does not fix errors that require judgment (like a missing required section
with unknown content). Fixes are applied in a deterministic order and are
idempotent -- running `td fmt` twice produces the same result as once.

## How to Break It

- **Wrong H1**: Change `# auto-fix` to `# Auto Fix` -- `td fmt` rewrites it back
- **Edit managed content**: Change the `## How This Works` section in `INDEX.md` -- `td fmt` restores it
- **Missing intro text**: Remove the `Things this test suite intentionally does not cover:` paragraph from `INDEX.md` -- `td fmt` re-inserts it
- **Out-of-order sections**: Swap two sections -- `td fmt` reorders them back

## Expected Errors

1. H1 mismatch: expected `auto-fix` (from filename), got `Auto Fix`
2. Managed section content mismatch in `INDEX.md`
3. Missing intro text in `INDEX.md Out of Scope` section
4. Section out of order

## Related Features

- [Link Validation](./link-validation.md)
