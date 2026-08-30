# Typedown Schema Authoring

Schemas live in `.typedown/<typename>.yaml`. The filename stem is the
type name. Documents are matched to schemas by:

1. `type: typename` in frontmatter, or
2. `paths:` glob patterns in the schema (relative to the project root).

This guide covers engine behaviour and authoring conventions that the
JSON Schema can't express. For the structural reference — every field
and its type — run `td schema`, which prints the json-schema baked into
the binary.

**Every schema declares `version: 2`, and that is the only version
there is.** Frontmatter is a literal json-schema, so patterns, ranges,
nested objects and unions all come for free, and the same block doubles
as a structured-output constraint when an agent generates the
frontmatter.

A schema that declares no `version:` at all fails to load, naming the
file. It is not read as an implicit version 2: an unversioned schema is
a version 1 leftover, and version 1's `fields:` block is now an unknown
key — reading the file as version 2 would drop every requirement in it
and leave a type that silently validates nothing. A leftover `fields:`
block is rejected outright for the same reason.

The file this rule is for is a stale preset in
`~/.config/typedown/presets/`. It is reported against the presets
directory rather than any document, because no document names it — a
broken preset doesn't error where it is used, it just stops typing the
files it was supposed to type.

Don't add a `# yaml-language-server:` modeline. It only worked when the
typedown repo sat at a fixed relative path from the project, and `td fmt`
strips it. `td schema` and `td check` cover the same ground without a
path on disk.

## Worked example

```yaml
version: 2                                     # Frontmatter is a literal json-schema
description: A person reference document       # Documentation only; ignored by engine

paths:
  - "people/*.md"                              # Relative to .typedown/ parent (project root)

frontmatter:                                   # Literal json-schema, validated as-is
  type: object
  properties:
    scope:
      type: string
      enum: [family, friend, professional]
    met:
      type: string
      format: date                             # ISO 8601: YYYY-MM-DD
    tags:
      type: array
      items:
        type: string
    age:
      type: integer
      minimum: 0
  required: [scope]

correspondence:                                # Ties data to prose, both ways
  - each: links-in-section "## Relationships"
    requires:
      target_type: person
      backlink-in: "## Relationships"

structure:
  title: from_filename                         # H1 must match filename stem
  strict_sections: true                        # Default. Unlisted H2s rejected.
  size_warning: 8000                           # Warn if file exceeds N bytes

  intro:                                       # Content between H1 and first H2
    template: "- **Text**: Text"               # template implies bullets: unordered

  sections:                                    # H2 sections; order enforced
    - title: Relationships
      description: Links to related people     # LLM guidance; ignored by engine
      required: true
      template: "- [Name](Name.md) - context"  # links: is the section-level
                                               # spelling of the rule above

    - title: Components
      bullets: unordered
      properties:                              # Typed sub-items per list item
        make:                                  # Keys MUST be lowercase in schema
          type: string
          required: true
        quantity:
          type: integer
          required: true
        category:
          type: enum
          required: true
          values: [storage, compute, networking, memory, power]

    - title: Related Documents
      managed_content:                         # Auto-managed by `td fmt`
        template: |
          ## Related Documents

          - **people/** - Other people
        merge: upsert                          # default; `replace` to clobber
        migrate_from:
          - Links
          - See Also
```

## Frontmatter: json-schema

A schema describes its frontmatter with the `frontmatter:` block, which
is a literal json-schema (draft 2020-12) — whatever you write there is
handed to a real validator unchanged.

- **The validated instance is the frontmatter object with `type:` folded
  back in.** So `type` can be constrained like any other property
  (`type: {const: person}`), and `additionalProperties: false` needs a
  `type` entry or it will reject every document.
- **Violations become ordinary typedown diagnostics**, anchored to the
  line of the offending top-level frontmatter key. Nested paths are
  reported as `files[0].name`.
- **Dates are ISO 8601 only.** `format: date` is `YYYY-MM-DD` (with real
  calendar validation — `2026-02-30` fails); `format: date-time` is RFC
  3339, so it wants the `T` separator, seconds, and an offset:
  `2026-03-04T14:30:00Z`. Loose spellings (`2026/03/04`, `March 4,
  2026`, `2026-03-04 14:30`) are not accepted; use `pattern:` instead of
  `format:` if you genuinely need a different shape. Format assertion is
  switched on; in stock draft 2020-12 `format` is an annotation and
  would silently accept anything.
