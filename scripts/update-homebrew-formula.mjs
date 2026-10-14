#!/usr/bin/env node
// Point homebrew/deltabox.rb at a release: rewrite the four tarball URLs
// (which carry the version) and their sha256 checksums. Input is always read
// from homebrew/deltabox.rb; use --out to write the result elsewhere.
//
//   node scripts/update-homebrew-formula.mjs 0.2.0
//       download each tarball's .sha256 sidecar from the GitHub release and
//       overwrite homebrew/deltabox.rb in place
//   node scripts/update-homebrew-formula.mjs 0.2.0 --from-dir dist
//       read the .sha256 sidecars from a local directory (what the release
//       workflow does with the downloaded build artifacts) instead of
//       downloading them
//   ... --base-url file:///path/to/dist
//       also rewrite the download URLs to point somewhere other than the
//       GitHub release, e.g. a local directory of tarballs
//   ... --out <path>
//       write the rewritten formula to <path> instead of overwriting
//       homebrew/deltabox.rb
//   ... --dry-run
//       print the would-be url/sha256 lines and write nothing (neither
//       homebrew/deltabox.rb nor --out, if given)
//
// To try a formula built with a local --base-url: current Homebrew refuses
// to `brew install --formula` a bare local path, so instead drop the output
// into a local tap and install from that:
//   brew tap-new <user>/localtest --no-git
//   cp <out> "$(brew --repository <user>/localtest)/Formula/deltabox.rb"
//   brew install <user>/localtest/deltabox
//
// Checksums come from the `<tarball>.sha256` files that
// scripts/package-prebuild.mjs writes next to each tarball, in
// `shasum -c` format: "<hex>  <filename>".

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const formulaPath = join(root, "homebrew", "deltabox.rb");
const repo = "delta-box/deltabox-lite";
const targets = [
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-gnu",
];

const args = process.argv.slice(2);
let version;
let fromDir;
let baseUrl;
let outPath;
let dryRun = false;
for (let i = 0; i < args.length; i += 1) {
  const arg = args[i];
  if (arg === "--from-dir") fromDir = args[++i];
  else if (arg === "--base-url") baseUrl = args[++i];
  else if (arg === "--out") outPath = args[++i];
  else if (arg === "--dry-run") dryRun = true;
  else if (arg.startsWith("--")) fail(`unknown flag ${arg}`);
  else if (version === undefined) version = arg;
  else fail(`unexpected argument ${arg}`);
}
if (!version) {
  fail(
    "usage: update-homebrew-formula.mjs <version> [--from-dir <dir>] [--base-url <url>] [--out <path>] [--dry-run]",
  );
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) fail(`"${version}" is not a semantic version`);
// Without --base-url the formula points at the GitHub release, and without
// --from-dir the sidecars are fetched from that same location.
baseUrl ??= `https://github.com/${repo}/releases/download/v${version}`;

const checksums = {};
for (const target of targets) {
  const tarball = `deltabox-${version}-${target}.tar.gz`;
  const sidecar = fromDir
    ? readLocal(join(fromDir, `${tarball}.sha256`))
    : await download(`${baseUrl}/${tarball}.sha256`);
  const match = sidecar.match(/^([0-9a-f]{64})\s+\*?(\S+)/);
  if (!match) fail(`${tarball}.sha256 does not look like shasum output`);
  if (match[2] !== tarball) fail(`${tarball}.sha256 names ${match[2]}, expected ${tarball}`);
  checksums[target] = match[1];
}

// The formula has no `version` line: Homebrew derives it from the tarball
// URL, so rewriting the URLs is what moves the formula to the new release.
let formula = readFileSync(formulaPath, "utf8");
const updates = [];
for (const target of targets) {
  const tarball = `deltabox-${version}-${target}.tar.gz`;
  const url = `${baseUrl}/${tarball}`;
  updates.push({ target, url, sha256: checksums[target] });
  // Each target has exactly one url line, and its sha256 is the next
  // sha256 line after it; rewrite them as a pair so they cannot drift.
  const pair = new RegExp(
    `^(\\s*url ")[^"]*-${target.replaceAll(".", "\\.")}\\.tar\\.gz(")\\n(\\s*sha256 ")[0-9a-f]{64}(")`,
    "m",
  );
  formula = replaceOnce(
    formula,
    pair,
    // A replacer FUNCTION, not a string: String.prototype.replace treats a
    // string replacement's `$1`, `$&`, etc. as substitution patterns, so a
    // baseUrl containing a literal `$` would get mangled. A function's
    // return value is always inserted literally.
    (_match, urlOpen, urlClose, shaOpen, shaClose) =>
      `${urlOpen}${url}${urlClose}\n${shaOpen}${checksums[target]}${shaClose}`,
    `url/sha256 pair for ${target}`,
  );
}

if (dryRun) {
  console.log(`(dry run, nothing written)`);
  for (const { target, url, sha256 } of updates) {
    console.log(`  ${target}`);
    console.log(`    url "${url}"`);
    console.log(`    sha256 "${sha256}"`);
  }
} else {
  const destination = outPath ?? formulaPath;
  writeFileSync(destination, formula);
  console.log(`${outPath ?? "homebrew/deltabox.rb"} -> ${version}`);
  for (const { target, sha256 } of updates) console.log(`  ${target}  ${sha256}`);
}

function readLocal(path) {
  try {
    return readFileSync(path, "utf8");
  } catch (error) {
    fail(`cannot read ${path}: ${error.code === "ENOENT" ? "no such file" : error.message}`);
  }
}

async function download(url) {
  const response = await fetch(url);
  if (!response.ok) fail(`GET ${url} failed: ${response.status} ${response.statusText}`);
  return response.text();
}

function replaceOnce(text, pattern, replacement, what) {
  const count = (text.match(new RegExp(pattern.source, `${pattern.flags}g`)) ?? []).length;
  if (count !== 1) fail(`expected exactly one ${what} in homebrew/deltabox.rb, found ${count}`);
  return text.replace(pattern, replacement);
}

function fail(message) {
  console.error(`update-homebrew-formula: ${message}`);
  process.exit(1);
}
