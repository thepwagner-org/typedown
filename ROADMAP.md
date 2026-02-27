# typedown Roadmap

This document explores future possibilities for typedown. For current implementation and development guidelines, see [AGENTS.md](AGENTS.md).

## Example Schemas -- Implemented

Four example schemas ship in `.typedown/`:

- `journal.yaml` -- date headings with `date_headings.sort`, month files with `from_date` title
- `agents.yaml` -- size warning, managed "Related Documents" section with template and `migrate_from`
- `readme.yaml` -- `from_directory` title, required `created` and `description` frontmatter
- `roadmap.yaml` -- fixed title, `strict_sections: false`, required "Non-Goals" with `intro_text`
These prove the schema system is expressive enough to replace hardcoded types. Journal's date-based heading validation is handled by the `date_headings` schema feature. `td init` creates an empty `.typedown/` directory; it doesn't install examples yet.

## LSP Diagnostics -- Implemented

`td lsp` runs a Language Server Protocol server over stdio, publishing validation diagnostics. Designed for opencode integration.
**What's there:**

- `lsp-server` + `lsp-types` (same transport as rust-analyzer), synchronous message loop
- On `didOpen` / `didChange`: `check_dir()` on the nearest `.typedown/`-rooted project, publish diagnostics for all files, clear stale diagnostics
- Project root found by walking up to nearest `.typedown/` directory
- All diagnostics reported as ERROR (opencode filters to severity 1 only)
- Proper URI handling with percent-decoding
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

## Verbose / Debug Output

`td check` and `td fmt` currently produce no output on success. When validation silently passes on files that should fail, there's no way to tell if schemas were loaded, if presets were discovered, or if any files were scanned at all.

**Minimum useful output with `--verbose`:**

- Which `.typedown/` directory was found
- Which preset files were loaded (and from where)
- Which schemas are active (name, source, path patterns)
- Which files matched which schema
- Per-file check results (pass/fail with reasons)

**Why this matters:** Preset-based schemas (like the `readme` preset requiring `description` frontmatter and `title: from_directory`) can silently fail to load or match, and the user has no signal. This came up in practice -- `td check` passed on a README with a missing required field and a wrong title.

## Link Repair

When a link is broken, try to fix it instead of just reporting it.
**Tiers of confidence:**

- **Auto-fix:** target filename exists but relative path depth is wrong (e.g., `../foo/README.md` from `journal/` should be `../../foo/README.md`). Rewrite the path.
- **Auto-fix:** target was renamed and old name matches exactly one file (e.g., `CLAUDE.md` renamed to `AGENTS.md`).
- **Suggest:** fuzzy matches like case mismatches (`readme.md` vs `README.md`). Report candidates, don't auto-fix.
- **Report:** no match found anywhere. Just broken.
**Considerations:**
- Traditional linters only report broken links, never repair them. This is a new feature.
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

- **Project navigation** -- out of scope. Typedown doesn't know about monorepos or sparse checkouts.
- **Git commit interleaving** -- no git-aware rendering or timeline views. The `git2` crate is used internally for link validation (walking HEAD tree), but typedown does not surface git history.
- **CLI document rendering** -- no `td log` or timeline views. Typedown validates and formats, it doesn't display.
- **Opinionated defaults** -- no built-in types, no "you must have a README". Schemas define everything.
- **Non-markdown formats** -- markdown only. No RST, AsciiDoc, or HTML.
- **Encryption** -- the complexity of encryption (AES-256-GCM, GPG key wrapping) wasn't worth it in practice. Not planned.

## See Also

- [AGENTS.md](AGENTS.md) - Development guidelines and architecture
