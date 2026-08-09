# Central daemon discovery and migration

<certain> Every default client and lifecycle entry point uses one ordered discovery seam; compatible legacy daemons are adopted during a bounded migration window. **Shipped in PR #320**; the legacy probe is still present and its removal is tracked by issue #323.[^2]

## ADR-002: Centralize discovery and adopt discoverable legacy daemons  [status: shipped in #320]

- **Context:** <certain> Sibling probing was introduced in `client.rs`, then reused by bootstrap and lifecycle code, while raw socket resolution remained callable from several modules. Caller drift allowed `serve`, `status`, or `stop` to disagree about daemon liveness.[^1]
- **Decision:** <certain> `DaemonSocketPaths` produces the canonical path and at most one discoverable legacy path. A single default-connect seam probes canonical first, then legacy. Server startup binds canonical only. Exact overrides skip discovery.
- **Alternatives:** <certain> Forcibly restarting a compatible legacy daemon adds shutdown coordination and interrupts active clients. A persistent rendezvous record adds a new stale-state protocol. Leaving callers to compose path helpers independently preserves recurrence risk.
- **Consequences:** <certain> Ordinary clients, MCP bootstrap, `daemon status`, `daemon stop`, and `daemon restart` share candidate ordering. A reachable compatible legacy daemon remains usable until exit or restart; its replacement binds canonical. The legacy probe is removed after the first canonical-socket release through follow-up issue #323.

<certain> Portable migration covers the current non-empty XDG runtime path and Linux's conventional `/run/user/<euid>` path. An arbitrary old path absent from the current environment cannot be discovered without non-portable process scanning.

[^1]: `crates/hallouminate-daemon/src/socket.rs:32-105`; `crates/hallouminate-daemon/src/client.rs:53-95`; `crates/hallouminate-daemon/src/bootstrap.rs:48,111`; `crates/hallouminate-daemon/src/lifecycle.rs:39-44,76-82`.
[^2]: [Issue #323 — remove legacy XDG socket probing after migration](https://github.com/paulnsorensen/hallouminate/issues/323). <certain> The shipped call sites still name the pair `primary` / `sibling` (`bootstrap.rs:111`, `lifecycle.rs:44,82`, `client.rs#connect_primary_or_sibling`), which contradicts the canonical/legacy vocabulary in [domain-model](domain-model.md); rename with #323.
