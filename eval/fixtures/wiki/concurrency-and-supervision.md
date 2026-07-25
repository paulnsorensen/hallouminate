---
status: reviewed
last_verified: 2026-07-21
confidence: high
sources:
  - https://github.com/paulnsorensen/hallouminate/issues/182
  - https://github.com/paulnsorensen/hallouminate/issues/196
---
# Concurrency and supervision

An `index` request — a full walk-plan-apply cycle over a corpus — can take anywhere from milliseconds to minutes depending on corpus size and how much changed. The daemon runs that work inside a supervisor with its own deadline, separate from any per-connection request deadline the caller is holding, so a scan that stalls cannot wedge the daemon for every other request.[^1]

## Why a supervisor, not a bare spawned task

A scan is not a single operation; it's a pipeline of filesystem walking, chunking, embedding, and LanceDB writes, any stage of which can hang — a huge file that pathologically defeats the chunker, an embedding call that never returns, a LanceDB write blocked behind a lock held by something else. Spawning the scan as a bare background task and hoping it finishes means the daemon has no way to notice that hang short of the caller eventually giving up and the daemon slowly accumulating stuck tasks. The supervisor wraps the scan in an explicit deadline and polls it, so a hang becomes a detected, reported condition instead of a silent one. This is the same instinct behind [daemon-idle-exit](daemon-idle-exit.md): don't rely on a resource eventually sorting itself out, make its lifecycle explicit.

## Why a stalled scan must not wedge the daemon

The daemon holds per-corpus mutation locks and a global write-lane semaphore for any mutating operation — see [daemon-and-cli](daemon-and-cli.md) for the lock order. If a scan against one corpus hung while still holding its per-corpus lock and a slot in the write-lane semaphore, every other mutating request against every other corpus would queue behind it indefinitely, because the write-lane semaphore is shared across corpora. Read operations (`ground`, `list_files`) don't take either lock and would keep working, but the daemon would effectively stop accepting new writes anywhere until the stuck scan was killed by hand. A supervisor deadline turns that unbounded queue into a bounded one: the scan is killed and its locks released once the deadline passes, regardless of which corpus it was running against.

## Killing a slow scan vs. returning partial results

When a scan hits its deadline, the supervisor terminates it rather than letting it run to completion in the background and returning whatever partial index state exists at the deadline. The alternative — surface partial results and let the scan keep running — was considered and rejected: a caller who gets back "here's what indexed so far, the rest is still in progress" has no clean way to know when the rest finishes short of polling, and in the meantime the corpus is left in a state where some files are indexed against stale mtimes and others aren't indexed at all. A killed-and-reported scan is a state the caller can reason about (retry, or investigate why this corpus's scan is slow) instead of an open-ended one they have to poll around.

This tradeoff costs real completed work on a scan that was making progress but happened to be slow rather than actually hung — a large corpus near the deadline boundary pays for a from-scratch retry rather than resuming. That's an accepted cost: the corpus walker's mtime-based diffing (see [corpus-walker](corpus-walker.md)) makes a retried scan cheap for everything that already indexed successfully, since only files with content or mtime changes since the last successful apply get re-processed.

## What the deadline does not cover

The supervisor deadline bounds the daemon-side scan; it is a different deadline from the client-side request deadline described in [socket-protocol](socket-protocol.md). A client can give up on its socket read before the supervisor's deadline fires, in which case the scan keeps running server-side to either finish or hit its own deadline — the client's early exit doesn't cancel daemon-side work. Conversely the supervisor deadline firing doesn't mean the client necessarily already gave up; it's the daemon's own backstop against a hang, independent of whether anyone is still listening for the answer.

See [daemon-and-cli](daemon-and-cli.md), [corpus-walker](corpus-walker.md), [daemon-idle-exit](daemon-idle-exit.md), [socket-protocol](socket-protocol.md).

[^1]: `crates/hallouminate-daemon/src/supervisor.rs:60-140`; https://github.com/paulnsorensen/hallouminate/issues/182
[^2]: `crates/hallouminate-daemon/src/maintenance.rs:200-260`; https://github.com/paulnsorensen/hallouminate/issues/196

_Source: issues #182 and #196 · Updated: 2026-07-21_