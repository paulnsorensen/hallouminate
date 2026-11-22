//! Build channel: release distributions versus every other build.
//!
//! A release distribution sets `HALLOUMINATE_RELEASE_CHANNEL` at compile time.
//! Every other build is a dev build. A dev build uses its own data, cache,
//! state, and socket directories, so it never shares a store or a daemon with
//! an installed release.

/// The distribution channel that a binary was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildChannel {
    /// A release distribution: the npm nightly, the stable installers, or
    /// crates.io.
    Release,
    /// Any other build, for example a worktree `cargo build` or a test run.
    Dev,
}

/// The channel of this binary, fixed at compile time.
pub const BUILD_CHANNEL: BuildChannel =
    BuildChannel::from_marker(option_env!("HALLOUMINATE_RELEASE_CHANNEL"));

impl BuildChannel {
    /// Returns [`BuildChannel::Release`] for a non-empty marker, else
    /// [`BuildChannel::Dev`].
    ///
    /// # Examples
    ///
    /// ```
    /// use hallouminate_config::channel::BuildChannel;
    /// assert_eq!(BuildChannel::from_marker(Some("stable")), BuildChannel::Release);
    /// assert_eq!(BuildChannel::from_marker(Some("")), BuildChannel::Dev);
    /// assert_eq!(BuildChannel::from_marker(None), BuildChannel::Dev);
    /// ```
    pub const fn from_marker(marker: Option<&str>) -> Self {
        match marker {
            Some(value) if !value.is_empty() => Self::Release,
            Some(_) => Self::Dev,
            None => Self::Dev,
        }
    }

    /// Returns the application directory name that this channel uses under
    /// each XDG-style base directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use hallouminate_config::channel::BuildChannel;
    /// assert_eq!(BuildChannel::Release.app_dir(), "hallouminate");
    /// assert_eq!(BuildChannel::Dev.app_dir(), "hallouminate-dev");
    /// ```
    pub const fn app_dir(self) -> &'static str {
        match self {
            Self::Release => "hallouminate",
            Self::Dev => "hallouminate-dev",
        }
    }
}
