# Development verification

Run local Cargo checks through the repository verification gate:

```sh
just verify
```

With no arguments, the gate runs `cargo fmt --all --check`, then holds one
exclusive lease across clippy, build, and test. The leased commands run with
`CARGO_BUILD_JOBS=1`.

## Targeted checks and fixes

Use the same lease for a focused Cargo command:

```sh
just verify cargo test -p hallouminate --test it verification_gate
```

A targeted command must start with `cargo`. Caller-supplied `-j`,
`--jobs`, and their attached forms are rejected before Cargo's `--`
delimiter because the gate owns the job cap. All Cargo `--config` forms are
also rejected before that delimiter because inline values and referenced files
can override `build.jobs`.

`just verify --fix` holds the lease while applying machine-fixable clippy
suggestions, formatting, and running the full gate. `just ci` and `just llm`
remain compatibility routes to `just verify` and `just verify --fix`.

## Cross-worktree lease and evidence

Linked worktrees share the paths returned by `git rev-parse
--git-common-dir`. The gate stores its state there:

- `hallouminate-verify.lock` — the exclusive compiler/linker lease.
- `hallouminate-verify.jsonl` — append-only wait, start, and finish evidence.

Each record includes a UTC timestamp, worktree, redacted command, exact
SHA-256 command digest, HEAD, content-based state digest, and PID. Cargo
`--token` values and arguments after `cargo login` are omitted from the
recorded command, and the log is kept owner-only at mode `0600`. Start records
add the wait duration and job cap; finish records add the exit status, duration,
and final state digest. A prior result is reusable only when its command digest,
HEAD, and state digest match the current work and its finish status is zero.

The repository lease prevents duplicate compiler/linker-heavy work for this
project. It does not provide host-wide admission control or resource isolation;
the execution environment must supply host-wide scheduling and cgroups or
equivalent limits.

## Baseline measurement

Measured on 2026-07-14 on a Mac17,9 with 24 GiB RAM, 15 logical CPUs,
Rust 1.91, Python 3.14.6, and just 1.56.0. Both successful commands ran
with `CARGO_BUILD_JOBS=1` through the verification lease:

- A warm full `just verify` completed in 42.27 seconds with a maximum
  resident set size of 1,789,263,872 bytes (1.67 GiB).
- After `cargo clean -p hallouminate` removed 2.0 GiB of package artifacts,
  `just verify cargo test --locked --no-run -p hallouminate --test it`
  relinked the integration-test binary in 31.94 seconds with a maximum
  resident set size of 2,719,940,608 bytes (2.53 GiB).

Both commands exited zero. `/usr/bin/time -l` reported zero block input and
output operations on this APFS host, so those counters do not justify a
higher cap. The incident was driven by simultaneous linker I/O, and the
measured package relink already used 2.53 GiB at one Cargo job. The cap stays
at one because it is the only measured setting that admits no second Cargo
compilation unit; raising it requires a separate measurement with explicit
memory and I/O headroom.
