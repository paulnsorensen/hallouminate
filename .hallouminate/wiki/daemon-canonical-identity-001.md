# Canonical daemon socket

<certain> The default daemon address is one stable cache socket per OS user; environment variables no longer choose daemon identity. **Shipped in PR #320** — the sections below describe what landed, not a proposal.[^2]

## ADR-001: Resolve the default socket from the OS account record  [status: shipped in #320]

- **Context:** <certain> Before #320, `daemon_socket_path()` preferred `XDG_RUNTIME_DIR` and fell back to a home-cache path. Processes for one user could therefore select different socket locks while targeting the same ground store.[^1]
- **Decision:** <certain> Resolve the effective UID's account home through POSIX `getpwuid_r` and use `<account-home>/.cache/hallouminate/daemon.sock`. Ignore `HOME`, `XDG_RUNTIME_DIR`, and `XDG_CACHE_HOME` for the canonical default. Preserve `HALLOUMINATE_SOCKET` as an exact override.
- **Alternatives:** <certain> A stable rendezvous record preserves dynamic runtime sockets but adds stale-record and atomic-update protocol. OS socket activation adds installation and cross-platform lifecycle work. Keeping environment-selected defaults preserves the identity split.
- **Consequences:** <certain> All default processes for one account target the same socket. Account lookup can fail for an unregistered container UID, so resolution becomes fallible and the error names `HALLOUMINATE_SOCKET` as the explicit escape hatch. Windows remains outside the Unix-domain-socket transport.

<certain> The canonical socket path is an identity key, not merely a preferred transport candidate.

[^1]: [Issue #318 — sibling-socket probe does not adopt a live daemon](https://github.com/paulnsorensen/hallouminate/issues/318).
[^2]: As shipped: `daemon_socket_path` / `daemon_socket_paths` (`crates/hallouminate-daemon/src/socket.rs:26-48`), `account_home_for_current_user` (`socket.rs:50-71`), path composition (`socket.rs:73-94`). See also [daemon-and-cli](daemon-and-cli.md).
