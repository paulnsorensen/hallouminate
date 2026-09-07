# Isolated Cargo targets for divergent worktrees

## Decision

Use a separate `CARGO_TARGET_DIR` for each divergent worktree.
Keep compiler gates inside the shared repository verification lease.
The lease serializes processes; it does not prove artifact identity.[^1]

## Evidence

A 2026-09-07 repair run demonstrates incorrect test artifact reuse across these worktrees.
The scheduler checkout contains new scheduler tests, but Cargo completes its test-list command without recompilation.
The selected daemon binary omits those tests and lists a coverage-only regression from another checkout.
The command uses an explicit scheduler working directory and the common target directory.[^2]

A matching verification record alone does not detect this failure.
The exact Cargo fingerprint cause remains unverified.
This observation does not establish a defect in every shared Cargo cache configuration.

An isolated target restores the expected test inventory after workspace-package artifacts are removed.
A dependency-cache clone reduces rebuild time, but it must not retain copied workspace-package artifacts.
Run target cleanup through `just verify cargo clean -p <workspace-package>` for each workspace package.
Run the focused regression with a nonzero selected-test count.
Then run the full `just verify` gate.

## Wiki destination

This note extends `worktree-dev-gotchas.md`.
The wiki mutation gate returns a Hard-debt timeout during this repair.
This tracked note preserves the measured recovery instead of treating a shared-target result as valid evidence.

[^1]: scripts/verify.py::run_leased; AGENTS.md::Local verification
[^2]: Performance review repair, 2026-09-07, base `c382ea26d45cb9458529a835f52037e1f61e5d85`; scheduler test-list command selects a binary containing `ground_coverage_caps_real_scans_and_releases_cancelled_walks` from the coverage checkout.
