# typedown

Like TypeScript adds types to JavaScript, typedown adds types to markdown. Define schemas in `.typedown/*.yaml`, declare a document's type in its frontmatter, and typedown validates structure, field types, section ordering, and cross-document links.

## Usage

```bash
td fmt              # validate, fix, and format
td check            # validate without writing (for CI)
td init             # scaffold a .typedown/ directory
td lsp              # start language server (LSP over stdio)
```

## How It Works

Create `.typedown/recipe.yaml`:
```yaml
description: A recipe document
fields:
  servings:
    type: integer
    required: true
  cuisine:
    type: enum
    values: [italian, mexican, japanese, thai, french, american]
  source:
    type: link
```
Write `pasta.md`:
```markdown
---
type: recipe
servings: 4
cuisine: italian
---
# Pasta
```
Run `td fmt` and typedown validates the frontmatter fields, checks required sections, verifies links exist, and formats the markdown.

## Features

- Schema-driven validation via `.typedown/*.yaml`
- Frontmatter field typing: string, date, datetime, integer, bool, enum, link, list
- Section structure enforcement: required sections, ordering, templates
- Bidirectional link validation across documents
- Round-trip markdown formatting (parse, validate, fix, serialize)
- Auto-managed sections with templates and content migration
- No hardcoded document types -- everything is a schema
- LSP server for editor integration
