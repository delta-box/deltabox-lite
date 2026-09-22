#!/usr/bin/env node
// Launcher for the `deltabox` command. The real binary ships in the
// platform-specific optional dependency; npm only links the bins that the
// top-level package declares, so a global install of `deltabox-lite` gets
// its command from this shim rather than from the platform package.
"use strict";

const { spawnSync } = require("node:child_process");

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

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  process.stderr.write(`deltabox-lite: failed to launch ${binary}: ${result.error.message}\n`);
  process.exit(1);
}
if (result.signal) {
  // Re-raise the signal so callers see the same termination as the binary.
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