- **Property declaration order sets frontmatter key order.** `td fmt`
  writes `type:` first, then the `properties:` keys in the order the
  schema lists them, then anything else in the order the document
  already had it.
- **`td json` coerces from the json-schema**: `type: integer` turns
  `"42"` into `42`, `type: boolean` turns `yes` into `true`, and arrays
  and nested objects recurse through `items` / `properties`.
- **A json-schema doesn't accept a bare YAML null.** A field that should
  still accept an empty value needs `type: [string, "null"]`.
- **`fields:` is a load error.** It was version 1's spelling and the
  engine no longer reads it; json-schema is the single source of truth.

## Correspondence rules

json-schema validates your data and `structure:` validates your prose;
`correspondence:` is what keeps them pointing at each other. Each rule
is a quantifier plus an obligation — *each* of these things *requires*
that:

```yaml
version: 2
correspondence:
  - each: frontmatter.files[]                  # data → prose
    description: Every file record gets prose describing it.
    requires:
      subsection-under: "## Files"
      heading: "### {quality}"

  - each: subsections-under "## Files"         # prose → data
    requires:
      frontmatter-item: files[].quality

  - each: frontmatter.species[]                # data → a link
    requires:
      link-in: "## Species"
      target_type: species

  - each: links-in-section "## Species"        # that link's inverse
    requires:
      frontmatter-item: species[]

  - each: links-in-section "## Cast"           # prose → prose
    requires:
      target_type: personality
      backlink-in: "## Movies"

  - each: docs-of-type tvseason in-directory "." # disk → prose
    requires:
      link-in: "## Seasons"
```

Four selectors — a frontmatter path is one of them whether or not it
ends in `[]` — and each takes its own `requires:` keys. Pairing a
requirement with a selector that can't act on it is a schema load
error, not a silently ignored knob.

| `each:` | the set | `requires:` |
| --- | --- | --- |
| `frontmatter.<array>[]` | every item of a frontmatter array | `subsection-under` + `heading`, or `link-in` (+ optional `target_type`) |
| `frontmatter.<field>` | the one value of a scalar field | the same two forms |
| `subsections-under "## S"` | every heading one level below `## S` | `frontmatter-item` |
| `links-in-section "## S"` | every relative link in `## S` | `target_type`, `backlink-in`, `frontmatter-item`, or any combination |
| `docs-of-type T in-directory "D"` | every `T` document that exists in `D` | `link-in` |

- **Section references carry their marker.** `"## Files"` says level
  and text at once, and `heading: "### {quality}"` has to be deeper
  than the `subsection-under` it lives in. Bare `"Files"` is an error.
- **`{field}` interpolates from the item.** `### {quality}` renders
  `quality: 4k` as `### 4k`; literal text around it is kept, so
  `### Copy {n}: {quality}` works too. Use `{.}` when the array holds
  scalars rather than records. An item whose placeholder has no scalar
  value is reported against the item (`files[0]`), not the prose.
- **The two directions are separate rules.** `frontmatter.files[]`
  catches data with no prose; `subsections-under "## Files"` catches
  prose with no data. Declare both to close the loop, or just one when
  only one direction matters.
- **An absent or empty array is vacuous, not violated.** A rule over
  `files[]` says nothing about a document that has no `files:`. Make
  the field `required:` in the json-schema if it must be there.
- **Only headings inside the container count.** A `### 4k` under
  `## Details` is invisible to a rule scoped to `## Files`, in both
  directions. The span ends at the next heading of the same or a
  shallower level.
- **A frontmatter path names an array or a scalar.** `species[]` is the
  items of a list; a bare `location` is the single value, which the
  rules read as a set of one. Both spellings work wherever a path is
  asked for, so a document with one location and a document with
  several species differ in their data, not in their rules.
