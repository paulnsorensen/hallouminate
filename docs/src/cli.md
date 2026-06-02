# CLI reference

`hallouminate` is a single binary. The CLI, the MCP server, and the daemon are
all the same executable; CLI subcommands dial the daemon over a Unix domain
socket (see [Architecture](./architecture.md)).

| Command | Purpose |
|---|---|
| `hallouminate serve` | Run the stdio MCP server (auto-spawns the daemon if down). |
| `hallouminate index [--corpus NAME]` | Bulk (re)index one corpus, or every configured corpus. |
| `hallouminate ground "<query>" [flags]` | Semantic search from the CLI. |
| `hallouminate daemon <run\|stop\|restart\|status>` | Manage the long-lived daemon. |
| `hallouminate config <init\|show\|validate\|download>` | Inspect or scaffold config. |
| `hallouminate hook <install\|uninstall>` | Manage the per-repo discovery hook. |

`hallouminate --version` prints the version; `hallouminate --help` and
`hallouminate <command> --help` print usage for any subcommand.

## `serve`

```sh
hallouminate serve
```

Starts the stdio MCP server an agent connects to. It is stateless beyond its
tool router and a startup-captured working directory — every tool call dials
the daemon. If no daemon is running, `serve` spawns one.

## `index`

```sh
hallouminate index               # rebuild every configured corpus
hallouminate index --corpus repo:hallouminate:wiki
```

Use this when files were touched outside hallouminate. Writes that go through
`add_markdown` already auto-reindex just the changed file.

## `ground`

```sh
hallouminate ground "how does the daemon work"
hallouminate ground "socket protocol" --corpus repo:hallouminate:wiki --format json-pretty
```

| Flag | Effect |
|---|---|
| `--corpus NAME` | Corpus to search (defaults to the wiki for the current repo). |
| `--format outline\|json\|json-pretty` | Output shape. `outline` (default) is a ripgrep-style digest. |
| `--full` | Return full chunk bodies instead of snippets. |
| `--top-files N` | Number of files to roll up. |
| `--chunks-per-file N` | Chunks to include per file. |
| `--limit N` | Hard cap on returned chunks. |
| `--snippet-chars N` | Snippet length when not using `--full`. |

## `daemon`

```sh
hallouminate daemon run        # run in the foreground
hallouminate daemon status     # is one running?
hallouminate daemon stop
hallouminate daemon restart
```

The daemon is the single owner of the LanceDB ground directory. Restart it
after editing the **baseline** config; repo-layer edits take effect on the
next request without a restart. `--config PATH` overrides the baseline config
path.

## `config`

```sh
hallouminate config init       # scaffold the XDG baseline config
hallouminate config show       # print the effective merged config for this cwd
hallouminate config validate   # parse + flag unknown top-level keys
hallouminate config download   # pre-fetch the configured embedding model
```

See [Configuration](./config.md).

## `hook`

```sh
hallouminate hook install [--repo PATH]
hallouminate hook uninstall [--repo PATH]
```

Installs or removes a per-repo discovery hook. `--repo PATH` targets a repo
other than the current directory.

## Socket override

`--socket PATH` on `index`, `ground`, and the other client subcommands points
at a specific daemon socket. Otherwise the socket is resolved from
`HALLOUMINATE_SOCKET`, then `$XDG_RUNTIME_DIR`, then the cache dir — see
[Architecture](./architecture.md#socket-location).
