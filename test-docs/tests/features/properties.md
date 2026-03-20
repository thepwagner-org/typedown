---
status: passing
complexity: 3
added: 2026-03-13
breaking: false
tags:
  - json
  - structure
---
# properties

## What It Tests

Section definitions can declare a `properties` map — typed named fields expected
in each top-level list item's sub-list. Each item's child list is parsed as
`Key: Value` pairs (split on `: `), validated against the declared types, and
coerced into a typed `properties` object in `td json` output. Schema keys are
lowercase; document text may use any case (`Quantity: 4` matches schema key
`quantity`). Validation catches missing required properties, wrong types
(integer, date, bool, enum), and invalid enum values — the same type system
used for frontmatter fields. `test-docs/hardware/homelab.md` is the live
exercise document validated by the `test-hardware` schema.

## How to Break It

- **Remove required property**: Delete `Quantity:` from a component in `test-docs/hardware/homelab.md`
- **Wrong type for integer**: Set `Quantity: many` in any component item
- **Invalid enum value**: Change `Category: memory` to `Category: furniture`

## Expected Errors

1. Missing required property `quantity` in `Components`
2. Invalid integer value for `quantity` in `Components`
3. Invalid enum value `furniture` for `category` in `Components`

## Related Features

- [JSON Output](./json-output.md)
