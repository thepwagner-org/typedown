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
paths: ["**/README.md"]

structure:
  title: from_directory          # H1 must match directory name
  sections:
    - title: Usage
      required: true
    - title: How It Works        # you are here
    - title: Features
      required: true
      bullets: true
      template: "- **Text**: Text"
```

This README complies with that schema. Run `td fmt` and typedown validates structure, then auto-fixes what it can: inserting missing titles, reordering sections, and reformatting.

## Features

- **Frontmatter fields**: string, date, integer, float, bool, enum, link, list
- **Bullet templates**: validate list items against patterns (like this section)
- **Managed sections**: auto-generated content from templates
- **Link validation**: with optional type constraints across documents
- **Auto-fix**: `td fmt` fixes titles, section order, date entry sorting
- **LSP and presets**: diagnostics on open/change, XDG presets for shared schemas
