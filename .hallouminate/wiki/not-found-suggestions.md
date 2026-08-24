# Not-found suggestions

Two of hallouminate's most common `InvalidParams` misses — an unknown corpus
name and a `read_markdown` path that doesn't exist — carry their own recovery
hints, so a caller (usually an LLM) fixes the mistake **without an extra
round trip**. This shipped in the 0.7.0 window (CHANGELOG "Errors: unknown-corpus
errors enumerate configured corpora with a closest-match suggestion;
`read_markdown` misses list the nearest existing directory plus top fuzzy path
matches"). The ranking is fuzzy string similarity via `strsim`.

## Unknown corpus → list + "did you mean"

`pick_corpus` (`crates/hallouminate-domain/src/corpus/sandbox.rs::pick_corpus`)
does not just fail with the bad name. When a named corpus isn't found it ranks
every configured corpus by `strsim::jaro_winkler` against the requested name and
returns:

```
corpus "multiplier-wiki" not found; configured: repo:multiplier:wiki, repo:dotfiles:wiki — did you mean repo:multiplier:wiki?
```

The full configured list is always included (the caller may have wanted a
different one); the single closest match is the "did you mean" nudge. The empty
and single-corpus cases keep their own messages ("no corpora configured…",
resolve-the-only-one).

## Read-path miss → nearest directory + closest filenames

`enrich_read_not_found`
(`crates/hallouminate-daemon/src/dispatch.rs::enrich_read_not_found`) intercepts
**only** the bare `<path> does not exist` read miss — a symlink rejection, an
unsafe-path error, or delete wording passes through untouched, because those are
not "you typed the wrong path" mistakes. For a genuine miss it appends two
things:

- **Ancestor directory listing** — the nearest existing directory's entries, via
  `read_miss_ancestor_listing` / `describe_read_miss_dir`, capped at
  `READ_MISS_LISTING_CAP` (20) with an `… and N more` tail. This orients a
  caller who got the directory right but the filename wrong.
- **Closest filename matches** — `read_miss_closest_matches` ranks every corpus
  markdown file by `strsim` similarity between its filename *stem* and the missing
  path's stem, skips anything the directory listing already showed, and appends
  the top `READ_MISS_SUGGESTION_CAP` (3). This catches a file that lives in a
  different directory than the caller guessed.

## Why strsim, and why it added no build cost

`strsim = "0.11"` is a direct dependency
(`Cargo.toml`), but it compiles nothing new: it was already transitive in the
tree via `clap`, so the direct dep resolves the same version. The comment in
`Cargo.toml` records this deliberately — the suggestion feature was cheap
precisely because the fuzzy-match crate was already paid for.

## Why this shape

The design principle is **zero extra round trips on a miss**. An agent that asks
for a wrong corpus or path would otherwise have to follow up with `list_corpora`
or `list_files` and re-issue the call; folding the listing and the closest match
into the error itself collapses that loop. The caps exist so the enrichment
never floods the error with a huge corpus's full file list.

## Related

- [mcp-surface](mcp-surface.md) — the `read_markdown` / corpus resolution tools these errors come from, and the JSON-RPC error mapping.
- [config-layering](config-layering.md) — where the configured-corpus set that `pick_corpus` lists is assembled.
