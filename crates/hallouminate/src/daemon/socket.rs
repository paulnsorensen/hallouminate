//! Daemon socket path resolution.
//!
//! The socket lives under the user's runtime/cache directory so multiple
//! daemons can coexist for different users on a shared machine, and so a
//! test process can override the location via `HALLOUMINATE_SOCKET` without
//! poisoning the developer's real daemon.

use std::path::PathBuf;

const SOCKET_NAME: &str = "daemon.sock";
const FALLBACK_BASE: &str = "~/.cache/hallouminate";

/// User-local Unix socket path shared by the daemon, the CLI client, and
/// the MCP proxy. Resolution order:
///
/// 1. `HALLOUMINATE_SOCKET` (full path) — test override; respected as-is.
/// 2. `XDG_RUNTIME_DIR` joined with `hallouminate/daemon.sock` — the POSIX
///    spec for per-user, per-session runtime data.
/// 3. `~/.cache/hallouminate/daemon.sock` — portable fallback for macOS
///    and other systems without `$XDG_RUNTIME_DIR`.
pub fn daemon_socket_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("HALLOUMINATE_SOCKET")
        && !explicit.is_empty()
    {
        return PathBuf::from(explicit);
    }
    // An explicitly empty `XDG_RUNTIME_DIR=` (set but blank) would otherwise
    // yield the relative `hallouminate/daemon.sock` and bind under the
    // daemon's CWD. Mirror the rest of this codebase's XDG handling and
    // treat empty as absent.
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR")
        && !runtime.is_empty()
    {
        let mut p = PathBuf::from(runtime);
        p.push("hallouminate");
        p.push(SOCKET_NAME);
        return p;
    }
    let base = shellexpand::tilde(FALLBACK_BASE).into_owned();
    PathBuf::from(base).join(SOCKET_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_named_daemon_sock() {
        // Regardless of which fallback runs, the leaf must be `daemon.sock`
        // so the lockfile and cleanup paths stay symmetric.
        let p = daemon_socket_path();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some(SOCKET_NAME));
    }
}
