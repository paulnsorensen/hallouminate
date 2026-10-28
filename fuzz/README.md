# Fuzz targets

cargo-fuzz (libFuzzer) targets for the hallouminate-domain parsers and the
text helpers that handle wiki content, MCP edit arguments, and paths.
Each target checks the documented contract in `src/lib.rs`, not only "no panic".

| Target | Domain code | Invariant |
| --- | --- | --- |
| `claim_marks` | `strip_claim_marks`, `extract_claim_marks` | Line count kept; marks on real lines, in order |
| `frontmatter` | `split_frontmatter` | `rest` is a suffix; line count matches the removed block |
| `footnotes` | `apply_footnote_mode` | `Include` is identity; `Exclude` and `Only` never grow text |
| `chunker` | `MarkdownChunker` | Verbatim, ordered chunks; line ranges inside the document |
| `rst_chunker` | `RstChunker` | Same chunk invariants as `chunker` |
| `section_edit` | `splice_under_heading`, `replace_line_range`, `replace_unique_match` | Bytes outside the edit kept; errors only for documented reasons |
| `relative_path` | `safe_relative_path` | Accepted paths have only normal components |
| `wikilinks` | `find_wikilinks`, `lint_wikilinks`, `resolve_slug` | Targets trimmed and closed; lint agrees with `resolve_slug` |
| `text_helpers` | `make_snippet`, `extract_summary`, `split_terms`, `collapse_whitespace`, `validate_phrase`, `inject_block` | Caps kept; whitespace collapse idempotent; index prose kept |

`section_edit` reads its first three bytes as the edit and its arguments.
A NUL in the rest separates the document from the inserted body.

## Run locally

```sh
just fuzz-build                # build all targets under the verification lease
just fuzz claim_marks 300      # fuzz one target for 300 seconds
```

The seeds are `seeds/` plus every page in `.hallouminate/wiki/`. libFuzzer
writes new inputs to `corpus/<target>/` and crash inputs to
`artifacts/<target>/`. Both directories are ignored by git.

## CI

`.github/workflows/fuzz.yml` has three jobs. The shell they need lives in
`ci/`, so you can run it locally with `gh` logged in.

- **build** runs on a PR that touches the parsers or this crate. It builds
  every target and replays the seeds and the newest daily corpus once with
  `-runs=0`. The corpus download is best effort: a fork PR or an API error
  replays the seeds only.
- **fuzz** runs daily, one leg per target. It restores the corpus, fuzzes
  for 1200 s with `--jobs $(nproc)`, minimizes the corpus with `cmin`, and
  uploads it as the `fuzz-corpus-<target>` artifact for 90 days. A failed
  `cmin` fails the leg and uploads the old corpus. The upload also runs when
  the fuzz step fails. Crash inputs go to `fuzz-crash-<target>`.
- **report** runs when a daily leg fails. It opens one issue per target with
  the title `Fuzz failure: <target>`, or comments the run URL on the open
  issue with that title. Close the issue when the fix lands.

The corpus is an artifact because the repository Actions cache is full, and
LRU eviction removes a daily cache entry before the next run.
`ci/corpus-artifact.sh` takes the newest artifact that is not expired and
comes from a `schedule` or `workflow_dispatch` run of `fuzz.yml` on the
default branch. It ignores artifacts from PR runs, so a PR cannot plant
corpus inputs. No artifact means the leg starts from the seeds.
`ci/test-corpus-artifact.sh` tests the restore offline with a `gh` stub.

`--jobs N` runs libFuzzer with `-fork=N`, and new inputs go to the first
corpus directory. Fork mode ignores timeouts and OOMs, so
`ci/check-artifacts.sh` fails the leg when `artifacts/<target>/` holds a
`crash-`, `timeout-`, `oom-`, or `leak-` file. `-timeout=10` reports an
input that runs longer than 10 s; the libFuzzer default is 1200 s.

To fuzz locally from the daily corpus:

```sh
GITHUB_REPOSITORY=paulnsorensen/hallouminate ci/corpus-artifact.sh fetch chunker corpus/chunker
```

## Reproduce a CI crash

1. Download the `fuzz-crash-<target>` artifact from the failed Fuzz run.
2. Run `cargo fuzz run --debug-assertions <target> <crash-file>` in `fuzz/`.
3. Minimize it with `cargo fuzz tmin --debug-assertions <target> <crash-file>`.
4. Add the minimized input to `seeds/` and a regression test in the parser
   module, then fix the parser.

## Bump the nightly

Change `channel` in `rust-toolchain.toml` and run `just fuzz-build`.
The toolchain file applies only inside `fuzz/`. A `RUSTUP_TOOLCHAIN`
environment variable overrides it, so the just recipes set that variable
from the file.
