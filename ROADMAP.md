# typedown Roadmap

This document explores future possibilities for typedown. For current implementation and development guidelines, see [AGENTS.md](AGENTS.md).

## Example Schemas -- Implemented

Four example schemas ship in `.typedown/`:
- `journal.yaml` -- date headings with `date_headings.sort`, month files with `from_date` title
- `agents.yaml` -- size warning, managed "Related Documents" section with template and `migrate_from`
- `readme.yaml` -- `from_directory` title, required `created` and `description` frontmatter
- `roadmap.yaml` -- fixed title, `strict_sections: false`, required "Non-Goals" with `intro_text`
These prove the schema system is expressive enough to replace meow's hardcoded types. Journal's date-based heading validation is handled by the `date_headings` schema feature. `td init` creates an empty `.typedown/` directory; it doesn't install examples yet.

## LSP Diagnostics -- Implemented

`td lsp` runs a Language Server Protocol server over stdio, publishing validation diagnostics. Designed for opencode integration.
**What's there:**
- `lsp-server` + `lsp-types` (same transport as rust-analyzer), synchronous message loop
- On `didOpen` / `didChange`: `check_dir()` on the nearest `.typedown/`-rooted project, publish diagnostics for all files, clear stale diagnostics
- Project root found by walking up to nearest `.typedown/` directory
- All diagnostics reported as ERROR (opencode filters to severity 1 only)
- Proper URI handling with percent-decoding (fixes meow's naive `strip_prefix`)
**opencode config:**
```json
{ "lsp": { "typedown": { "command": ["td", "lsp"], "extensions": [".md"] } } }
```
**Maybe -- features opencode doesn't currently use:**
- Diagnostic severity differentiation (WARNING for size/backlink issues, HINT for fixable cosmetic issues) -- only useful if opencode starts surfacing non-ERROR diagnostics
- Code actions from fixable diagnostics (`Fix::from_diagnostic` already exists)
- `textDocument/formatting` via the existing format pipeline
- Document symbols (H1/H2 headings as symbols)
- Single-file validation on `didOpen` with full-project on `didChange` (performance optimization, unnecessary at current scale)

## Link Repair

When a link is broken, try to fix it instead of just reporting it.
**Tiers of confidence:**
- **Auto-fix:** target filename exists but relative path depth is wrong (e.g., `../foo/README.md` from `journal/` should be `../../foo/README.md`). Rewrite the path.
- **Auto-fix:** target was renamed and old name matches exactly one file (e.g., `CLAUDE.md` renamed to `AGENTS.md`).
- **Suggest:** fuzzy matches like case mismatches (`readme.md` vs `README.md`). Report candidates, don't auto-fix.
- **Report:** no match found anywhere. Just broken.
**Considerations:**
- meow only reports broken links, never repairs. This is a new feature.
- Needs a search scope -- probably the repo root or the directory `td fmt` is pointed at.
- High-confidence fixes should be applied by `td fmt`. Suggestions should be unfixable diagnostics.

## Watch Mode

`td fmt --watch` for continuous formatting during development.
**Possible approaches:**
- inotify/kqueue file watching
- Debounced formatting on change
- Only re-validate changed files and their link targets

## Non-Goals

Explicitly out of scope to keep the project focused:
- **Project navigation** -- that's meow's job. Typedown doesn't know about monorepos or sparse checkouts.
- **Git commit interleaving** -- no git-aware rendering or timeline views. The `git2` crate is used internally for link validation (walking HEAD tree), but typedown does not surface git history.
- **CLI document rendering** -- no `td log` or timeline views. Typedown validates and formats, it doesn't display.
- **Opinionated defaults** -- no built-in types, no "you must have a README". Schemas define everything.
- **Non-markdown formats** -- markdown only. No RST, AsciiDoc, or HTML.
- **Encryption** -- meow implemented AES-256-GCM with GPG key wrapping; the complexity wasn't worth it in practice. Not planned for typedown.

## See Also

- [AGENTS.md](AGENTS.md) - Development guidelines and architecture
