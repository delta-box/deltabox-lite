#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const targets = {
  "aarch64-apple-darwin": ["deltabox-lite-darwin-arm64", "darwin", "arm64", "libdeltabox_node.dylib"],
  "x86_64-apple-darwin": ["deltabox-lite-darwin-x64", "darwin", "x64", "libdeltabox_node.dylib"],
  "aarch64-unknown-linux-gnu": ["deltabox-lite-linux-arm64", "linux", "arm64", "libdeltabox_node.so"],
  "x86_64-unknown-linux-gnu": ["deltabox-lite-linux-x64", "linux", "x64", "libdeltabox_node.so"],
  "x86_64-pc-windows-msvc": ["deltabox-lite-windows-x64", "win32", "x64", "deltabox_node.dll"],
};

const target = process.argv[2];
const platform = targets[target];
if (!platform) {
  throw new Error(`usage: package-prebuild.mjs <${Object.keys(targets).join("|")}> [output]`);
}

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const output = resolve(process.argv[3] ?? join(root, "dist"));
const manifest = readFileSync(join(root, "Cargo.toml"), "utf8");
const version = manifest.match(/\[workspace\.package\][\s\S]*?version = "([^"]+)"/)?.[1];
if (!version) throw new Error("workspace package version is missing");

const cargo = process.env.CARGO ?? "cargo";
const build = spawnSync(
  cargo,
  [
    "build", "--release", "--locked", "--target", target,
    "-p", "deltabox", "-p", "deltabox-node",
  ],
  { cwd: root, stdio: "inherit" },
);
if (build.status !== 0) process.exit(build.status ?? 1);

const [name, os, cpu, nativeLibrary] = platform;
const executable = os === "win32" ? "deltabox.exe" : "deltabox";
const stage = mkdtempSync(join(tmpdir(), `${name}-`));
try {
  mkdirSync(join(stage, "bin"));
  const binary = join(stage, "bin", executable);
  copyFileSync(join(root, "target", target, "release", executable), binary);
  chmodSync(binary, 0o755);
  mkdirSync(join(stage, "native"));
  copyFileSync(
    join(root, "target", target, "release", nativeLibrary),
    join(stage, "native", "deltabox.node"),
  );
  copyFileSync(join(root, "LICENSE"), join(stage, "LICENSE"));
  writeFileSync(
    join(stage, "README.md"),
    `# ${name}\n\nNative ${os}/${cpu} prebuild for deltabox-lite.\n`,
  );
  writeFileSync(
    join(stage, "package.json"),
    `${JSON.stringify({
      name,
      version,
      description: `Native ${os}/${cpu} prebuild for deltabox-lite`,
      license: "Apache-2.0",
      repository: "https://github.com/delta-box/deltabox-lite",
      os: [os],
      cpu: [cpu],
      files: ["bin", "native", "README.md", "LICENSE"],
      // No `bin` here: the wrapper's launcher (npm/deltabox-lite/bin/deltabox.js)
      // is the one command on PATH, and it finds bin/deltabox through
      // index.cjs. A second bin would race the wrapper's on local installs.
    }, null, 2)}\n`,
  );
  mkdirSync(output, { recursive: true });
  const packed = spawnSync("npm", ["pack", stage, "--pack-destination", output], {
    cwd: root,
    env: { ...process.env, npm_config_cache: join(stage, ".npm-cache") },
    stdio: "inherit",
    shell: os === "win32",
  });
  if (packed.status !== 0) process.exit(packed.status ?? 1);

  // Plain binary tarball for Homebrew and scripts/install.sh: the binary,
  // license, and README at the top level, named by the Rust target triple so
  // installers can map `uname` output onto it without a second lookup table.
  // A sidecar .sha256 in `shasum -c` format lets installers verify the
  // download without a separate manifest.
  const tarballStage = join(stage, "tarball");
  mkdirSync(tarballStage);
  copyFileSync(binary, join(tarballStage, executable));
  chmodSync(join(tarballStage, executable), 0o755);
  copyFileSync(join(root, "LICENSE"), join(tarballStage, "LICENSE"));
  copyFileSync(join(root, "README.md"), join(tarballStage, "README.md"));
  const tarballName = `deltabox-${version}-${target}.tar.gz`;
  const tarball = join(output, tarballName);
  const tarred = spawnSync(
    "tar",
    ["-czf", tarball, "-C", tarballStage, executable, "LICENSE", "README.md"],
    { cwd: root, stdio: "inherit" },
  );
  if (tarred.status !== 0) process.exit(tarred.status ?? 1);
  const digest = createHash("sha256").update(readFileSync(tarball)).digest("hex");
  writeFileSync(join(output, `${tarballName}.sha256`), `${digest}  ${tarballName}\n`);
  console.log(`${tarballName} sha256 ${digest}`);
} finally {
  rmSync(stage, { recursive: true });
}
