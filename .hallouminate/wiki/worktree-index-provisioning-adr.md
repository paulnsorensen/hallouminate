# Worktree index provisioning — ADRs

This page records accepted decisions for worktree index provisioning and watcher lifecycle.

## Watcher lifecycle recovery

The watcher registry retains declared roots, including absent paths. Reconciliation rebuilds each root from current filesystem state, installs newly available watches, and removes obsolete physical watches after logical registrations change.
