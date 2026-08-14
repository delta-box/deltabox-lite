import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DeltaboxClient } from "./index.mjs";

const project = mkdtempSync(join(tmpdir(), "deltabox-client-test-"));
writeFileSync(join(project, "README.md"), "# test project\n");

await assert.rejects(
  DeltaboxClient.start(project, { binary: "/nonexistent/deltabox" }),
  /ENOENT/,
);
console.log("PASS start rejects cleanly when the binary is missing");

const client = await DeltaboxClient.start(project, {
  binary: process.env.DELTABOX_BIN ?? "deltabox",
});

try {
  assert.ok(client.sessionId.length > 0, "session id is set");
  console.log(`PASS session started (${client.info.resolved_fs_mode})`);

  await client.write("notes.txt", "written inside the sandbox\n");
  const back = await client.read("notes.txt");
  assert.equal(back.toString(), "written inside the sandbox\n");
  console.log("PASS write + read round-trip");

  const ex = await client.exec(["sh", "-c", "echo hello-$((1 + 1))"]);
  assert.equal(ex.exit_code, 0);
  assert.equal(ex.stdout.trim(), "hello-2");
  console.log("PASS exec runs a confined command");

  let diff = await client.diff();
  assert.equal(diff.changes.length, 1);
  assert.equal(diff.changes[0].kind, "added");
  console.log("PASS diff sees 1 pending added file");

  await client.abort();
  diff = await client.diff();
  assert.equal(diff.changes.length, 0);
  console.log("PASS abort discards the pending change");

  await client.write("notes.txt", "final version\n");
  await client.commit();
  assert.equal(readFileSync(join(project, "notes.txt"), "utf8"), "final version\n");
  console.log("PASS commit publishes to the real project");

  const status = await client.status();
  assert.equal(status.pending_changes, 0);
  console.log("PASS status reports a clean session");

  await client.stop();
  console.log("PASS session stopped and process closed");
} finally {
  await client.close();
  rmSync(project, { recursive: true, force: true });
  assert.ok(!existsSync(project));
}

console.log("\nall tests passed");
