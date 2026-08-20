#!/usr/bin/env node

import { spawnSync } from "node:child_process";
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
const stage = mkdtempSync(join(tmpdir(), `${name}-`));
try {
  mkdirSync(join(stage, "bin"));
  const binary = join(stage, "bin", "deltabox");
  copyFileSync(join(root, "target", target, "release", "deltabox"), binary);
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
      repository: "https://github.com/Ddnirvana/deltabox-lite",
      os: [os],
      cpu: [cpu],
      files: ["bin", "native", "README.md", "LICENSE"],
      bin: { deltabox: "bin/deltabox" },
    }, null, 2)}\n`,
  );
  mkdirSync(output, { recursive: true });
  const packed = spawnSync("npm", ["pack", stage, "--pack-destination", output], {
    cwd: root,
    env: { ...process.env, npm_config_cache: join(stage, ".npm-cache") },
    stdio: "inherit",
  });
  if (packed.status !== 0) process.exit(packed.status ?? 1);
} finally {
  rmSync(stage, { recursive: true });
}
