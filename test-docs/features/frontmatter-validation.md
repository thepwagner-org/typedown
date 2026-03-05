---
status: passing
complexity: 2
added: 2026-03-03
breaking: false
tags:
  - frontmatter
---
# frontmatter-validation

## What It Tests

Typedown validates frontmatter fields against the schema's `fields` definitions.
Each field declares a type (`string`, `date`, `integer`, `bool`, `enum`, `list`)
and can be marked `required`. Enum fields are checked against an `values` list.
List fields check every item against the item type. Unknown fields are ignored.

## How to Break It

- **Remove a required field**: Delete `status` or `complexity` from frontmatter
- **Use a bad enum value**: Change `status` to `broken` instead of `passing`, `failing`, or `untested`
- **Wrong type for integer**: Set `complexity` to `high` instead of a number
- **Wrong type for date**: Set `added` to `yesterday` instead of `YYYY-MM-DD`
- **Bad list item**: Add `- parsing` to `tags` (not in the allowed values list)

## Expected Errors

1. Missing required field `status`
2. Invalid enum value `broken` for `status`
3. Invalid integer value `high` for `complexity`
4. Invalid date value `yesterday` for `added`
5. Invalid enum value `parsing` in list `tags`

## Related Features

- [Section Structure](./section-structure.md)
