---
status: passing
complexity: 2
added: 2026-03-13
breaking: false
tags:
  - json
---
# json-output

## What It Tests

`td json` outputs each document as a structured JSON object with `path`, `type`,
`frontmatter`, `title`, `intro`, and `sections`. Frontmatter values are coerced
to native JSON types by the schema: `integer` fields become numbers, `bool`
fields become booleans, `list` fields become arrays. Path-matched files that
carry no `type:` in frontmatter get the resolved type injected. Content between
H1 and the first H2 appears as `intro`. H2s become `sections`, each with
`heading`, `content` (full markdown, lossless), `links` (all `[text](url)` pairs
extracted), and `items` (present when the section body contains lists — each item
has `text` with markup stripped and optionally nested `items` for sub-lists).
H3s nest as `subsections`, H4s nest further; H5+ folds into the H4's content.
Multiple files produce JSONL — one object per line — which `jq` can slice and
aggregate without slurp mode.

## How to Break It

- **Remove a required field**: Delete `status` or `complexity` from frontmatter
- **Use a bad enum value**: Change `status` to `broken`
- **Wrong type for integer**: Set `complexity` to `high`

## Expected Errors

1. Missing required field `status`
2. Invalid enum value `broken` for `status`
3. Invalid integer value `high` for `complexity`

## Related Features

- [Frontmatter Validation](./frontmatter-validation.md)
- [Properties](./properties.md)
