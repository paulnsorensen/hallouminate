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

Each feature binds to a collected `test_*.py` module.
The suite collects six scenarios.

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
