# RULE.md – Wiki Design System

This file defines the structure, design and conventions of the wiki so it can be
replicated in any other TontooOS repository. When creating a wiki in a new repo,
follow this document exactly – the goal is that every TontooOS wiki looks and works
the same.

> This file itself is part of the wiki. It is the contract for how wiki pages are
> written, structured and maintained.

---

## 1. General Rules

| Rule | Value |
|---|---|
| Language | English only (per AGENTS.md: "Only Englisch in TontooOS Repos") |
| File format | Markdown (`.md`), GitHub Flavored Markdown |
| Encoding | UTF-8, LF line endings |
| Tabs / spaces | 2 spaces for code indentation inside markdown |
| Line width | Wrap prose at ~100 characters |
| Emojis | Never (no emojis in any file) |
| Images | Only if required; keep diagrams as text/tables/code |
| Version | Last edit date + short change note in `## Changelog` of MAIN.md (if kept) |

## 2. Wiki File Structure

Every wiki lives in a `wiki/` directory at the repository root.

```
<repo>/
├── wiki/
│   ├── MAIN.md            # Entry point: overview + index of all features
│   ├── RULE.md            # This file: wiki design system + repo rules
│   ├── <Feature>.md       # One page per feature, PascalCase names
│   ├── <Feature>.md       # ...
│   └── AGENTS.md          # NOT included (never copy AGENTS.md into the wiki)
├── src/                   # code
├── Headers/               # C headers (if FFI exists)
└── lang/                  # language files (if i18n exists)
```

Rules:

1. `MAIN.md` is the only entry point. It must be linked from the repo `README.md`.
2. Each feature gets **exactly one** page. No subfolders unless a feature is so
   large it needs its own sub-wiki (then `MAIN.md` links to the sub-wiki's `MAIN.md`).
3. `AGENTS.md` must never be copied into the wiki folder.
4. File names are PascalCase (`LangStore.md`, not `langstore.md` or `lang-store.md`).

## 3. MAIN.md – The Entry Page

`MAIN.md` is the wiki homepage. Structure, in this exact order:

```
# <ProjectName> – Wiki

<1-2 sentence description of the project>

- Repository: <url>
- License: <name>
- Version: <version>

## Feature Index

| Feature | File | Description |
|---|---|---|
| Main index | [MAIN.md](MAIN.md) | This page |
| Rules | [RULE.md](RULE.md) | Development and usage rules |
| <Feature> | [<Feature>.md](<Feature>.md) | <short description> |

## Quick Start

<minimal working example: Rust code, C code, or both>

See [<Feature>.md](<Feature>.md) for details.
```

Rules:

- The `## Feature Index` table always starts with `MAIN.md` and `RULE.md` rows.
- Every feature page must appear in the table; no orphan pages.
- `Quick Start` shows the shortest possible "hello world" of the library (init +
  one call), in Rust and C if both exist.
- All internal links are relative (`[LangStore.md](LangStore.md)`).

## 4. Feature Pages

Each feature page documents one capability of the library. Structure:

```
# <Feature Name>

<One paragraph: what the feature does and why it exists.>

## <Section: e.g. API, Constructors, Functions, Rules>

<prose>

```<lang>
<code example>
```

## <Next Section>

<...>

## Usage / Example

<end-to-end example>

## Cross References

- [RelatedPage.md](RelatedPage.md) – why
```

Rules:

- Start with `# Title` (H1) matching the file name.
- Use `##` for main sections and `###` for subsections. Never skip heading levels.
- Every Rust struct, function or macro gets its own `###` section with:
  - Signature in a fenced code block (language `rust`)
  - Short description of behavior
  - Return values / error conditions as a bullet list or table
  - Minimal code example
- Document **behavior as implemented**, including quirks (e.g. "invalid files are
  skipped silently", "returns debug string of the key").
- Mention error behavior explicitly: `Returns Err when ...`, `Returns None when ...`.
- C FFI pages use a `| Return | Meaning |` table for every function with numeric
  return codes.
- Include a `## Cross References` section at the end linking to related pages.
- Code examples must be syntactically valid and match the real API.

## 5. Tables

Tables are the primary way to present structured data (API overviews, return codes,
file formats, rules).

```markdown
| Field | Type | Description |
|---|---|---|
| `lang` | `string` | The language code, e.g. `"en_us"` |
```

Rules:

- Header row + separator row + at least one data row.
- First column: identifiers in backticks; keep them short.
- Align columns by padding (GFM renders any alignment).
- Keep descriptions concise; move long prose below the table.

## 6. Code Blocks

- Always set a language: `rust`, `c`, `json`, `toml`, `markdown`, `bash`.
- Rust code blocks are the primary form; use `rust`.
- Paths and file names in prose are written in backticks: `./lang/en_us.json`.
- JSON examples must be valid JSON and mirror real files where possible.
- Do not add `$` prompts to bash examples.

## 7. Links, Inline Code and Emphasis

| Element | Markdown | Note |
|---|---|---|
| Wiki-internal link | `[Page.md](Page.md)` | Relative paths, no `./` |
| External link | `[text](https://...)` | Only for repo/GitHub URLs |
| Code / identifier | `` `accessibility_init` `` | All function names, fields, paths |
| Key names | `t!("app.title")` | Inside code blocks or backticks |
| Emphasis | `**strong**` | For rules and warnings; never `*italic*` alone for emphasis |
| Warnings | `> **Note:** ...` | Blockquote with bold label |

## 8. Language Conventions

- All content in English, American spelling.
- Third person, imperative for rules ("Create a `lang/` folder").
- No slang, no emojis, no abbreviations (`e.g.`, `i.e.` are allowed).
- Use the real symbol names from the code, verbatim (never rename or paraphrase
  identifiers).

## 9. Creating a New Feature Page – Checklist

Use this checklist when adding a new feature to a repo and its wiki:

1. `src` change is implemented, compiled (`wsl -d archlinux` or `cargo check`)
   and tested.
2. Create `wiki/<FeatureName>.md` following the structure in section 4.
3. Add the row to the `## Feature Index` table in `wiki/MAIN.md`.
4. Link related pages in the `## Cross References` section.
5. If the feature is exported to C, update `Headers/accessibility.h` and the
   `## Memory Rules` table on the FFI page.
6. Update `RULE.md` only when the design system itself changes.

## 10. Replicating This Wiki in Another Repo

To create a wiki for a new TontooOS repository:

1. Create the `wiki/` folder.
2. Copy `MAIN.md` and `RULE.md` from an existing TontooOS wiki.
3. Rewrite `MAIN.md`:
   - Project name + description (section 3)
   - Repository / license / version bullets
   - Feature table: one row per actual feature of the new repo
   - Quick Start matching the new library
4. Create one feature page per public capability of the new repo (section 4).
5. Delete feature pages from the copy that do not exist in the new repo.
6. Keep `RULE.md` verbatim – it is the shared design system.
7. Link `wiki/MAIN.md` from the repo `README.md`.

## 11. Do's and Don'ts

| Do | Don't |
|---|---|
| One page per feature | Combine multiple features in one page |
| Document errors/return values | Assume success cases only |
| Link with relative paths | Use absolute repo URLs for internal pages |
| Keep `RULE.md` stable | Modify it per repo |
| Match the code exactly | Invent syntax or wrappers not in the source |
