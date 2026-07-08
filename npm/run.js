#!/usr/bin/env node

"use strict";

// Thin launcher for the downloaded binary. Node has no execve, so we can't
// truly replace this process — instead we spawn the binary, forward the
// termination/job-control signals a caller might send to this wrapper, and
// exit reflecting the child's fate (same code, or re-raise the same signal).
// This matters for `hallouminate serve`: an MCP client that shuts the server
// down by signalling the launcher PID must have that signal reach the binary.
const { spawn } = require("child_process");
const path = require("path");

const bin = path.join(__dirname, "bin", "hallouminate");

const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });

// Registering a listener overrides Node's default (which would kill the
// wrapper and orphan the child); forward to the child and let its exit drive
// ours. SIGKILL/SIGSTOP can't be trapped, so they're intentionally absent.
const SIGNALS = [
  "SIGINT",
  "SIGTERM",
  "SIGHUP",
  "SIGQUIT",
  "SIGUSR1",
  "SIGUSR2",
];
for (const sig of SIGNALS) {
  process.on(sig, () => {
    if (child.exitCode === null && child.signalCode === null) {
      try {
        child.kill(sig);
      } catch {
        /* child already gone */
      }
    }
  });
}

child.on("exit", (code, signal) => {
  if (signal) {
    // Re-raise so our parent observes signal-death, not a synthetic exit code.
    process.kill(process.pid, signal);
  } else {
    process.exit(code === null ? 1 : code);
  }
});

child.on("error", (err) => {
  console.error(`hallouminate: failed to run binary at ${bin}`);
  console.error(err.message);
  process.exit(1);
});
