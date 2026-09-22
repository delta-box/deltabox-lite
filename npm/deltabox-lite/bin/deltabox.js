#!/usr/bin/env node
// Launcher for the `deltabox` command. The real binary ships in the
// platform-specific optional dependency; npm only links the bins that the
// top-level package declares, so a global install of `deltabox-lite` gets
// its command from this shim rather than from the platform package.
//
// The child runs with inherited stdio (so `deltabox serve` can speak
// JSON-RPC over the launcher's own stdin/stdout), termination signals sent to
// the launcher are forwarded to it, and its exit status is mirrored.
"use strict";

const { spawn } = require("node:child_process");
const { constants } = require("node:os");

let binary;
try {
  binary = require("../index.cjs").binaryPath();
} catch (error) {
  process.stderr.write(
    `deltabox-lite: cannot locate the deltabox binary for this platform.\n` +
      `${error.message}\n` +
      `Reinstall with optional dependencies enabled (npm install --include=optional).\n`,
  );
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

// A supervisor that signals only the launcher (rather than the whole
// process group) must still stop the real deltabox, so relay termination
// signals instead of dying and leaving the child orphaned.
const relayed = ["SIGINT", "SIGTERM", "SIGHUP"];
for (const signal of relayed) {
  process.on(signal, () => {
    if (child.exitCode === null && child.signalCode === null) child.kill(signal);
  });
}

child.on("error", (error) => {
  process.stderr.write(`deltabox-lite: failed to launch ${binary}: ${error.message}\n`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    // Mirror the child's termination: raise the same signal on ourselves with
    // the default disposition restored. Signals Node ignores (SIGPIPE) cannot
    // terminate us that way, so fall back to the shell convention 128+n.
    for (const relayedSignal of relayed) process.removeAllListeners(relayedSignal);
    const number = constants.signals[signal];
    process.exitCode = number ? 128 + number : 1;
    try {
      process.kill(process.pid, signal);
    } catch {
      // Unknown signal name: the fallback exit code above applies.
    }
    return;
  }
  process.exit(code ?? 1);
});
