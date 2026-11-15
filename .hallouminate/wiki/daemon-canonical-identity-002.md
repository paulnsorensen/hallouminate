# Central daemon discovery and migration

<certain> Every default client and lifecycle entry point resolves the one canonical socket through `daemon_socket_path`. **Shipped in PR #320**; the one-release legacy XDG probe was removed by issue #323.[^2]

## ADR-002: Centralize discovery and retire the legacy probe  [status: shipped in #320; probe removed in #323]

- **Context:** <certain> Sibling probing was introduced in `client.rs`, then reused by bootstrap and lifecycle code, while raw socket resolution remained callable from several modules. Caller drift allowed `serve`, `status`, or `stop` to disagree about daemon liveness.[^1]
- **Decision:** <certain> `daemon_socket_path` is the only default-discovery seam: an exact `HALLOUMINATE_SOCKET` override, else the canonical account-home path. Clients, bootstrap, and lifecycle dial that one path. Server startup binds it. PR #320 also probed the XDG runtime path for one release so a compatible running daemon was adopted instead of interrupted; #323 removed that probe.
- **Alternatives:** <certain> Forcibly restarting a compatible legacy daemon adds shutdown coordination and interrupts active clients. A persistent rendezvous record adds a new stale-state protocol. Leaving callers to compose path helpers independently preserves recurrence risk.
- **Consequences:** <certain> Ordinary clients, MCP bootstrap, `daemon status`, `daemon stop`, and `daemon restart` share one path. A daemon that listens only at the old `$XDG_RUNTIME_DIR/hallouminate/daemon.sock` path is no longer adopted, reported, or stopped; stop it by hand (or point `HALLOUMINATE_SOCKET` at it) before upgrading past the release that contains #323.

<certain> Releases v0.10.0 through v0.11.1 contain the canonical socket and the legacy probe, so the one-release migration window has passed.

[^1]: `crates/hallouminate-daemon/src/socket.rs:11-18`; `crates/hallouminate-daemon/src/client.rs:30-84`; `crates/hallouminate-daemon/src/bootstrap.rs:43-115`; `crates/hallouminate-daemon/src/lifecycle.rs:38-45,70-80`.
[^2]: [Issue #323 — remove legacy XDG socket probing after migration](https://github.com/paulnsorensen/hallouminate/issues/323). The `primary` / `sibling` naming and the `DaemonSocketPaths` pair are gone with the probe.
