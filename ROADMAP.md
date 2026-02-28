# typedown Roadmap

This document explores future possibilities for typedown. For current implementation and development guidelines, see [AGENTS.md](AGENTS.md).

## LSP Enhancements

The LSP server works for diagnostics. Possible next steps if opencode or other editors can use them:

- Code actions from fixable diagnostics (`Fix::from_diagnostic` already exists)
- `textDocument/formatting` via the existing format pipeline
- Document symbols (H1/H2 headings as symbols)
- Diagnostic severity differentiation (WARNING for size/backlink, HINT for cosmetic)

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
