# typedown-md

The markdown document layer behind [typedown](../typedown/): a lossless
parse/serialize round-trip, the AST it round-trips through, and the frontmatter
model that rides along with it. No schemas, no validation, no I/O.

Shared via cargo path dep — a consumer's `Cargo.toml` is the whole
declaration; the nix build sandbox derives its sibling closure from there.

## Usage

```rust
use typedown_md::parse::{parse, serialize};

let doc = parse("---\ntype: readme\n---\n# Title\n\nBody.\n");
assert_eq!(doc.frontmatter.unwrap().doc_type.as_deref(), Some("readme"));
assert_eq!(serialize(&doc), "---\ntype: readme\n---\n\n# Title\n\nBody.\n");
```

## How It Works

`parse` runs pulldown-cmark over the source and rebuilds it as `Block`s
carrying their source line numbers, then `serialize` writes markdown back out.
The round-trip is a fixed point for anything already formatted: `escape` keeps
the unescaped spelling whenever it re-parses intact, so a clean document comes
back byte-identical rather than sprouting backslashes.

Frontmatter is `type: Option<String>` plus an `IndexMap` for everything else,
so key order survives, along with a key-to-line map for diagnostics.

## Features

- **Lossless round-trip**: parse then serialize is a fixed point for formatted documents
- **Minimal re-escaping**: escapes go back only where a character would parse as markup again
- **Line numbers on blocks**: diagnostics can point at the source without a parallel array
- **Frontmatter model**: ordered keys, block scalars preserved, key-to-line map
- **YAML scalar quoting**: `needs_yaml_quoting` and friends are public, so other writers can match `td fmt` byte for byte