- **Nothing here is auto-fixed.** A missing subsection needs prose only
  a human or an agent can write, and an orphaned subsection may be the
  half that's right. Violations stay diagnostics with line numbers;
  `td fmt` reports them and leaves the document alone. That holds for
  `docs-of-type` too: where the link goes in the section is an editorial
  call, so it names the file and leaves the document alone.

`docs-of-type` is the only selector that starts from the files that
exist rather than from what the document says. Every other rule walks
outward from the document, so a section listing three of four seasons
is clean — each link it does have is valid. This one asks the opposite
question:

```yaml
  - each: docs-of-type tvseason in-directory "."
    requires:
      link-in: "## Seasons"
```

- **The scope is a directory, relative to the document.** `"."` is the
  document's own directory — its siblings — which is the shape this is
  for: a container README beside the documents it indexes. `"seasons"`
  looks in a subdirectory. Absolute paths are a load error, and the
  scope keyword isn't optional: an unscoped `docs-of-type` would have to
  mean the whole project, which is a different rule with a different
  cost.
- **`in-directory` is that directory only; `under-directory` descends.**
  Siblings by default, so a `Season 1/` subdirectory of episode
  documents doesn't leak into a rule about seasons. Spell
  `under-directory "."` when children nested at any depth should count.
- **Type, not filename, decides membership.** A file is in the set when
  its resolved type — frontmatter `type:` or a `paths:` match — is the
  named one. Files under no schema at all are invisible, since the type
  resolution that finds them never ran.
- **The document never has to link itself.** A rule whose type matches
  the document declaring it skips the document itself, so a type can
  index its own kind.
- **An absent section is still a violation.** Empty `## Seasons` with a
  season doc beside it reports, and so does no `## Seasons` at all — the
  point is naming the file nobody linked, and the diagnostic anchors to
  the heading only when there is one.

`links-in-section` is where `links:` moves to in a v2 schema.
`target_type` means the same thing it always did. `backlink-in` names
the inverse section outright, where `bidirectional: true` had to go
looking for one in the target's schema — so it also states which
section to add the backlink to when it's missing, and it takes a list
when a type can answer from any of several:

```yaml
  - each: links-in-section "## Leads To"
    requires:
      target_type: threat
      backlink-in: ["## Enabled By", "## Leads To"]
```

The section-level `links:` spelling keeps working — move a section to a
correspondence rule when you want the inverse direction checked too, not
merely because the rule is newer.

`subsection-under` asks for prose *about* a frontmatter value. When the
value is itself a document, what you want instead is a pointer *at* it —
`species: [Blue Jay]` in frontmatter and `[Blue Jay](../species/Blue%20Jay.md)`
under `## Species`, kept in agreement both ways:

```yaml
  - each: frontmatter.species[]                # every value has its link
    requires:
      link-in: "## Species"
      target_type: species

  - each: links-in-section "## Species"        # every link has its value
    requires:
      frontmatter-item: species[]
```

- **A link answers a value when the target's filename matches it.** The
  URL is percent-decoded first, so `../species/Blue%20Jay.md` answers a
  frontmatter `Blue Jay` — neither side has to know how the other
  spells a space. The link text is never consulted; the file is the
  identity.
- **`target_type` narrows what counts as an answer.** With it, a link
  whose target resolves to another type doesn't satisfy the value even
  when the filename matches, which is what keeps `## Species` pointing
  at species documents rather than at a place that happens to share a
  name. Without it, any relative link with the right filename does.
- **`link-in` and `subsection-under` are two different obligations.** A
  rule declares one or the other; asking for both in one rule is a load
  error. Write two rules when a value needs both a link and prose.
- **The inverse is a `links-in-section` rule.** `frontmatter-item:` on
  it names the path each link's target has to appear in, and it
  composes with `target_type` and `backlink-in` on the same rule.
- **The path may reach into each record.** `frontmatter.species[].name`
  is the field of each item the link must answer, the same spelling the
  subsection direction takes.
- **Absent and empty stay vacuous, in both directions.** No `species:`
  key, an empty list, or a `location:` with nothing after it says
  nothing about the prose. A missing *section* with data to place is
  still reported — once for the section, not once per value.

## Type resolution

Each document gets exactly one type, resolved in this order:

