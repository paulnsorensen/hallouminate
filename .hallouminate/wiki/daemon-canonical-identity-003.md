# Store-lock owner diagnostics

<certain> `flock` remains the store-ownership authority; the lock file additionally records enough metadata to identify a contending process. **Shipped in PR #320.**

## ADR-003: Attach diagnostics to the store ownership lock  [status: shipped in #320]

- **Context:** <certain> Before #320, `LanceStore` held `store.lock` for its lifetime and timed out after bounded retry, but the resulting error named no owner. Operators could not distinguish a hidden daemon from another direct process without external inspection.[^1]
- **Decision:** <certain> After acquiring `flock`, write diagnostic JSON containing PID, socket, and Hallouminate version, then retain the same file handle in `LanceStore`. On contention, parse and report valid metadata. Missing or malformed metadata retains the generic error.
- **Alternatives:** <certain> `/proc` or process-table inspection is platform-specific and still cannot prove ownership. A daemon-only registry would not cover direct `LanceStore` users. Treating metadata as authority would permit stale-file errors.
- **Consequences:** <certain> Lock failures become actionable for new owners without weakening fail-closed storage safety. Old daemons leave no metadata, so contention against them can remain generic. `StoreLockOwner` crosses from the daemon into the adapters crate while `LanceStore::open_or_create` keeps a process-only compatibility path.
- **As implemented:** <certain> Freshness needed a second file the ADR did not anticipate. A separate advisory guard, `store.lock.diagnostics`, is `flock`ed alongside `store.lock`; a contender reads the metadata only when that guard is also held, so bytes left by a dead owner never become a false attribution.[^2]

<certain> Metadata describes the kernel lock holder; it never establishes or extends ownership.

[^1]: `crates/hallouminate-adapters/src/lance.rs:684-737,786-814`.
[^2]: `crates/hallouminate-adapters/src/lance.rs:30-61` (`StoreLockOwner`, both lock filenames); `lance.rs:742-764` (`contended_store_lock_owner`).
