//! Daemon socket path resolution.
//!
//! The socket lives under the user's runtime/cache directory so multiple
//! daemons can coexist for different users on a shared machine, and so a
//! test process can override the location via `HALLOUMINATE_SOCKET` without
//! poisoning the developer's real daemon.

use std::ffi::OsStr;
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

/// The sibling socket candidate: the path a client with a differently-set
/// `XDG_RUNTIME_DIR` would have resolved instead of ours (#218). A process
/// only sees its own environment, so this recomputes the OTHER candidate
/// from the same two-branch resolution `daemon_socket_path` uses, rather
/// than reading an "other" env var directly. Returns `None` when
/// `HALLOUMINATE_SOCKET` is set (explicit override — honor it exactly, no
/// sibling probing) or when the computed sibling coincides with the
/// primary path.
pub(crate) fn sibling_socket_path() -> Option<PathBuf> {
    compute_sibling_socket_path(
        std::env::var_os("HALLOUMINATE_SOCKET").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        rustix::process::geteuid().as_raw(),
    )
}

/// Pure sibling computation, parameterized so tests can exercise every
/// branch without mutating process env (unsafe on edition 2024, and racy
/// across the parallel test harness — same pattern as
/// `bootstrap::has_explicit_socket_override`).
fn compute_sibling_socket_path(
    explicit_socket: Option<&OsStr>,
    xdg_runtime_dir: Option<&OsStr>,
    euid: u32,
) -> Option<PathBuf> {
    if explicit_socket.is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let cache_candidate =
        PathBuf::from(shellexpand::tilde(FALLBACK_BASE).into_owned()).join(SOCKET_NAME);
    let (primary, sibling) = match xdg_runtime_dir.filter(|v| !v.is_empty()) {
        Some(runtime) => {
            let primary = PathBuf::from(runtime)
                .join("hallouminate")
                .join(SOCKET_NAME);
            (primary, cache_candidate)
        }
        None => {
            // Unset: mirror the conventional systemd path
            // (`/run/user/<euid>`) a session client would have resolved,
            // using our own euid since we cannot read another process's env.
            let runtime = PathBuf::from(format!("/run/user/{euid}"))
                .join("hallouminate")
                .join(SOCKET_NAME);
            (cache_candidate, runtime)
        }
    };
    (sibling != primary).then_some(sibling)
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

    // ── sibling candidate (#218) ──────────────────────────────────────

    #[test]
    fn sibling_is_none_when_explicit_socket_override_set() {
        // HALLOUMINATE_SOCKET is a test/override seam — honor it exactly
        // and never probe a sibling, matching daemon_socket_path's own
        // short-circuit.
        let sibling = compute_sibling_socket_path(
            Some(OsStr::new("/tmp/explicit.sock")),
            Some(OsStr::new("/run/user/1000")),
            1000,
        );
        assert_eq!(sibling, None);
    }

    #[test]
    fn sibling_is_cache_candidate_when_xdg_runtime_dir_set() {
        // A systemd-session client (XDG_RUNTIME_DIR set) resolves the
        // runtime path as primary; its sibling is the cache fallback a
        // detached-shell client without XDG_RUNTIME_DIR would have used.
        let sibling = compute_sibling_socket_path(None, Some(OsStr::new("/run/user/1000")), 1000);
        let expected =
            PathBuf::from(shellexpand::tilde(FALLBACK_BASE).into_owned()).join(SOCKET_NAME);
        assert_eq!(sibling, Some(expected));
    }

    #[test]
    fn sibling_is_conventional_run_user_path_when_xdg_runtime_dir_unset() {
        // A detached-shell client (XDG_RUNTIME_DIR unset) resolves the
        // cache path as primary; its sibling is the conventional
        // `/run/user/<euid>` path a systemd session would have used —
        // computed from our own euid since we can't read the other
        // process's env directly.
        let sibling = compute_sibling_socket_path(None, None, 1000);
        assert_eq!(
            sibling,
            Some(PathBuf::from("/run/user/1000/hallouminate/daemon.sock"))
        );
    }

    #[test]
    fn empty_xdg_runtime_dir_treated_as_unset() {
        // A set-but-blank `XDG_RUNTIME_DIR=` must be filtered out just like
        // `daemon_socket_path`'s own handling, so the sibling falls back to
        // the conventional `/run/user/<euid>` path instead of an empty-string
        // branch.
        let sibling = compute_sibling_socket_path(None, Some(OsStr::new("")), 1000);
        assert_eq!(
            sibling,
            Some(PathBuf::from("/run/user/1000/hallouminate/daemon.sock"))
        );
    }

    #[test]
    fn sibling_coincident_with_primary_collapses_to_none() {
        // When XDG_RUNTIME_DIR's parent equals FALLBACK_BASE's parent, the
        // XDG-set primary and the cache-candidate sibling resolve to the
        // same path — the self-coincidence guard must collapse this to
        // None rather than reporting yourself as your own sibling.
        let cache = PathBuf::from(shellexpand::tilde(FALLBACK_BASE).into_owned());
        let runtime = cache.parent().expect("FALLBACK_BASE has a parent");
        let sibling = compute_sibling_socket_path(None, Some(runtime.as_os_str()), 1000);
        assert_eq!(
            sibling, None,
            "coincident primary/sibling collapses to None"
        );
    }
}
