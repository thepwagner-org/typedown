# typedown

Like TypeScript adds types to JavaScript, typedown adds types to markdown. Define schemas in `.typedown/*.yaml` and typedown validates frontmatter, section structure, link integrity, and formatting — then auto-fixes what it can.

## Usage

```bash
td fmt              # validate, fix, and format
td check            # validate without writing (for CI)
td lsp              # start language server (LSP over stdio)
```

## How It Works

Create a schema in `.typedown/readme.yaml`:

```yaml
description: Project README
paths:
  - "**/README.md"

fields:
  description:
    type: string
    required: true
  created:
    type: date
  github:
    type: string

structure:
  title: from_directory
  sections:
    - title: Usage
      required: true
    - title: How It Works
      required: true
      paragraph: true
    - title: Features
      required: true
```

You're reading a document that complies with this schema. Here's what each piece enforces:

**`paths: ["**/README.md"]`** matches this file by path — no `type:` field in the frontmatter. Typedown resolves types by explicit `type:` first, then falls back to path patterns.

**`fields`** validates the frontmatter above: `description` is required, `created` must parse as a date, `github` is an optional string. Other field types: `integer`, `bool`, `enum`, `datetime`, `link`, `list`.

**`title: from_directory`** enforces that the H1 matches the directory name — `typedown/` becomes `# typedown`. Other modes: `from_filename`, `from_date`, a fixed string like `"My Project"`, or `none`.

**`sections`** requires Usage, How It Works, and Features in that order. Sections default to bullet-list content; `paragraph: true` on How It Works allows the prose you're reading now. With `strict_sections` (the default), any unlisted H2 is an error.

Run `td fmt` and typedown validates all of this, then auto-fixes what it can: inserting missing titles, reordering shuffled sections, removing empty optional sections, and reformatting the markdown.

## Features

- Schema-driven validation via `.typedown/*.yaml` — no hardcoded document types
- Frontmatter field types: string, date, datetime, integer, bool, enum, link, list
- Path-based type matching — files are typed by glob pattern without `type:` in frontmatter
- Section enforcement: required/optional sections, strict ordering, bullet vs. prose mode
- Bullet templates: validate list items against patterns like `- [Name](path.md) - YYYY-MM-DD`
- Managed sections: auto-generated from templates with legacy section migration; user content below the template is preserved
- Link validation: every local `[text](path.md)` link checked for existence
- Link constraints: restrict links in a section to a target document type, with optional bidirectional enforcement
- Date headings: H2s as `YYYY-MM-DD` with chronological sort — `td fmt` reorders entries
- Auto-fix: titles, section order, empty sections, managed content, date entry sorting
- Round-trip markdown formatting (parse, validate, fix, serialize)
- LSP server with diagnostics on open and change
- XDG presets for shared schemas across projects
