# Idle-exit / idle-shutdown policy for long-lived, auto-respawned local daemons

## Question
For hallouminate issue #222: which activity should reset an idle-exit clock in a
tokio daemon holding ONNX Runtime models, when many concurrent Claude Code
clients send cheap keepalive-like RPCs that currently starve the 900s
idle-exit timer.

## Synthesis

Every mature comparable daemon counts only **build/compile/inference work**
toward its idle timer, not liveness or status RPCs — Bazel, sccache, and
Gradle all treat a "request" as a build/compile invocation, and none of their
public docs describe a cheap status RPC resetting the clock. Two of the three
(Bazel, Gradle) additionally support **memory-pressure-triggered shutdown**
layered on top of the idle timer, not instead of it, and both guard in-flight
work by only killing between builds/compiles, not mid-request. **Exit-on-idle
with on-demand respawn has a documented, still-partially-open race class**
(systemd/D-Bus discussion; sccache #204) where a shutdown-in-progress can eat
a request that arrives concurrently, and the general mitigation is "wait for
pending work before exiting" rather than "exit exactly at t=0 connections."
For the ORT arena question: `memory.enable_memory_arena_shrinkage` is a
**`RunOptions` (per-run) config key, not a session key**, it is reported
non-functional for the CPU arena in current ORT (a maintainer says it was
built for GPU and recommends disabling the CPU arena entirely instead), and
even with the CPU arena off, a separate ORT 1.28 mechanism (thread-local
packed-GEMM/KleidiAI buffers on Apple Silicon SME) leaks memory outside the
arena's control — so arena shrinkage is **not a reliable non-exit
alternative** for this daemon's memory-reclaim problem; exit-based recycling
remains the primary lever.

## Evidence

| # | Claim | Source | Quote/paraphrase | Confidence |
|---|---|---|---|---|
| 1 | Bazel's idle timer counts server inactivity between commands, not per-command-type; default 3h (10800s), 0 disables it | [Bazel command-line-reference](https://bazel.build/reference/command-line-reference) | `--max_idle_secs`: "The number of seconds the build server will wait idling before shutting down. Zero means that the server will never shutdown." Default 10800. | certain |
| 2 | Bazel layers a memory-pressure shutdown on top of (not instead of) the idle timer, Linux/macOS only | [Bazel command-line-reference](https://bazel.build/reference/command-line-reference) | `--shutdown_on_low_sys_mem`: "If max_idle_secs is set and the build server has been idle for a while, shut down the server when the system is low on free RAM. Linux and MacOS only." Default false. | certain |
| 3 | `--max_idle_secs` is read only at server startup; live changes need a restart | [Bazel GitHub issue #6773](https://github.com/bazelbuild/bazel/issues/6773) | Issue title/body: changing `--max_idle_secs` on a running server has no effect until restart. | certain |
| 4 | sccache's idle clock is compile-request activity; default idle timeout 600s, `SCCACHE_IDLE_TIMEOUT=0` disables it | [sccache server.rs](https://docs.rs/sccache/latest/src/sccache/server.rs.html) | Default idle timeout 600s; `SCCACHE_IDLE_TIMEOUT` env var controls it, 0 disables idle shutdown. | certain |
| 5 | sccache has an open, acknowledged race: idle shutdown can kill in-flight long compiles because it only waits 10s for pending work before terminating connections | [sccache issue #204](https://github.com/mozilla/sccache/issues/204) | Reporter: an 18-minute compile was killed by the 10-minute idle timeout firing mid-job; server "waits 10 seconds for pending work to finish" then terminates, dropping the client connection. Proposed fix: keep listening for a shutdown-eligible state instead of force-terminating when jobs are still in flight. Status: open, unresolved as of fetch. | certain |
| 6 | Gradle's idle timer is separate from its JVM-memory-pressure-based daemon expiry; both exist, and OOM expiry happens at build boundaries ("will be stopped at the end of the build") | [Gradle issue #24026](https://github.com/gradle/gradle/issues/24026), [Gradle issue #14741](https://github.com/gradle/gradle/issues/14741) | Daemon expires "because JVM heap space is exhausted" with message "Daemon will be stopped at the end of the build after running out of JVM memory" — i.e., recycling is deferred to a safe boundary, not an immediate kill mid-build. Configurable via `org.gradle.daemon.idletimeout`. | certain |
| 7 | Duplicate/non-primary Gradle daemons get a separate, much shorter (10s) expiry independent of the idle-timeout property | [Gradle issue #8408](https://github.com/gradle/gradle/issues/8408) | "Gradle expires the non-recently used daemons after a fixed idle timeout of 10 seconds... does not affect the expiration of duplicate daemons" (hardcoded 10s grace period). | certain |
| 8 | systemd's `systemd-socket-proxyd --exit-idle-time` (added v246) is exit-on-*no-connections*, paired with `StopWhenUnneeded=` on the real unit for on-demand respawn | [systemd-socket-proxyd(8) man page](https://man.archlinux.org/man/systemd-socket-proxyd.8.en) | `--exit-idle-time=` "takes a time span value... configures the idle timeout, i.e. specifies how long to wait without any connection before exiting"; default infinity. | certain |
| 9 | The systemd maintainer (Poettering) explicitly distinguishes "no active connections" exit-on-idle from dependency-based `StopWhenUnneeded=`, and confirms exit-on-idle was accepted as a real feature request specifically because it needs to track connections-in-progress, not just presence of any client | [systemd issue #2106](https://github.com/systemd/systemd/issues/2106) | Poettering: "Exit-on-idle would mean that the daemon exits on its own when there are no more connections going on. I think that would make a ton of sense to add" — turned the report into an RFE; feature (`--exit-idle-time`) was later implemented in `systemd-socket-proxyd`. | certain |
| 10 | D-Bus's parallel exit-on-idle effort was abandoned as "impossible to implement correctly for non-trivial cases" due to races between a new connection arriving and a service mid-shutdown; the proposed structural fix was a different socket transport, not a timer tweak | [systemd-devel: dbus and exit-on-idle](https://lists.freedesktop.org/archives/systemd-devel/2017-May/038892.html) | Discussion concludes race-free exit-on-idle needs `SOCK_SEQPACKET`-based unified connection-state tracking; timer-based heuristics alone are unreliable. | speculating (mailing-list thread, not a merged doc, but from the systemd maintainer) |
| 11 | Practical race pattern confirmed independently for socket-proxy-style idle exit: a request arriving exactly as the backing service is stopping is dropped unless the client/launcher retries or waits | [utcc.utoronto.ca notes on systemd-socket-proxyd](https://utcc.utoronto.ca/~cks/space/blog/linux/SystemdSocketProxydNotes) | "If a request is made during the server's service shutdown, the service will not restart" without a startup script that waits for the prior shutdown to complete. | speculating (third-party blog, but consistent with #2106/#9 above) |
| 12 | macOS launchd's guidance for on-demand daemons is to opt in to `EnablePressuredExit` for memory-pressure-driven reclaim; launchd itself does not idle-exit jobs by elapsed-time on its own, it reclaims idle jobs under system load/pressure | [Apple: Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html), [TN2083: Daemons and Agents](https://developer.apple.com/library/archive/technotes/tn2083/_index.html) | "Jobs seeking to exit when idle should use the `EnablePressuredExit` key to opt into the system mechanism for reclaiming killable jobs under memory pressure"; on modern systems launchd "cleans up idle jobs when the system is under load" rather than on a fixed idle timer. | certain |
| 13 | gunicorn/uWSGI worker recycling is request-count- or lifetime-based (`max_requests`, `max-worker-lifetime`) and memory-based (`reload-on-rss`), and jitter is used specifically to avoid a thundering-herd of simultaneous recycles | [Gunicorn Settings docs](https://gunicorn.org/reference/settings/) | `max_requests`: workers restart after N requests; `max_requests_jitter` adds randomness "to prevent all workers from restarting at the same time." uWSGI: `reload-on-rss` restarts a worker once RSS crosses a hard limit; `max-worker-lifetime` restarts after N seconds. All are per-*worker* recycles behind a load balancer/master, never a full-process idle-exit of the only server. | certain |
| 14 | rust-analyzer's process lifetime is intentionally tied to the editor session, not an internal idle timer; it has no documented idle-exit/memory-recycle policy of its own — the editor/client owns spawn and shutdown | [rust-analyzer issue #15187](https://github.com/rust-lang/rust-analyzer/issues/15187), [rust-analyzer issue #5258](https://github.com/rust-lang/rust-analyzer/issues/5258) | Reports describe rust-analyzer continuing to run and consume resources after the editor closes because nothing sent it a shutdown/exit, i.e., there is no independent idle-exit safety net in the LSP server itself; the client is the sole lifecycle owner. | certain |
| 15 | `memory.enable_memory_arena_shrinkage` is a `RunOptions` (per-`Run()`) config entry, applied post-run, distinct from `arena_extend_strategy`/`enable_cpu_mem_arena` which are session-construction-time settings | [ONNX Runtime C API docs](https://onnxruntime.ai/docs/get-started/with-c.html) | "ShrinkArenaAfterRun... is applied on every run," while "LimitBytes (`gpu_mem_limit`) and ArenaExtend (`arena_extend_strategy`) are read when a session is built." Set via `run_option.AddConfigEntry("memory.enable_memory_arena_shrinkage", "cpu:0")` (or `"gpu:0"`). | certain |
| 16 | A user-reported, maintainer-acknowledged report says `memory.enable_memory_arena_shrinkage` has no measurable effect on CPU-provider memory via Python bindings on ORT 1.20.1; the maintainer's guidance is that shrinkage "was primarily introduced for GPU memory" and for CPU the recommendation is to **disable the arena entirely** rather than rely on shrinkage | [onnxruntime issue #23339](https://github.com/microsoft/onnxruntime/issues/23339) | User: "I call `InferenceSession.run` with `memory.enable_memory_arena_shrinkage`, but it doesn't seem to have any effect." Maintainer: "This feature was primarily introduced for GPU memory. For CPU we recommend disabling the arena all together and see if default allocator does a better job (it often does)." A follow-up commenter reports the same daemon-style symptom this research targets: CPU provider, long-lived service, memory jumps from ~4GB to ~10GB after a large input and never returns even with the arena disabled. Thread stale-closed without a fix. | certain |
| 17 | The `ort` Rust crate (used transitively via `fastembed`) exposes the same `AddConfigEntry` mechanism through `RunOptions::set(key, value)`, so `run_options.set("memory.enable_memory_arena_shrinkage", "cpu:0")` is reachable from Rust if the caller threads `RunOptions` into the inference call | [docs.rs: ort RunOptions](https://docs.rs/ort/latest/ort/session/struct.RunOptions.html) | `pub fn set(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> Result<()>` — arbitrary config entries, same shape as `SessionOptions::with_config_entry`. | certain |
| 18 | The user's own team already found and fixed a *separate*, ORT-1.28-specific memory leak in `fastembed-rs` unrelated to the BFC arena: thread-local packed-GEMM buffers from the KleidiAI path on Apple Silicon SME never shrink, and disabling that path (`mlas.disable_kleidiai=1`, a session-level config, not the arena shrinkage run-option) cut retained memory from 2189 MB to 216 MB after one embedding batch | [fastembed-rs PR #292](https://github.com/anush008/fastembed-rs/pull/292) (merged 2026-09-12), [fastembed-rs issue #291](https://github.com/anush008/fastembed-rs/issues/291), [onnxruntime issue #29538](https://github.com/microsoft/onnxruntime/issues/29538) | PR body: "ONNX Runtime 1.28.0 keeps packed GEMM buffers in thread-local storage. The buffers never shrink... With `mlas.disable_kleidiai=\"1\"` the same process retains 216 MB instead of 2189 MB." This is orthogonal to `memory.enable_memory_arena_shrinkage` (claim 16) — it is a session config entry, and it addresses a leak the arena shrinkage run-option does not reach. | certain |

## Answers to the four sub-questions

1. **Which activity counts toward idleness in mature daemons?** Bazel and
   sccache count build/compile invocations; Gradle counts build invocations
   for the idle timer and separately watches JVM heap for OOM-triggered
   expiry at a build boundary. None of the surveyed daemons documents a
   distinct "cheap status/list RPC" category — their command surface is
   already narrow (compile/build), so this daemon's choice of "only
   inference-bearing work resets the clock" (candidate a) matches the
   pattern: idleness = absence of the work the daemon exists to do, not
   absence of any client traffic. (Claims 1, 4, 6.)

2. **Do any use memory-based or lifetime-based recycle, and how do they avoid
   killing in-flight work?** Yes — Bazel (`--shutdown_on_low_sys_mem`, layered
   on the idle timer, so it only fires when already idle), Gradle (JVM
   heap-exhaustion expiry deferred to "end of build"), gunicorn/uWSGI
   (`max_requests`, `max-worker-lifetime`, `reload-on-rss`, all per-worker
   behind a pool so one recycling worker doesn't drop capacity, plus
   jitter to avoid correlated recycles), and launchd (`EnablePressuredExit`,
   an OS-level opt-in reaped under memory pressure, not a self-timer). The
   common pattern for safety is: recycle triggers are checked only when the
   daemon is *already idle or between discrete units of work*, never
   mid-request. (Claims 2, 6, 12, 13.)

3. **Known races for exit-on-idle with on-demand respawn?** Yes, and they are
   not fully solved anywhere surveyed. systemd's own maintainer treated
   generic exit-on-idle as hard to make race-free for D-Bus and it was
   dropped there in favor of a transport redesign; `systemd-socket-proxyd`'s
   `--exit-idle-time` implementation is narrower (proxy-level, single
   listening socket) and still needs `StopWhenUnneeded=` cooperation to avoid
   killing a service a new connection just triggered. sccache has an open bug
   where the 10-second "wait for pending work" grace period is too short and
   kills genuinely in-flight (18-minute) jobs. The general mitigation pattern
   across sources is: track in-flight work explicitly and refuse to exit (or
   extend the grace window) while a unit of work is outstanding, rather than
   trusting a simple connection-count-reaches-zero timer. (Claims 5, 9, 10, 11.)

4. **Is ORT arena shrinkage a viable non-exit alternative in ORT 1.2x via
   `ort`/`fastembed`?** Evidence says no, for two independent reasons: (a) a
   maintainer-acknowledged report says the CPU-arena shrinkage run-option has
   no measurable effect and the maintainer's own recommendation for CPU is to
   disable the arena entirely, not to rely on shrinkage; and (b) even with
   the arena off, this exact team already found a *different* ORT 1.28 leak
   (thread-local KleidiAI GEMM buffers) that arena shrinkage cannot reach at
   all, because it lives outside the arena allocator. The `ort` crate does
   expose the mechanism (`RunOptions::set`) if someone wants to test it
   empirically against this daemon's actual model/batch shapes, but the
   primary-source evidence does not support relying on it as a memory-reclaim
   strategy in place of process exit. (Claims 15, 16, 17, 18.)

## Open questions

- Whether `memory.enable_memory_arena_shrinkage` behaves differently on the
  CPU arena in ORT 1.2x specifically for the embed/rerank workloads this
  daemon runs (batch sizes, model shapes) was not empirically tested here —
  issue #23339 is one user's report on ORT 1.20.1, not a controlled benchmark
  against this daemon's models.
  Cited sources present "disable the CPU arena" and "keep shrinkage" as
  alternatives without ranking them for this use case; this is not a
  recommendation, only what the sources say.
  Whether `fastembed`'s public API threads `RunOptions` through to
  `session.run()` (vs. only `SessionOptions` at construction time, as landed
  in PR #292) was not verified in this pass — needed before shrinkage could
  even be attempted from this daemon.
- No primary source was found describing a "keepalive/liveness RPC should
  never reset an idle clock" policy stated as an explicit design principle
  anywhere (it was inferred from what mature daemons treat as their unit of
  "activity"); this is an inference, not a directly stated best practice.
- gopls-specific idle/lifetime documentation was not directly retrieved in
  this pass (searches surfaced rust-analyzer and general LSP process-lifetime
  discussion instead); gopls likely follows the same editor-owns-lifecycle
  pattern as rust-analyzer, but that is unconfirmed by a primary gopls source.

## Confidence
Overall: certain for the comparable-daemon survey and the ORT arena-shrinkage
non-viability finding (multiple primary sources, including a maintainer
statement and the team's own merged fix for a related leak); speculating only
on the D-Bus race-avoidance mailing-list thread and the third-party blog note
on systemd-socket-proxyd races, both corroborating but non-primary for that
specific sub-claim.

## Agent resolution
Gathered inline in the researcher's own context window (no sub-agent fork);
all fetches were single-hop WebSearch/WebFetch/gh calls, none heavy enough to
warrant a sub-agent per `context-isolation.md` triggers.
