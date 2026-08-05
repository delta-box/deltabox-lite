#!/usr/bin/env node
// deltabox-lite interactive walkthrough:
// session.start -> checkpoint -> sandboxed edit/exec -> diff -> abort ->
// redo -> checkpoint -> diff -> commit -> session.stop.
// Press Enter to advance between steps (auto-advances when stdin is piped).
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, readdirSync } from "node:fs";
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

const c = (code, s) => `\x1b[${code}m${s}\x1b[0m`;
const clear = () => process.stdout.write("\x1b[2J\x1b[H");
const short = (s, n = 8) => (s ? s.slice(0, n) : "");
const enter = () =>
  new Promise((resolve) => {
    if (!process.stdin.isTTY) return setTimeout(resolve, 700);
    process.stdout.write(c("2", "  press Enter ▸"));
    process.stdin.resume();
    process.stdin.once("data", () => {
      process.stdin.pause();
      resolve();
    });
  });

const step = async (num, title, rpcName, fn) => {
  clear();
  console.log(c("1;36", `▸ ${num} · ${title}`));
  console.log(c("2", `  rpc ${rpcName}`));
  console.log();
  await fn();
  console.log();
};

const showDiff = (diff) => {
  console.log(`  ${c("1;37", `${diff.changes.length} file(s) changed`)}`);
  for (const ch of diff.changes) {
    const mark =
      ch.kind === "added" ? c("1;32", "A") : ch.kind === "modified" ? c("1;33", "M") : c("1;31", "D");
    console.log(`   ${mark} ${ch.path}${ch.is_binary ? " (binary)" : ""}`);
    if (ch.text_diff) {
      for (const line of ch.text_diff.split("\n").slice(0, 12)) {
        if (line.startsWith("+")) console.log(c("32", "      " + line));
        else if (line.startsWith("-")) console.log(c("31", "      " + line));
        else console.log("      " + line);
      }
    }
  }
};

try {
  let sid = "";

  await step(1, "start a sandbox session", "session.start", async () => {
    console.log(c("2", `  params  project: ${project}`));
    const info = await rpc("session.start", { project });
    sid = info.session_id;
    console.log(`  ← session ${c("1;33", short(sid))} … started · pending ${c("1;37", String(info.pending_changes))}`);
  });
  await enter();

  await step(2, "freeze the base state", "checkpoint.create", async () => {
    const ck = await rpc("checkpoint.create", { session_id: sid, label: "base" });
    console.log(`  ← checkpoint ${c("1;33", short(ck.id))} … frozen · ${c("1;37", String(ck.change_count))} change(s)`);
  });
  await enter();

  await step(3, "edit inside the sandbox", "fs.write + session.exec", async () => {
    await rpc("fs.write", {
      session_id: sid,
      path: join(project, "src/hello.py"),
      content_base64: Buffer.from('print("hello from deltabox-lite")\n').toString("base64"),
    });
    console.log(`  ← wrote ${c("32", "src/hello.py")} (host is still untouched)`);
    const ex = await rpc("session.exec", { session_id: sid, argv: ["sh", "-c", "ls -R ."], label: "demo" });
    console.log(`  $ ls -R .   → exit ${c("1;33", String(ex.exit_code))}`);
    for (const line of ex.stdout.trimEnd().split("\n").slice(0, 10)) console.log("    " + line);
  });
  await enter();

  await step(4, "review the pending diff", "diff.list", async () => {
    showDiff(await rpc("diff.list", { session_id: sid }));
  });
  await enter();

  await step(5, "abort — discard the changes", "changes.abort", async () => {
    await rpc("changes.abort", { session_id: sid });
    const diff = await rpc("diff.list", { session_id: sid });
    console.log(`  ← ${c("32", `${diff.changes.length} file(s) changed`)} — sandbox changes discarded`);
    console.log(`  host tree: ${c("37", readdirSync(project).filter((f) => f !== ".deltabox").join(", "))}`);
  });
  await enter();

  await step(6, "redo the work + checkpoint", "fs.write + checkpoint.create", async () => {
    await rpc("fs.write", {
      session_id: sid,
      path: join(project, "src/hello.py"),
      content_base64: Buffer.from('print("hello from deltabox-lite")\n').toString("base64"),
    });
    await rpc("fs.write", {
      session_id: sid,
      path: join(project, "docs/api.md"),
      content_base64: Buffer.from("# API\n").toString("base64"),
    });
    console.log(`  ← wrote ${c("32", "src/hello.py")} · ${c("32", "docs/api.md")}`);
    const ck = await rpc("checkpoint.create", { session_id: sid, label: "second-pass" });
    console.log(`  ← checkpoint ${c("1;33", short(ck.id))} … frozen · ${c("1;37", String(ck.change_count))} change(s)`);
  });
  await enter();

  await step(7, "review once more", "diff.list", async () => {
    showDiff(await rpc("diff.list", { session_id: sid }));
  });
  await enter();

  await step(8, "commit — apply to the real project", "changes.commit", async () => {
    const res = await rpc("changes.commit", { session_id: sid });
    console.log(`  ← committed ${c("1;33", String(res.applied))} change(s) → real project`);
  });
  await enter();

  await step(9, "session done", "session.stop", async () => {
    await rpc("session.stop", { session_id: sid });
    const tree = readdirSync(project, { recursive: true })
      .filter((f) => typeof f === "string" && !f.includes(".deltabox"))
      .sort();
    console.log(`  ← session ${c("1;33", short(sid))} … stopped`);
    console.log(`  host tree: ${c("32", tree.join(" · "))}`);
  });
} finally {
  child.stdin.end();
  await new Promise((r) => setTimeout(r, 300));
  child.kill();
  rmSync(project, { recursive: true, force: true });
}
