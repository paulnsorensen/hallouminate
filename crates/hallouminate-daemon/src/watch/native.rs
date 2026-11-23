use std::path::{Path, PathBuf};

use notify::{
    Config, ErrorKind, EventHandler, PathOp, RecommendedWatcher, RecursiveMode, Result,
    UpdatePathsError, WatchPathConfig, Watcher, WatcherKind,
};

pub(super) struct NativeWatcher(RecommendedWatcher);

impl Watcher for NativeWatcher {
    fn new<F: EventHandler>(handler: F, config: Config) -> Result<Self> {
        RecommendedWatcher::new(handler, config).map(Self)
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> Result<()> {
        self.0.watch(path, mode)
    }

    fn watch_with(&mut self, path: &Path, config: WatchPathConfig) -> Result<()> {
        self.0.watch_with(path, config)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.0.unwatch(path)
    }

    fn update_paths(&mut self, ops: Vec<PathOp>) -> std::result::Result<(), UpdatePathsError> {
        let mut result = self.0.update_paths(ops);
        loop {
            let Err(error) = result else {
                return Ok(());
            };
            let (Some(PathOp::Unwatch(_path)), ErrorKind::WatchNotFound) =
                (&error.origin, &error.source.kind)
            else {
                return Err(error);
            };
            // The kernel already removed this watch. Report success so the debouncer forgets it.
            if error.remaining.is_empty() {
                return Ok(());
            }
            result = self.0.update_paths(error.remaining);
        }
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        self.0.configure(config)
    }

    fn watched_paths(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        self.0.watched_paths()
    }

    fn kind() -> WatcherKind {
        RecommendedWatcher::kind()
    }
}
