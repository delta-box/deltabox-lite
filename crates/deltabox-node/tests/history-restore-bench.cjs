#!/usr/bin/env node
// Compare old/new packages on identical disposable fixtures, never a user's project.
// node history-restore-bench.cjs <addon.node> <helper> [runs=3] [files=512]
const fs = require('node:fs/promises')
const assert = require('node:assert/strict')
const { tmpdir } = require('node:os')
const { join } = require('node:path')
const [addon, helper, runsArg = '3', filesArg = '512'] = process.argv.slice(2)
const { NativeDeltabox } = require(addon)
const runs = Number(runsArg), count = Number(filesArg)
async function main() {
  const results = []
  for (let run = 0; run < runs; run++) {
    const root = await fs.realpath(await fs.mkdtemp(join(tmpdir(), 'history-restore-bench-')))
    const project = join(root, 'project')
    await fs.mkdir(join(project, 'target'), { recursive: true })
    await fs.writeFile(join(project, '.gitignore'), 'target/\n')
    await fs.writeFile(join(project, 'a.txt'), 'v1')
    for (let i = 0; i < count; i++) await fs.writeFile(join(project, 'target', `${i}.o`), Buffer.alloc(32 * 1024, i % 256))
    await fs.writeFile(join(project, 'target', 'large'), Buffer.alloc(64 * 1024 * 1024, 42))
    const client = new NativeDeltabox(project, helper, { stateDir: join(root, 'state'), runtimeDir: join(root, 'runtime'), fsMode: 'cbcow-through', isolation: 'none' })
    let sid
    try {
      const start = performance.now()
      sid = (await client.startSession()).session_id
      const startupMs = performance.now() - start
      const v1 = await client.historyCapture(sid, 'v1')
      await client.writeFile(sid, join(project, 'a.txt'), Buffer.from('v2'))
      await client.commit(sid)
      const v2 = await client.historyCapture(sid, 'v2')
      const before = (await fs.stat(join(project, 'target', 'large'))).ino
      const restoreStart = performance.now()
      await client.historyRestore(sid, v1.id, v2.id)
      const restoreMs = performance.now() - restoreStart
      assert.equal(await fs.readFile(join(project, 'a.txt'), 'utf8'), 'v1')
      assert.equal((await client.historyDiff(sid, v1.id)).pending_changes, 0)
      const after = (await fs.stat(join(project, 'target', 'large'))).ino
      assert.deepEqual(await fs.readFile(join(project, 'target', 'large')), Buffer.alloc(64 * 1024 * 1024, 42))
      results.push({ run, startupMs: Math.round(startupMs), restoreMs: Math.round(restoreMs), generatedInodePreserved: before === after })
    } finally {
      if (sid) await client.releaseSession(sid)
      await fs.rm(root, { recursive: true, force: true })
    }
  }
  const sorted = results.map(x => x.restoreMs).sort((a, b) => a - b)
  console.log(JSON.stringify({ generatedFiles: count + 1, generatedBytes: count * 32 * 1024 + 64 * 1024 * 1024,
    results, medianRestoreMs: sorted[Math.floor(sorted.length / 2)] }))
}
main().catch(error => { console.error(error); process.exitCode = 1 })