1. `type: none` in frontmatter — opts out of validation entirely.
2. Explicit `type: typename` in frontmatter — always wins over `paths:`.
3. Path-pattern match — schemas' `paths:` globs are tested against the
   file path relative to the project root (the `.typedown/` parent).
4. No match — file has unknown type; no validation applied.

Path-matched files can omit frontmatter entirely if the schema has no
required fields. The `type:` field is never required for path-matched
files.

If two schemas' `paths:` patterns both match a file at runtime, a
conflict diagnostic is emitted. If two schemas declare the *same exact
pattern string*, schema loading fails outright.

## Managed sections

`managed_content:` hands a section over to `td fmt`: the template is the
canonical wording, and the formatter writes it back into the document.
`merge:` decides what happens to the content the section already has.

- **`merge: upsert` (default)** — the template's entries are inserted
  and normalised; anything else the section holds is preserved.
  List items are paired by *identity*, not position: the first link URL
  in the item, else its first bold run or code span, else its plain
  text, compared case- and whitespace-insensitively. An item the
  template declares is rewritten to the template's wording wherever it
  sits; an item the template has never heard of survives and is
  appended after the templated ones, in the order the document had it.
  Blocks the template doesn't account for at all (prose, a second list)
  are appended after the managed blocks.
- **`merge: replace`** — the section becomes the template and nothing
  else. Curated additions are deleted. Use it only for sections that
  are wholly generated.

Preserving is the default on purpose. Note what upsert already does to
drift: an entry whose *identity* the template declares is rewritten to
the template's wording however far its text has wandered, so a stale
`- **journal/** - Daily notes in YYYY-MM.md files` becomes the current
sentence. Only an entry the template has never named survives untouched.

For those, the two failure modes are not symmetric: a bullet that
survives is stale text somebody can read and delete, while a bullet that
gets clobbered is authored content nobody can recover from the
formatter. `td fmt` runs unattended over whole trees, so it defaults to
the recoverable failure. Drift removal keeps its own affordances, and
they are deliberate per-schema opt-ins rather than a formatter default:
`merge: replace` for a section that is wholly generated, and
`migrate_from:` for one that was renamed. Retiring a bullet the template
never declared is an editing job, not a formatting one.

So a template of

```yaml
managed_content:
  template: |
    ## Related Documents

    - **README.md** — what this project is and why.
    - **GOALS.md** — goals and non-goals.
```

applied to a section reading

```markdown
## Related Documents

- **GOALS.md** — Committed goals and explicit non-goals.
- **characters/** — one doc per character.
```

yields the two templated bullets in template order, then
`**characters/**` untouched. Under `merge: replace` the `characters/`
bullet would be gone.

`migrate_from:` lists legacy H2 titles for the same section. The first
one found is renamed to the section's title and its list merged as
above; any further legacy sections in the span are preserved verbatim
underneath rather than dropped.

`scope:` decides which of the type's documents the managed section
applies to.

- **`scope: any` (default)** — every document the type matches.
- **`scope: root`** — only a document sitting directly in the schema
  root (the `.typedown/` parent). Nested matches keep whatever they
  authored: the section is neither injected nor rewritten there.

Reach for `root` when the type's glob is deliberately recursive but the
template names project-root paths. `agents.yaml` is the case it exists
for: `**/CLAUDE.md` is right — per-directory instruction files are a
real convention and want the `size_warning` — but a `Related Documents`
list naming `README.md`, `GOALS.md`, `tasks/` and `journal/` is wrong
anywhere but the root. Narrowing the glob instead would have given the
nested files no schema at all; `scope: root` narrows only the part that
was actually root-shaped.

## Built-in presets

`td` ships with presets compiled into the binary: see `td preset` for
the list and `td preset <name>` to print one. These apply automatically
to every project — `README.md` → `readme`, `journal/*.md` → `journal`,
and so on. Override a preset by placing a same-named schema in the
project's `.typedown/` or in `~/.config/typedown/presets/`.

Every preset is `version: 2`, and so is every override — an override
that omits `version:` fails to load rather than shadowing the preset
with a type that validates nothing.

Presets for a project's singleton documents (`readme`, `goals`,
`roadmap`) anchor their globs at the schema root — `README.md`, not
`**/README.md`. Their `title:` modes rewrite the H1, which is what you
want for the project's own README and is destructive anywhere else.

