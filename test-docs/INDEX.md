---
edition: 1
updated: 2026-03-03
---
# Test Index

## Features

- [Frontmatter Validation](./features/frontmatter-validation.md)
- [Section Structure](./features/section-structure.md)
- [Link Validation](./features/link-validation.md)
- [Auto Fix](./features/auto-fix.md)
- [JSON Output](./features/json-output.md)
- [Properties](./features/properties.md)

## How This Works

These test documents are self-referential: each file documents a
typedown feature while being validated by that same feature. Break
a file and typedown catches it. Run `td check` to see errors or
`td fmt` to auto-fix what it can.

## Out of Scope

Things this test suite intentionally does not cover:

- LSP protocol compliance (tested manually via OpenCode)
- Performance benchmarks
- Parser edge cases (covered by unit tests in `src/parse.rs`)
