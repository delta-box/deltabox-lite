#!/usr/bin/env node
// deltabox-lite JSON-RPC demo: sandboxed edits -> diff -> abort -> redo -> commit.
//
// Usage:
//   cargo build --release
//   export PATH="$PWD/target/release:$PATH"   # or set DELTABOX_BIN
//   node examples/rpc-demo.mjs
//
// Requires the Linux backend (deltabox serve with the sandlock worker).
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const project = mkdtempSync(join(tmpdir(), "deltabox-demo-"));
writeFileSync(join(project, "README.md"), "# demo\n\nA tiny sandboxed project.\n");
const bin = process.env.DELTABOX_BIN ?? "deltabox";

const child = spawn(bin, ["serve"], { cwd: project, stdio: ["pipe", "pipe", "pipe"] });
let buf = "";
const pending = new Map();
let nextId = 1;
const rpc = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
  });
child.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    const msg = JSON.parse(line);
    const p = pending.get(msg.id);
    if (!p) continue;
    pending.delete(msg.id);
    msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
  }
});
child.stderr.on("data", (d) => process.stderr.write(d));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const c = (code, s) => `\x1b[${code}m${s}\x1b[0m`;
const step = (label, ...lines) => {
  console.log(c("1;36", "▶") + " " + c("1;37", label));
  for (const line of lines) console.log("  " + line);
  console.log();
};
const humanDiff = (diff) => {
  const out = [`${diff.changes.length} file(s) changed`];
  for (const ch of diff.changes) {
    const mark = ch.kind === "added" ? c("1;32", "A") : ch.kind === "modified" ? c("1;33", "M") : c("1;31", "D");
    out.push(` ${mark} ${ch.path}${ch.is_binary ? " (binary)" : ""}`);
    if (ch.text_diff) {
      for (const line of ch.text_diff.split("\n").slice(0, 24)) {
        if (line.startsWith("+")) out.push(c("32", "    " + line));
        else if (line.startsWith("-")) out.push(c("31", "    " + line));
        else out.push("    " + line);
      }
    }
  }
  return out;
};

try {
  step("1 · start a sandbox session", "deltabox serve owns the project session");
  const { session_id: sid } = await rpc("session.start", { project });
  console.log(`   session ${c("1;33", sid.slice(0, 8))} … started`);
  console.log();

  const ck0 = await rpc("checkpoint.create", { session_id: sid, label: "base" });
  console.log(`   checkpoint ${c("1;33", ck0.id.slice(0, 8))} … base state frozen`);

  step("2 · make changes inside the sandbox");
  await rpc("fs.write", { session_id: sid, path: join(project, "src/hello.py"), content_base64: Buffer.from("print(\"hello from deltabox-lite\")\n").toString("base64") });
  console.log(`   wrote ${c("32", "src/hello.py")}`);
  const rm = await rpc("session.exec", { session_id: sid, argv: ["rm", "README.md"], label: "demo" });
  console.log(`   rm README.md → exit ${c("1;33", String(rm.exit_code))}`);
  console.log();

  step("3 · review the pending diff");
  let diff = await rpc("diff.list", { session_id: sid });
  for (const l of humanDiff(diff)) console.log("  " + l);
  console.log();

  step("4 · abort — roll back to the base checkpoint");
  await rpc("changes.abort", { session_id: sid });
  diff = await rpc("diff.list", { session_id: sid });
  console.log(`   ${c("32", diff.changes.length + " file(s) changed")} — sandbox changes discarded`);
  console.log();

  step("5 · redo, review once more, and commit");
  await rpc("fs.write", { session_id: sid, path: join(project, "src/hello.py"), content_base64: Buffer.from("print(\"hello from deltabox-lite\")\n").toString("base64") });
  await rpc("fs.write", { session_id: sid, path: join(project, "docs/api.md"), content_base64: Buffer.from("# API\n").toString("base64") });
  console.log(`   wrote ${c("32", "src/hello.py")} · ${c("32", "docs/api.md")}`);
  const ck2 = await rpc("checkpoint.create", { session_id: sid, label: "second-pass" });
  console.log(`   checkpoint ${c("1;33", ck2.id.slice(0, 8))} … frozen`);
  diff = await rpc("diff.list", { session_id: sid });
  for (const l of humanDiff(diff)) console.log("  " + l);
  const committed = await rpc("changes.commit", { session_id: sid });
  console.log(`   committed ${c("1;33", String(committed.applied))} change(s) → real project`);
  console.log();

  step("6 · session done");
  await rpc("session.stop", { session_id: sid });
  console.log(`   project: ${c("1;37", project)}`);
  console.log(`   tree:   ${c("32", "README.md")} · ${c("32", "src/hello.py")} · ${c("32", "docs/api.md")}`);
} finally {
  child.stdin.end();
  await sleep(300);
  child.kill();
  rmSync(project, { recursive: true, force: true });
}
