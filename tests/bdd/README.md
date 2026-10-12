# Behavior tests

These scenarios exercise the real Rust CLI and MCP server through separate processes.
Python supplies test bindings and fixtures. It is not an application package.

## Run

Install Python 3.12 or newer, uv 0.13.0, Rust, protoc, ripgrep, and just.

```sh
just bdd
just verify
```

`just bdd` locks Python dependencies, builds once under the existing lease, and warms the tokenizer.
`just bdd-check` collects scenarios, runs them serially, and runs them with two pytest-xdist workers.
CI uses the same check after its existing Rust build and tokenizer warm-up.

Select scenarios with `just bdd -k publish`.
For another built binary, set `HALLOUMINATE_TEST_BIN` to its absolute path.
Fixtures respect `CARGO_TARGET_DIR` when the binary override is absent.
Workers never compile or download application assets.

## Behavior trace

| Requirement | Scenario | Public boundary | Assertion | Representative regression |
|---|---|---|---|---|
| Register a repository | Register for later processes | `init-repo`, `config validate` | Exit zero, registered corpus, exact initial index | Wrong wiki title |
| Reject accidental replacement | Reject repository replacement | `init-repo` | Exit one, specific error, unchanged config and index bytes | Ignore overwrite permission |
| Preserve the wiki during a rename | Rename without replacing wiki | `init-repo --force`, `config show` | New name, same root, unchanged wiki bytes | Rewrite the existing wiki |
| Publish readable, searchable documents | Publish for another client | MCP add/read/stats/ground | Exact content, indexed counts, document identity, ancestor links | Skip indexing or link refresh |
| Reject accidental document replacement | Reject document replacement | MCP add/read/ground | Invalid-parameters error, original content, unchanged index | Allow unrequested overwrite |
| Remove deleted documents | Delete from wiki and search | MCP delete/read/ground | Specific missing-file error, empty search, removed link | Keep document or indexed rows |
| Survive process changes | Graceful restart and manual restart after crash | daemon CLI, new MCP process | Exact persisted page and search hit after restart or stale-socket cleanup | Lose durable knowledge or refuse a stale socket |
| Start on first use | New client with no daemon | MCP serve, daemon status, add/read/ground | A later client reads and finds the page after automatic daemon startup | Require manual startup on the default socket |
| Keep one daemon owner | Reject second daemon | daemon CLI, live MCP process | Nonzero second exit; first client still reads and searches | Displace the active listener |
| Reconcile external files | Create, update, delete, repeat index | filesystem edit, MCP index/read/ground/list/stats | New facts appear, old facts disappear, unchanged index counts remain stable | Retain stale rows or miss new files |
| Observe outside edits | Watcher reconciliation | filesystem edit, MCP ground/stats | New phrase appears without explicit index; old phrase disappears; index has full file coverage | Leave external edits stale |
| Make safe targeted changes | Overwrite, section edit, ambiguous match | MCP add/read/ground | Exact replacement, preserved unrelated section, error and unchanged page | Clobber content or guess an edit target |
| Keep successful concurrent work | Distinct pages and shared-section appends | two MCP processes, add/read/ground | Both successful authors' facts remain once and are searchable | Lost update or serialized result omission |
| Isolate repository context | Two repositories on one daemon | MCP calls with distinct cwd | Default reads/searches use each wiki; pinned searches exclude the other wiki | Resolve every cwd to one repository |
| Navigate and cite | Corpus, file, tree, backlink, line and footnote reads | MCP list/read/backlinks/ground | Exact page path, linking page, source line, footnote and heading range | Hide source provenance or stale navigation |
| Reject unsafe or partial writes | Path escape and mixed-result batch | MCP add/read/ground | Outside bytes unchanged; ordered item errors do not roll back successes | Escape the root or treat a batch as atomic |

Each feature binds to a collected `test_*.py` module.
The suite collects 26 scenarios: six baseline cases and 20 knowledge workflows.

## Coverage boundaries

| Area | Status | Reason |
|---|---|---|
| Lexical search, live files, MCP writes, lifecycle, and navigation | Covered here | Tests use the built Rust binary, a real daemon, isolated storage, and separate MCP clients. |
| Dense retrieval and model downloads | Not run here | Offline fixtures disable embeddings. Ranking quality needs a pinned model and separate evidence. |
| Schema migration and version skew | Not run here | Schema migration needs dedicated legacy-store fixtures. Version skew needs two built binaries. |
| Interrupted indexing and partial database failure | Not run here | This suite does not inject internal faults or corrupt LanceDB state. |
| High-contention multi-process stress | Not run here | Two-author scenarios verify representative outcomes, not a stress or linearizability proof. |
| Mutation-based regression proof | Not run here | Production Rust edits are outside this test-only scope. |

## Isolation and lifetime

Function-scoped fixtures own repository files, HOME, XDG directories, model cache, and LanceDB storage.
Each invocation also owns a unique pytest temporary root, unless you explicitly supply `--basetemp`.
This prevents pytest cleanup from visiting other projects' temporary directories.
The root remains alive until workers stop and is removed when pytest exits.
Each scenario owns a short socket path under a unique temporary directory in `/tmp`.
A foreground daemon owns that socket. Readiness requires a successful public `daemon status` response.
Each MCP connection performs initialization and includes the repository cwd in every tool call.
A separate MCP process verifies that another client can read published state.

The tokenizer cache is the only shared application asset.
Fixtures set `HF_HUB_OFFLINE=1` and use it read-only after the serial warm-up.
Dense embeddings stay disabled. Tokenization, lexical search, indexing, and LanceDB persistence remain real.
The suite uses no dependency substitutes.

Requests and subprocess calls have deadlines.
Fixture teardown terminates only its owned processes, then waits or kills and reaps them.
Teardown also runs after failed assertions.
The suite requires Unix socket support, as does the application.

## Extend

Write a domain scenario before adding its step bindings.
Reuse the application and MCP fixtures.
Use public commands or tools for the behavior under test.
Keep setup shortcuts outside that behavior.
Check error categories and compare state before and after rejected mutations.
Run both execution modes before committing new scenarios.

Python wheel smoke tests do not apply because the application is a Rust binary.