`agents` is the exception, and stays recursive: per-directory
`AGENTS.md` / `CLAUDE.md` files are a real convention, and its
`size_warning` is exactly what they want. Only its `Related Documents`
section was root-shaped, so that carries `scope: root` rather than the
whole type being narrowed.

## Discovery

`td` walks parent directories for `.typedown/` and loads every `*.yaml`
inside as a type definition. Load order: built-in presets first, then
overlaid by XDG presets, then by project `.typedown/`. Patterns are
relative to the `.typedown/` parent directory (the project root).

## Document compliance

A document passes validation when:

1. Its type resolves (either by `type:` frontmatter or `paths:` match).
2. H1 satisfies the `title` mode (matches filename for `from_filename`,
   matches the parent directory ignoring case for `from_directory`, etc.).
3. Frontmatter satisfies the type's `frontmatter:` json-schema.
4. All `required: true` sections are present; no unlisted H2s when
   `strict_sections: true`.
5. Sections appear in the schema-defined order.
6. Sections with `bullets:` set (or a `template:`) contain only bullet lists.
7. Each bullet matches the section `template` when defined.
8. Links in sections with `links:` point to documents of the declared
   `target_type`.
9. Targets of `bidirectional: true` links link back.
10. In `properties:` sections, every top-level list item provides
    sub-items for all required properties, parseable as `Key: Value`
    (split on first `: `). Values type-validate the same as frontmatter.
11. Every `correspondence:` rule holds: each frontmatter item named by
    a rule has its subsection or its link, each subsection and each
    link has the frontmatter item behind it, and each link in a
    constrained section has the right `target_type` and its
    `backlink-in`.

## Properties variant

For sections where every top-level list item has the same set of named
sub-fields:

```markdown
## Components

- Kingston 32GB DDR5-4800 ECC UDIMM
  - Make: Kingston
  - Model: KSM48E40BS8KM-32
  - Category: memory
  - Quantity: 4
- Raspberry Pi 5 8GB
  - Make: Raspberry Pi Ltd
  - Model: SC1112
  - Category: compute
  - Quantity: 1
```

- Sub-items are parsed by splitting on the first `: ` (colon-space).
- **Schema keys must be lowercase. Document text can be any case.**
  `Size:`, `SIZE:`, and `size:` all match schema key `size`. A schema
  key `Size:` will never match anything.
- `td json` emits a typed `properties` object on each list item,
  alongside the raw `items` array.
- **Section `properties:` are typed with typedown's own field types**,
  not json-schema: `integer`, `float`, `bool`, `date`, `datetime`,
  `string`, `enum`, and `list` (with `item_type:`). This is deliberate
  and is the one place those types survive the removal of version 1.
  A section's properties describe `Key: Value` pairs inside a list item
  — prose, parsed out of markdown — so there is no json instance for a
  json-schema to bind to, and `format: date` has nothing to validate.
  Converting them would be a separate job with a separate design; until
  then, `frontmatter:` is json-schema and `properties:` is not.
- `enum` still requires a non-empty `values:`, and `list` an
  `item_type:`; both are checked at schema load.

## Journal / date-headings variant

For documents where every H2 is a date (journals, changelogs):

```yaml
description: Monthly journal. H1 from filename; H2s are date entries.

structure:
  title: from_date          # '2026-02.md' → '# February 2026'
                            # '2026-04-14.md' → '# April 14, 2026'
  date_headings:
    sort: newest_first      # or: oldest_first
```

`date_headings` and `sections` are mutually exclusive.

## Gotchas

- **A missing `version: 2`, or a leftover `fields:` block.** Both are
  hard errors at schema load, naming the file. Neither is treated as an
  implicit version 2, because a version 1 schema read that way would
  enforce nothing at all.
- **`additionalProperties: false` without a `type` property.** The
  validated instance always carries `type:`, so a closed schema that
  doesn't declare it rejects every document.
- **Unknown `format:` names are silently ignored.** Formats are the
  `jsonschema` crate's own implementations — `date`, `date-time`,
  `email`, `uri` and friends. A typo like `format: dat` asserts nothing
  rather than erroring.
