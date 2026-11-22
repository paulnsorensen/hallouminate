# Build channel paths

A release build and a dev build never share a default store, bootstrap log, or daemon socket. A dev build uses `hallouminate-dev` where a release build uses `hallouminate`. Both share the baseline config and the model cache.[^code]

## Why

A dev build with store schema v4 migrated the shared `~/.local/share/hallouminate/ground`. The installed nightly (schema v3) then refused the store with `store schema version 4 is NEWER than this build expects (3)`. Per-channel defaults stop a worktree build or test run from touching the installed release's state.

## How a build knows its channel

- `hallouminate_config::channel::BUILD_CHANNEL` is `Release` when `HALLOUMINATE_RELEASE_CHANNEL` is non-empty at compile time (`option_env!`), else `Dev`.
- `nightly.yml` sets it to `nightly`. `release.yml` (`build-local-artifacts`, cargo-dist) sets it to `stable`. Keep both when the workflows change; a release build without it moves every installed user to empty dev paths.
- `crates/hallouminate-config/build.rs` sets it to `crates-io` when `.cargo_vcs_info.json` is in the crate directory. `cargo package` writes that file, so `cargo install hallouminate` stays a release build. A git checkout has no such file.
- The marker is not `debug_assertions`: a dev build can use the release profile.

## What moves

| Default | Release | Dev |
| --- | --- | --- |
| `[storage].ground_dir` | `~/.local/share/hallouminate/ground` | `~/.local/share/hallouminate-dev/ground` |
| Daemon socket | `<account-home>/.cache/hallouminate/daemon.sock` | `<account-home>/.cache/hallouminate-dev/daemon.sock` |
| Bootstrap log | `$XDG_STATE_HOME/hallouminate/daemon-bootstrap.log` | `$XDG_STATE_HOME/hallouminate-dev/daemon-bootstrap.log` |

The watchdog trip file sits next to the socket, so it moves too.

## What does not move

- The baseline config stays `$XDG_CONFIG_HOME/hallouminate/config.toml` for both channels.
- `[embeddings].cache_dir` stays `~/.cache/hallouminate/fastembed`. Model files are immutable downloads with no schema, so sharing them is safe. `eval/baseline.json` also records this value, and `committed_baseline_matches_the_current_schema_and_records_provenance` requires it to equal the production default, so a per-channel cache would fail every dev test run.
- Explicit `[storage].ground_dir`, `[embeddings].cache_dir`, and `HALLOUMINATE_SOCKET` win. A baseline from `hallouminate config init` pins `ground_dir`, so it shares the store across channels.
- The process log (`logging.rs`) still writes `$XDG_STATE_HOME/hallouminate/hallouminate.log`.

## Tests

Path helpers take a `BuildChannel` argument, so unit tests cover both channels in one build: `ground_dir_for` (`crates/hallouminate-config/src/lib.rs`), and `resolve_socket_path`, `canonical_socket_path` (`crates/hallouminate-daemon/src/socket.rs`).

## Related

- [daemon-and-cli](daemon-and-cli.md): socket resolution and single-instance lock.
- [daemon-version-skew](daemon-version-skew.md): `HALLOUMINATE_BUILD_ID`, the other compile-time build marker.

[^code]: `crates/hallouminate-config/src/channel.rs`, `crates/hallouminate-config/build.rs`, `crates/hallouminate-daemon/src/socket.rs`, `crates/hallouminate-daemon/src/bootstrap.rs`.