- **`enum` with no `values`, or `list` with no `item_type`**, in a
  section's `properties:`. Hard error at schema load — `td` won't start.
  A `list` of `enum` requires both `item_type: enum` and a `values:`
  list.
- **`paths:` globbing.** `*` matches within a single directory; `**`
  crosses directories. `*.md` does NOT match `foo/bar.md` — use
  `**/*.md` for recursive.
- **`strict_sections` defaults to `true`.** Unlisted H2s are rejected
  by default. Set `strict_sections: false` to allow freeform sections.
- **`template:` implies `bullets: unordered`.** A section with a
  template can only contain unordered bullets.
- **A prose-only `template:` validates nothing.** `- Fact about the
  movie` compiles to the list marker plus a free-text wildcard, which
  every bullet matches. `td` reports it at schema load; write the format
  you mean (`- **Text**: Text`, `- [Name](url) - Text`, `- YYYY-MM-DD -
  Text`) and put the guidance in `description:`.
- **Paragraphs are the default content type.** Sections allow any
  content (prose, code, etc.) unless `bullets:` is set or a `template:`
  is declared.
- **Unrecognised `title:` values become fixed text.** Special modes are
  `none`, `from_filename`, `from_directory`, `from_date`, and
  `required`. Anything else (e.g. `Roadmap`, or a typo
  like `form_filename`) becomes a fixed H1 that's auto-created if
  missing — no error.
- **`managed_content.template` must include the `## Heading` line.**
  Templates that omit the heading fail validation every time.
- **`managed_content` preserves by default.** `merge: upsert` keeps the
  entries the template doesn't declare; `merge: replace` is the opt-in
  that throws them away. Reach for `replace` only when the section is
  wholly generated. Upsert rewrites the entries the template names, but
  will never delete one it doesn't — that is the intended trade, not an
  oversight.
- **A `**/`-prefixed `paths:` glob plus a rewriting `title:` mode eats
  authored H1s.** `**/README.md` with `title: from_directory` claims
  every nested README and renames each one after its directory. Anchor
  singleton-document globs at the schema root instead.
- **A recursive `paths:` glob plus a root-shaped `managed_content`
  invents sections.** The template describes the project root, so every
  nested match gets a section listing paths that don't exist beside it.
  Set `scope: root` on the `managed_content` — that keeps the rest of
  the type (size warnings, section rules) applying everywhere.
- **`from_directory` ignores case; `from_filename` doesn't.** A
  directory name is a filesystem slug, so it constrains which title the
  H1 states, not its capitalisation — `bridge/README.md` keeps
  `# Bridge`. A filename under `from_filename` carries the authored
  casing itself, so it still has to match exactly.
- **One correspondence rule only checks one direction.** `each:
  frontmatter.files[]` never notices a subsection with no record
  behind it; that takes the matching `each: subsections-under` rule.
  Declaring only the data→prose half is a common way to end up with
  orphaned prose nobody flags.
- **A `heading:` with no `{field}` is rejected.** It would demand the
  same heading of every item, which is a `required: true` section, not
  a correspondence. Same for a `subsection-under` that isn't shallower
  than its `heading`.
- **`requires:` keys are hyphenated, and typos are errors.**
  `subsection-under`, `frontmatter-item`, `backlink-in`, `link-in` — the
  underscore spellings fail to load rather than being ignored, as does
  any requirement the selector can't act on (`backlink-in` under a
  `frontmatter.<path>` rule, `target_type` under one with no `link-in`,
  `heading` under `links-in-section`). The error names what the
  selector does take.
- **A section full of valid links can still be incomplete.**
  `links-in-section` only judges the links that are there, so a
  `required:` section listing three of four seasons passes. Add the
  matching `docs-of-type` rule when the section is meant to be
  exhaustive of what's on disk.
- **`properties` keys must be lowercase.** Document text is normalised
  before matching; schema keys are not. The most common silent-failure
  schema bug.
- **`structure.frontmatter` was removed.** Unknown keys under
  `structure:` are rejected. Frontmatter validation lives at the top
  level, under `frontmatter:`, whose `properties:` declaration order
  also controls serialization order.
